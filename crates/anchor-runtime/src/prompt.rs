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
When the task is complete, reply with a short summary and no tool calls.";

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
    let f = |name: &str, desc: &str, params: serde_json::Value| {
        serde_json::json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}})
    };
    let path = serde_json::json!({"type": "string", "description": "Workspace-relative path"});
    serde_json::json!([
        f("read_file", "Read an entire file.", s(serde_json::json!({"path": path}), &["path"])),
        f("write_file", "Create a file, or replace a file you have fully read.",
          s(serde_json::json!({"path": path, "content": {"type": "string"}}), &["path", "content"])),
        f("edit_file", "Edit a file: {\"op\":\"append\",\"text\":...} or {\"op\":\"replace\",\"old\":...,\"new\":...}.",
          s(serde_json::json!({"path": path, "edit": {"type": "object"}}), &["path", "edit"])),
        f("list_dir", "List a directory.", s(serde_json::json!({"path": path}), &["path"])),
        f("glob", "Find files by workspace-relative glob.",
          s(serde_json::json!({"pattern": {"type": "string"}}), &["pattern"])),
        f("grep", "Search file contents with a regex.",
          s(serde_json::json!({"pattern": {"type": "string"}, "path_glob": {"type": "string"}}), &["pattern"])),
        f("run_command", "Run a program (bare name) with an argument list. No shell.",
          s(serde_json::json!({"program": {"type": "string"}, "args": {"type": "array", "items": {"type": "string"}},
                               "network": {"type": "boolean"}}), &["program", "args"])),
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
    pub fn check(&mut self, messages: &[ChatMessage], tools: &serde_json::Value) -> Result<(), PrefixChanged> {
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
