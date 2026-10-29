//! Deterministic Lifecycle Hook Engine (Claude Code & Antigravity `hooks.json` compatible).
//!
//! Runs configured hooks (`PreToolUse`, `PostToolUse`, `SessionStart`, `Stop`)
//! with structured JSON on `stdin` inside the [`anchor_exec`] sandbox.
//! A `PreToolUse` hook can return:
//! * exit code `0` (with optional JSON `{"decision": "allow" | "deny" | "ask", "reason": "..."}`), or
//! * exit code `2` to deterministically block the tool call (`DeniedBy::Hook`).

use std::path::{Path, PathBuf};

use anchor_exec::{CommandSpec, ExitKind, FsConfinement, Limits, Network};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

use crate::customizations::{DiscoveredHook, discover};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Allow,
    Deny { reason: String },
    Ask { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOutcome {
    pub hook_command: String,
    pub event: String,
    pub decision: HookDecision,
    pub stdout: String,
}

fn matches_tool(matcher: &str, tool_name: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    if matcher.eq_ignore_ascii_case(tool_name) {
        return true;
    }
    RegexBuilder::new(&format!("^(?:{matcher})$"))
        .case_insensitive(true)
        .size_limit(64 << 10)
        .build()
        .is_ok_and(|re| re.is_match(tool_name))
}

/// Evaluate all discovered `PreToolUse` hooks matching `tool_name`.
pub fn run_pre_tool_hooks(
    workspace_root: &Path,
    state_dir: &Path,
    path_var: &str,
    exec_read_only: &[PathBuf],
    session_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> Vec<HookOutcome> {
    let inv = discover(workspace_root, state_dir);
    let matching: Vec<&DiscoveredHook> = inv
        .hooks
        .iter()
        .filter(|h| {
            (h.event.eq_ignore_ascii_case("PreToolUse")
                || h.event.eq_ignore_ascii_case("pre_tool_use"))
                && matches_tool(&h.matcher, tool_name)
        })
        .collect();
    if matching.is_empty() {
        return Vec::new();
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "workspace_root": workspace_root,
        "hook_event_name": "PreToolUse",
        "tool_name": tool_name,
        "tool_input": tool_input,
    });
    let stdin_bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut outcomes = Vec::with_capacity(matching.len());
    for h in matching {
        outcomes.push(execute_hook_cmd(
            workspace_root,
            path_var,
            exec_read_only,
            "PreToolUse",
            &h.command,
            &stdin_bytes,
        ));
    }
    outcomes
}

/// Evaluate all discovered `PostToolUse` hooks matching `tool_name`.
pub fn run_post_tool_hooks(
    workspace_root: &Path,
    state_dir: &Path,
    path_var: &str,
    exec_read_only: &[PathBuf],
    session_id: &str,
    tool_name: &str,
    tool_output: &str,
) -> Vec<HookOutcome> {
    let inv = discover(workspace_root, state_dir);
    let matching: Vec<&DiscoveredHook> = inv
        .hooks
        .iter()
        .filter(|h| {
            (h.event.eq_ignore_ascii_case("PostToolUse")
                || h.event.eq_ignore_ascii_case("post_tool_use"))
                && matches_tool(&h.matcher, tool_name)
        })
        .collect();
    if matching.is_empty() {
        return Vec::new();
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "workspace_root": workspace_root,
        "hook_event_name": "PostToolUse",
        "tool_name": tool_name,
        "tool_output": tool_output,
    });
    let stdin_bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut outcomes = Vec::with_capacity(matching.len());
    for h in matching {
        outcomes.push(execute_hook_cmd(
            workspace_root,
            path_var,
            exec_read_only,
            "PostToolUse",
            &h.command,
            &stdin_bytes,
        ));
    }
    outcomes
}

fn execute_hook_cmd(
    workspace_root: &Path,
    path_var: &str,
    exec_read_only: &[PathBuf],
    event: &str,
    command_line: &str,
    stdin_bytes: &[u8],
) -> HookOutcome {
    let parts: Vec<String> = command_line
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let Some((prog_name, args)) = parts.split_first() else {
        return HookOutcome {
            hook_command: command_line.to_string(),
            event: event.to_string(),
            decision: HookDecision::Allow,
            stdout: String::new(),
        };
    };
    let Ok(prog) = anchor_exec::resolve_program(prog_name, path_var) else {
        return HookOutcome {
            hook_command: command_line.to_string(),
            event: event.to_string(),
            decision: HookDecision::Deny {
                reason: format!("hook program {prog_name:?} could not be resolved on PATH"),
            },
            stdout: String::new(),
        };
    };
    let spec = CommandSpec {
        program: prog,
        args: args.to_vec(),
        cwd: workspace_root.to_path_buf(),
        env: vec![
            ("PATH".into(), path_var.to_string()),
            ("HOME".into(), workspace_root.display().to_string()),
        ],
        limits: Limits::default(),
        network: Network::Deny,
        fs: Some(FsConfinement {
            read_write: vec![workspace_root.to_path_buf()],
            read_only: exec_read_only.to_vec(),
        }),
        stdin: stdin_bytes.to_vec(),
    };
    match anchor_exec::run(&spec) {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if matches!(out.exit, ExitKind::Exited(2)) {
                return HookOutcome {
                    hook_command: command_line.to_string(),
                    event: event.to_string(),
                    decision: HookDecision::Deny {
                        reason: if stderr.is_empty() {
                            stdout.clone()
                        } else {
                            stderr
                        },
                    },
                    stdout,
                };
            }
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
                let dec = json
                    .get("permissionDecision")
                    .or_else(|| json.get("decision"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("allow");
                let reason = json
                    .get("reason")
                    .or_else(|| json.get("message"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("blocked by PreToolUse hook")
                    .to_string();
                let decision = match dec {
                    "deny" | "block" => HookDecision::Deny { reason },
                    "ask" | "prompt" => HookDecision::Ask { reason },
                    _ => HookDecision::Allow,
                };
                return HookOutcome {
                    hook_command: command_line.to_string(),
                    event: event.to_string(),
                    decision,
                    stdout,
                };
            }
            HookOutcome {
                hook_command: command_line.to_string(),
                event: event.to_string(),
                decision: HookDecision::Allow,
                stdout,
            }
        }
        Err(e) => HookOutcome {
            hook_command: command_line.to_string(),
            event: event.to_string(),
            decision: HookDecision::Deny {
                reason: format!("hook execution failed: {e}"),
            },
            stdout: String::new(),
        },
    }
}
