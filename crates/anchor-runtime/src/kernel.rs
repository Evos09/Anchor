//! DeepSeek Harness (`dsh` / Cordis) inspired spatiotemporal microkernel.
//!
//! Provides a clean composition model for the runtime without giving Layer 2
//! any filesystem or process authority:
//!
//! * [`TrajectoryLog`] — append-only event trajectory with pure
//!   [`TrajectoryLog::derive_messages`] projection so raw tool outputs,
//!   reasoning traces, and compaction boundaries can be projected into
//!   OpenAI/Anthropic chat messages deterministically.
//! * [`ScopedToolRegistry`] — hierarchical tool scope stack with reversible
//!   [`ScopeEntry`] frames so sub-modes (`Plan`, `CodeMode`, `Review`) can
//!   mount and unmount tools cleanly while preserving prompt prefix stability.
//! * [`HarnessContext`] — unified `ctx` facade (`ctx.trajectory`, `ctx.tools`,
//!   `ctx.memory`) inspired by DeepSeek Harness's `ctx.llm` / `ctx.tools` /
//!   `ctx.sessions` service kernel.

use anchor_protocol::{ChatMessage, ChatRole, TaskState, ToolCall};
use serde::{Deserialize, Serialize};

/// One immutable entry in a session's spatiotemporal trajectory log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrajectoryEvent {
    /// Leading system prompt (kept byte-identical across turns for KV cache hits).
    SystemPrompt { content: String },
    /// User instruction, resumed checkpoint notice, or skill injection.
    UserMessage { content: String },
    /// Assistant reasoning trace (`reasoning_content` / `<think>` block from R1/Thinking).
    ReasoningTrace { turn: u64, text: String },
    /// Assistant completion and tool requests.
    AssistantTurn {
        turn: u64,
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    /// Validated tool execution outcome returned by Layer 1.
    ToolReply {
        turn: u64,
        call_id: String,
        content: String,
    },
    /// Compaction marker replacing earlier turns while keeping checkpoints intact.
    CompactionMarker {
        checkpoint_id: String,
        removed_messages: usize,
    },
}

/// Append-only trajectory log with deterministic `derive_messages()` projection.
#[derive(Debug, Clone, Default)]
pub struct TrajectoryLog {
    events: Vec<TrajectoryEvent>,
}

impl TrajectoryLog {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Seed the trajectory log from an existing slice of [`ChatMessage`]s.
    pub fn from_messages(messages: &[ChatMessage]) -> Self {
        let mut log = Self::new();
        let mut turn = 0u64;
        for m in messages {
            match m.role {
                ChatRole::System => log.record(TrajectoryEvent::SystemPrompt {
                    content: m.content.clone(),
                }),
                ChatRole::User => log.record(TrajectoryEvent::UserMessage {
                    content: m.content.clone(),
                }),
                ChatRole::Assistant => {
                    turn += 1;
                    log.record(TrajectoryEvent::AssistantTurn {
                        turn,
                        content: m.content.clone(),
                        tool_calls: m.tool_calls.clone(),
                    });
                }
                ChatRole::Tool => log.record(TrajectoryEvent::ToolReply {
                    turn,
                    call_id: m.tool_call_id.clone().unwrap_or_default(),
                    content: m.content.clone(),
                }),
            }
        }
        log
    }

    pub fn record(&mut self, event: TrajectoryEvent) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[TrajectoryEvent] {
        &self.events
    }

    /// Pure projection from trajectory events to provider [`ChatMessage`]s
    /// (`dsh` `deriveMessages()` pattern). Reasoning traces stay in the
    /// trajectory for inspection without polluting the prompt prefix.
    pub fn derive_messages(&self) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.events.len());
        for ev in &self.events {
            match ev {
                TrajectoryEvent::SystemPrompt { content } => out.push(ChatMessage {
                    role: ChatRole::System,
                    content: content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                }),
                TrajectoryEvent::UserMessage { content } => out.push(ChatMessage {
                    role: ChatRole::User,
                    content: content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                }),
                TrajectoryEvent::ReasoningTrace { .. } => {
                    // Reasoning traces are emitted to the backend via Message::Reasoning
                    // and excluded from subsequent prompt prefixes to preserve KV cache.
                }
                TrajectoryEvent::AssistantTurn {
                    content,
                    tool_calls,
                    ..
                } => out.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: content.clone(),
                    tool_calls: tool_calls.clone(),
                    tool_call_id: None,
                }),
                TrajectoryEvent::ToolReply {
                    call_id, content, ..
                } => out.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call_id.clone()),
                }),
                TrajectoryEvent::CompactionMarker {
                    checkpoint_id,
                    removed_messages,
                } => out.push(ChatMessage {
                    role: ChatRole::User,
                    content: format!(
                        "[Context compacted: {removed_messages} earlier messages removed. The full history is preserved in checkpoint {checkpoint_id}.]"
                    ),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                }),
            }
        }
        out
    }
}

/// One reversible tool-scope frame mounted onto [`ScopedToolRegistry`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeEntry {
    pub scope_name: String,
    pub tools: Vec<serde_json::Value>,
}

/// Hierarchical tool registry supporting reversible scope mounting (`dsh` plugin pattern).
#[derive(Debug, Clone)]
pub struct ScopedToolRegistry {
    base_tools: serde_json::Value,
    scopes: Vec<ScopeEntry>,
}

impl ScopedToolRegistry {
    pub fn new(base_tools: serde_json::Value) -> Self {
        Self {
            base_tools,
            scopes: Vec::new(),
        }
    }

    /// Mount a named tool scope. Calling [`ScopedToolRegistry::unmount_scope`]
    /// with `scope_name` cleanly restores the previous tool set.
    pub fn mount_scope(&mut self, scope_name: impl Into<String>, tools: Vec<serde_json::Value>) {
        self.scopes.push(ScopeEntry {
            scope_name: scope_name.into(),
            tools,
        });
    }

    /// Unmount a previously mounted scope by name. Returns `true` if a scope was removed.
    pub fn unmount_scope(&mut self, scope_name: &str) -> bool {
        if let Some(pos) = self.scopes.iter().rposition(|s| s.scope_name == scope_name) {
            self.scopes.remove(pos);
            true
        } else {
            false
        }
    }

    pub fn active_scopes(&self) -> &[ScopeEntry] {
        &self.scopes
    }

    /// Snapshot the active OpenAI-compatible tools array. When no extra scopes
    /// are mounted, returns `base_tools` unchanged so [`crate::prompt::PrefixMonitor`]
    /// sees an identical hash.
    pub fn active_tools_json(&self) -> serde_json::Value {
        if self.scopes.is_empty() {
            return self.base_tools.clone();
        }
        let mut arr = self.base_tools.as_array().cloned().unwrap_or_default();
        for scope in &self.scopes {
            arr.extend(scope.tools.iter().cloned());
        }
        serde_json::Value::Array(arr)
    }
}

/// Externalized session memory & task tracker (`dsh` `MEMORY.md` + Claude `TodoWrite`).
#[derive(Debug, Clone, Default)]
pub struct SessionMemory {
    pub tasks: Vec<TaskState>,
    pub active_skills: Vec<String>,
    pub reasoning_steps: usize,
}

/// Unified microkernel context (`ctx`) combining trajectory, scoped tools, and memory.
#[derive(Debug, Clone)]
pub struct HarnessContext {
    pub trajectory: TrajectoryLog,
    pub tools: ScopedToolRegistry,
    pub memory: SessionMemory,
}

impl HarnessContext {
    pub fn new(initial_messages: &[ChatMessage], base_tools: serde_json::Value) -> Self {
        Self {
            trajectory: TrajectoryLog::from_messages(initial_messages),
            tools: ScopedToolRegistry::new(base_tools),
            memory: SessionMemory::default(),
        }
    }
}
