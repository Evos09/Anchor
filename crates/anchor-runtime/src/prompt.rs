//! System prompt, tool definitions, and prompt-prefix stability (spec §9.4).
//!
//! The system prompt and tool definitions are compile-time constants, so the
//! prompt prefix is byte-identical across turns. [`PrefixMonitor`] hashes the
//! prefix every turn and reports any change loudly.

use std::sync::LazyLock;

use anchor_protocol::{ChatMessage, ChatRole};
use sha2::{Digest, Sha256};

pub const SYSTEM_PROMPT: &str = "You are Anchor's coding agent. You work inside a single \
workspace through the tools provided. You cannot access the filesystem or run commands \
directly: every tool call is checked by the user's permission policy and may be refused. \
Read a file in full before rewriting it. Files marked append-only accept only append edits. \
Before finishing, call submit_claims once with what your change does: each claim names the \
files it changed and the tests that exercise it. Claims are checked against the diff, the \
build, the test results and coverage; a claim without a changed file and a test that ran is \
rejected. When the task is complete, reply with a short summary and no tool calls.";

/// The reviewer's system prompt (spec §8.1): verify, do not re-implement.
pub const REVIEWER_PROMPT: &str = "You are Anchor's reviewer. You judge another model's \
change. You receive its claims, the diff, and the evidence the tester captured: build and \
test commands with exit codes and output, per-test results, and line coverage. You have no \
workspace access and must not rely on the executor's description of what it did: validate each \
claim against the diff AND the evidence. A claim is supported only if a diff hunk implements \
it, a test exercises it, and the evidence shows that test passing with the changed lines run. \
Look for behaviour the tests do not check, changed lines that never ran, weakened or vacuous \
tests, and code that could fake test results. Answer by calling submit_verdicts exactly once, \
with one verdict per claim: verdict is supported, refuted or unverifiable; reason_code is a \
short snake_case reason; evidence_refs lists the references (from claim_refs) you relied on.";

/// The executor's claims tool (part of [`TOOLS_JSON`]).
fn submit_claims_tool() -> serde_json::Value {
    let strings = serde_json::json!({"type": "array", "items": {"type": "string"}});
    serde_json::json!({"type": "function", "function": {
        "name": "submit_claims",
        "description": "Record what your change does, before finishing. Each claim names the files that implement it and the test ids (e.g. test_mod.TestCase.test_name) that exercise it. Calling again replaces earlier claims.",
        "parameters": {
            "type": "object",
            "properties": {"claims": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "text": {"type": "string"},
                    "files": strings,
                    "tests": strings,
                },
                "required": ["id", "text", "files", "tests"],
                "additionalProperties": false,
            }}},
            "required": ["claims"],
            "additionalProperties": false,
        }
    }})
}

/// The reviewer's only tool. Serialised once; never rebuilt.
pub static REVIEW_TOOLS_JSON: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!([{"type": "function", "function": {
        "name": "submit_verdicts",
        "description": "Submit one verdict per claim. Call exactly once.",
        "parameters": {
            "type": "object",
            "properties": {"verdicts": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "claim_id": {"type": "string"},
                    "verdict": {"type": "string", "enum": ["supported", "refuted", "unverifiable"]},
                    "reason_code": {"type": "string"},
                    "evidence_refs": {"type": "array", "items": {"type": "string"}},
                    "note": {"type": "string"},
                },
                "required": ["claim_id", "verdict", "reason_code", "evidence_refs", "note"],
                "additionalProperties": false,
            }}},
            "required": ["verdicts"],
            "additionalProperties": false,
        }
    }}])
});

/// OpenAI-compatible tool definitions. Serialised once; never rebuilt.
pub static TOOLS_JSON: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let s = |props: serde_json::Value, required: &[&str]| {
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    };
    let f = |name: &str, desc: &str, params: serde_json::Value| serde_json::json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}});
    let path = serde_json::json!({"type": "string", "description": "Workspace-relative path"});
    serde_json::json!([
        f(
            "read_file",
            "Read an entire file.",
            s(serde_json::json!({"path": path}), &["path"])
        ),
        f(
            "write_file",
            "Create a file, or replace a file you have fully read.",
            s(
                serde_json::json!({"path": path, "content": {"type": "string"}}),
                &["path", "content"]
            )
        ),
        f(
            "edit_file",
            "Edit a file: {\"op\":\"append\",\"text\":...} or {\"op\":\"replace\",\"old\":...,\"new\":...}.",
            s(
                serde_json::json!({"path": path, "edit": {"type": "object"}}),
                &["path", "edit"]
            )
        ),
        f(
            "list_dir",
            "List a directory.",
            s(serde_json::json!({"path": path}), &["path"])
        ),
        f(
            "glob",
            "Find files by workspace-relative glob.",
            s(
                serde_json::json!({"pattern": {"type": "string"}}),
                &["pattern"]
            )
        ),
        f(
            "grep",
            "Search file contents with a regex.",
            s(
                serde_json::json!({"pattern": {"type": "string"}, "path_glob": {"type": "string"}}),
                &["pattern"]
            )
        ),
        f(
            "run_command",
            "Run a program (bare name) with an argument list. No shell.",
            s(
                serde_json::json!({"program": {"type": "string"}, "args": {"type": "array", "items": {"type": "string"}},
                               "network": {"type": "boolean"}}),
                &["program", "args"]
            )
        ),
        submit_claims_tool(),
    ])
});

/// Hash of the cache-relevant prefix: tool definitions + system message.
pub fn prefix_hash(messages: &[ChatMessage], tools: &serde_json::Value) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(tools.to_string().as_bytes());
    h.update([0u8]);
    for m in messages.iter().take_while(|m| m.role == ChatRole::System) {
        h.update(m.content.as_bytes());
        h.update([0u8]);
    }
    h.finalize().into()
}

#[derive(Debug, Default)]
pub struct PrefixMonitor {
    expected: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("prompt prefix changed between turns (cache invalidated)")]
pub struct PrefixChanged;

impl PrefixMonitor {
    pub fn check(
        &mut self,
        messages: &[ChatMessage],
        tools: &serde_json::Value,
    ) -> Result<(), PrefixChanged> {
        let h = prefix_hash(messages, tools);
        match self.expected {
            None => {
                self.expected = Some(h);
                Ok(())
            }
            Some(e) if e == h => Ok(()),
            Some(_) => Err(PrefixChanged),
        }
    }
}
