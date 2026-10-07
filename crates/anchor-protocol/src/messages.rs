use serde::{Deserialize, Serialize};

use crate::verify::{Claim, ClaimVerdict, ReviewInput, validate_claims, validate_verdicts};
use crate::{MAX_CONTENT_BYTES, Validate, check_param};

/// Every message that may appear on the wire, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Message {
    // ---- runtime -> backend -------------------------------------------
    /// First frame from the runtime.
    Hello(Hello),
    /// Request a tool execution. The backend answers with `ToolResult`.
    ToolCall(ToolCall),
    /// Context-window usage after a model response.
    ContextUsage(ContextUsage),
    /// Token and cost accounting for one model call.
    Usage(Usage),
    /// A checkpoint the backend must persist (turn boundary or pause).
    Checkpoint(Box<Checkpoint>),
    /// The runtime has paused and waits for a `Resume`.
    Paused(Paused),
    /// Something the user must be told (fallback, downgrade, ...).
    Notice(Notice),
    /// Turn bookkeeping.
    TurnStarted {
        turn: u64,
    },
    TurnFinished {
        turn: u64,
    },
    /// A reviewer's per-claim verdicts (review sessions only), sent just
    /// before `Done`.
    Verdicts(Verdicts),
    /// Final answer for the task.
    Done(Done),
    /// Fatal runtime error.
    Failed(Failed),

    // ---- backend -> runtime -------------------------------------------
    /// Answer to `Hello`.
    Welcome(Welcome),
    /// The task to run, plus all configuration the runtime needs (the
    /// runtime has no filesystem access, so it cannot read config itself).
    Task(Box<Task>),
    ToolResult(ToolResult),
    CheckpointSaved(CheckpointSaved),
    Resume(Resume),
    Shutdown,
}

impl Validate for Message {
    fn validate(&self) -> Result<(), String> {
        match self {
            Message::Hello(h) => check_param("runtime_version", &h.runtime_version),
            Message::ToolCall(c) => c.validate(),
            Message::ContextUsage(u) => {
                check_param("model_id", &u.model_id)?;
                if u.window_tokens == 0 {
                    return Err("window_tokens must be > 0".into());
                }
                Ok(())
            }
            Message::Usage(u) => {
                check_param("role", &u.role)?;
                check_param("model_id", &u.model_id)?;
                if !u.cost_usd.is_finite() || u.cost_usd < 0.0 {
                    return Err("cost_usd must be finite and >= 0".into());
                }
                Ok(())
            }
            Message::Checkpoint(c) => c.validate(),
            Message::Paused(p) => check_param("checkpoint_id", &p.checkpoint_id),
            Message::Notice(n) => check_param("detail", &n.detail),
            Message::TurnStarted { .. } | Message::TurnFinished { .. } => Ok(()),
            Message::Done(d) => {
                if d.summary.len() > MAX_CONTENT_BYTES {
                    return Err("summary too large".into());
                }
                validate_claims(&d.claims)
            }
            Message::Verdicts(v) => validate_verdicts(&v.verdicts),
            Message::Failed(f) => check_param("error", &f.error),
            Message::Welcome(w) => check_param("session_id", &w.session_id),
            Message::Task(t) => t.validate(),
            Message::ToolResult(r) => check_call_id(&r.call_id),
            Message::CheckpointSaved(c) => check_param("checkpoint_id", &c.checkpoint_id),
            Message::Resume(_) | Message::Shutdown => Ok(()),
        }
    }
}

fn check_call_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("call_id must be 1..=128 printable ASCII bytes".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub runtime_version: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Welcome {
    pub session_id: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub call_id: String,
    pub request: ToolRequest,
}

impl Validate for ToolCall {
    fn validate(&self) -> Result<(), String> {
        check_call_id(&self.call_id)?;
        self.request.validate()
    }
}

/// The closed set of tools the runtime may request. Every parameter is
/// treated by the backend as hostile input (spec §5.2(e)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "tool",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ToolRequest {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    EditFile {
        path: String,
        edit: EditOp,
    },
    ListDir {
        path: String,
    },
    Glob {
        pattern: String,
    },
    Grep {
        pattern: String,
        #[serde(default)]
        path_glob: Option<String>,
    },
    RunCommand {
        program: String,
        args: Vec<String>,
        #[serde(default)]
        network: bool,
    },
    /// A Coordination Hub tool (spec §6): claims, messages, tasks,
    /// decisions. Only offered when [`Task::hub`] is set. The backend
    /// executes it as the agent bound to this runtime; the arguments can
    /// never name another caller.
    Hub {
        tool: String,
        args: serde_json::Value,
    },
    /// The executor's claims about its change (spec §8.1). Handled inside
    /// the runtime and reported with `Done`; never executed by the backend.
    SubmitClaims {
        claims: Vec<Claim>,
    },
    /// A reviewer's verdicts (spec §8.1). Runtime-local, like
    /// `SubmitClaims`.
    SubmitVerdicts {
        verdicts: Vec<ClaimVerdict>,
    },
}

/// Largest serialized hub tool argument object.
pub const MAX_HUB_ARGS_BYTES: usize = 64 << 10;

/// `hub_` followed by 1-40 lowercase letters or underscores.
pub fn valid_hub_tool(name: &str) -> bool {
    name.strip_prefix("hub_").is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 40
            && rest.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    })
}

impl ToolRequest {
    pub fn name(&self) -> &'static str {
        match self {
            ToolRequest::ReadFile { .. } => "read_file",
            ToolRequest::WriteFile { .. } => "write_file",
            ToolRequest::EditFile { .. } => "edit_file",
            ToolRequest::ListDir { .. } => "list_dir",
            ToolRequest::Glob { .. } => "glob",
            ToolRequest::Grep { .. } => "grep",
            ToolRequest::RunCommand { .. } => "run_command",
            ToolRequest::Hub { .. } => "hub",
            ToolRequest::SubmitClaims { .. } => "submit_claims",
            ToolRequest::SubmitVerdicts { .. } => "submit_verdicts",
        }
    }

    /// Whether this tool can mutate the filesystem. Decided by the tool,
    /// never by the model's description of what it will do (spec §5.5(a)).
    pub fn can_mutate(&self) -> bool {
        matches!(
            self,
            ToolRequest::WriteFile { .. }
                | ToolRequest::EditFile { .. }
                | ToolRequest::RunCommand { .. }
        )
    }
}

impl Validate for ToolRequest {
    fn validate(&self) -> Result<(), String> {
        match self {
            ToolRequest::ReadFile { path } | ToolRequest::ListDir { path } => {
                crate::check_name("path", path)
            }
            ToolRequest::WriteFile { path, content } => {
                crate::check_name("path", path)?;
                if content.len() > MAX_CONTENT_BYTES {
                    return Err("content too large".into());
                }
                Ok(())
            }
            ToolRequest::EditFile { path, edit } => {
                crate::check_name("path", path)?;
                edit.validate()
            }
            ToolRequest::Glob { pattern } => crate::check_name("pattern", pattern),
            ToolRequest::Grep { pattern, path_glob } => {
                crate::check_name("pattern", pattern)?;
                if let Some(g) = path_glob {
                    crate::check_name("path_glob", g)?;
                }
                Ok(())
            }
            ToolRequest::RunCommand { program, args, .. } => {
                crate::check_name("program", program)?;
                if args.len() > 256 {
                    return Err("too many args".into());
                }
                for a in args {
                    if a.len() > crate::MAX_PARAM_BYTES || a.contains('\0') {
                        return Err("argument too long or contains NUL".into());
                    }
                }
                Ok(())
            }
            ToolRequest::Hub { tool, args } => {
                if !valid_hub_tool(tool) {
                    return Err("invalid hub tool name".into());
                }
                if !(args.is_object() || args.is_null()) {
                    return Err("hub tool arguments must be an object".into());
                }
                if args.to_string().len() > MAX_HUB_ARGS_BYTES {
                    return Err("hub tool arguments too large".into());
                }
                Ok(())
            }
            ToolRequest::SubmitClaims { claims } => validate_claims(claims),
            ToolRequest::SubmitVerdicts { verdicts } => validate_verdicts(verdicts),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditOp {
    /// Pure suffix append. The only edit allowed on append-only files.
    Append { text: String },
    /// Replace exactly one occurrence of `old` with `new`.
    Replace { old: String, new: String },
}

impl Validate for EditOp {
    fn validate(&self) -> Result<(), String> {
        match self {
            EditOp::Append { text } => {
                if text.is_empty() || text.len() > MAX_CONTENT_BYTES {
                    return Err("append text empty or too large".into());
                }
            }
            EditOp::Replace { old, new } => {
                if old.is_empty() {
                    return Err("replace.old is empty".into());
                }
                if old.len() + new.len() > MAX_CONTENT_BYTES {
                    return Err("replace too large".into());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call_id: String,
    pub outcome: ToolOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolOutcome {
    Ok {
        output: String,
    },
    /// Refused by the permission engine, the guard, or the user.
    Denied {
        by: DeniedBy,
        detail: String,
    },
    Error {
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeniedBy {
    Policy,
    User,
    Guard,
    PathResolution,
    ParameterValidation,
    Sandbox,
    /// Another agent holds a conflicting claim, or the path is outside the
    /// agent's claimable paths (spec §6.4).
    Claim,
    /// The Coordination Hub refused the request (permissions, identity).
    Hub,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUsage {
    pub model_id: String,
    pub used_tokens: u64,
    pub window_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub role: String,
    pub model_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paused {
    pub reason: PauseReason,
    pub checkpoint_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    ContextThreshold,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    pub choice: ResumeChoice,
}

/// The three options of spec §5.4(d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeChoice {
    /// Continue; the runtime compacts its context. The checkpoint stays.
    ContinueWithCompaction,
    /// Stop this session; the user starts a fresh one seeded from the
    /// checkpoint.
    FreshSession,
    /// Stop; the user rolls back to an earlier snapshot.
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub kind: NoticeKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    ProviderFallback,
    PromptPrefixChanged,
    RoutingDecision,
    SandboxStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Done {
    pub summary: String,
    /// The executor's claims (empty for sessions that make none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<Claim>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdicts {
    pub verdicts: Vec<ClaimVerdict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failed {
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointSaved {
    pub checkpoint_id: String,
}

// ---------------------------------------------------------------------------
// Task and configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub session_id: String,
    pub instruction: String,
    /// The roles configuration, as TOML text read by the backend.
    pub roles_toml: String,
    /// Fraction of the context window at which the runtime must pause.
    pub checkpoint_threshold: f64,
    /// Seed the session from this checkpoint instead of starting empty.
    #[serde(default)]
    pub resume_from: Option<Box<Checkpoint>>,
    /// Scripted responses for the mock provider (tests and demos only).
    #[serde(default)]
    pub mock_script: Option<String>,
    /// Set when this agent is part of a multi-agent run (Phase 4): who it
    /// is on the Hub, its peers, and the hub tools it may call.
    #[serde(default)]
    pub hub: Option<HubInfo>,
    /// Set for a review session (Phase 5): the runtime routes to the review
    /// role, gets no workspace tools, and must answer with verdicts.
    #[serde(default)]
    pub review: Option<Box<ReviewInput>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubInfo {
    pub agent_id: i64,
    pub display_name: String,
    pub peers: Vec<HubPeer>,
    pub tools: Vec<HubToolDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubPeer {
    pub agent_id: i64,
    pub display_name: String,
    pub role: String,
    pub family: String,
}

/// A hub tool as offered to the model (definitions come from the backend;
/// the runtime cannot link the Hub).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl Validate for HubInfo {
    fn validate(&self) -> Result<(), String> {
        check_param("display_name", &self.display_name)?;
        if self.peers.len() > 64 || self.tools.len() > 64 {
            return Err("too many hub peers or tools".into());
        }
        for p in &self.peers {
            check_param("peer name", &p.display_name)?;
        }
        for t in &self.tools {
            if !valid_hub_tool(&t.name) || t.description.len() > 4096 {
                return Err(format!("invalid hub tool definition {:?}", t.name));
            }
        }
        Ok(())
    }
}

impl Validate for Task {
    fn validate(&self) -> Result<(), String> {
        check_param("session_id", &self.session_id)?;
        if self.instruction.is_empty() || self.instruction.len() > MAX_CONTENT_BYTES / 4 {
            return Err("instruction empty or too large".into());
        }
        if !(self.checkpoint_threshold > 0.0 && self.checkpoint_threshold <= 1.0) {
            return Err("checkpoint_threshold must be in (0, 1]".into());
        }
        if let Some(c) = &self.resume_from {
            c.validate()?;
        }
        if let Some(h) = &self.hub {
            h.validate()?;
        }
        if let Some(r) = &self.review {
            if self.hub.is_some() || self.resume_from.is_some() {
                return Err("a review session has no hub identity and no checkpoint".into());
            }
            r.validate()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Chat and checkpoint data (spec §5.4(c))
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: ChatRole,
    #[serde(default)]
    pub content: String,
    /// Tool calls requested by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `role = tool`: the call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskState {
    pub id: String,
    pub description: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Review,
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileClaimState {
    pub path: String,
    pub claim_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingDecision {
    pub id: String,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    TurnBoundary,
    ContextThreshold,
}

/// Authoritative record of a session's state. The `summary` is auxiliary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub format_version: u32,
    pub checkpoint_id: String,
    pub session_id: String,
    pub reason: CheckpointReason,
    pub turn: u64,
    pub model_id: String,
    pub context_used_tokens: u64,
    pub context_window_tokens: u64,
    pub messages: Vec<ChatMessage>,
    pub tasks: Vec<TaskState>,
    pub file_claims: Vec<FileClaimState>,
    pub pending_decisions: Vec<PendingDecision>,
    #[serde(default)]
    pub summary: Option<String>,
}

pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

impl Validate for Checkpoint {
    fn validate(&self) -> Result<(), String> {
        if self.format_version != CHECKPOINT_FORMAT_VERSION {
            return Err(format!(
                "unsupported checkpoint format_version {}",
                self.format_version
            ));
        }
        let id = &self.checkpoint_id;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("checkpoint_id must be 1..=128 chars of [A-Za-z0-9_-]".into());
        }
        check_param("session_id", &self.session_id)?;
        check_param("model_id", &self.model_id)?;
        if self.context_window_tokens == 0 {
            return Err("context_window_tokens must be > 0".into());
        }
        Ok(())
    }
}
