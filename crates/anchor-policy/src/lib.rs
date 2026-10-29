//! Structured permission engine (spec §5.2).
//!
//! The engine decides from a [`PermissionRequest`] and nothing else. The
//! request type has no free-text field: the engine is structurally unable to
//! read the user's prompt, the model's reasoning, or any natural language.
//!
//! Evaluation order (ADR-0011):
//!
//! 1. **Capability floor** — fixed rules the policy file cannot override
//!    (e.g. Anchor's own state directory is never agent-writable).
//! 2. **Deny overrides** — if *any* `deny` rule matches, the request is denied.
//! 3. **First match** — otherwise the first matching rule in file order wins.
//! 4. **Default** — otherwise the `[default]` action applies.
//!
//! This is total: every request receives exactly one action.
//!
//! An `allow` decision (or a user-confirmed `prompt`) mints an [`Approved`]
//! token. Mutating filesystem operations in `anchor-fs` require one, and its
//! constructor is private to this crate, so no mutation can bypass the engine.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub mod audit;
pub mod params;
mod policy;

pub use audit::{AuditEntry, AuditError, AuditLog, Tailer};
pub use policy::{Action, Policy, PolicyError, Scope};

/// Tools known to the engine. Mirrors the protocol's closed tool set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    ReadFile,
    WriteFile,
    EditFile,
    MultiEdit,
    ApplyPatch,
    ListDir,
    Glob,
    Grep,
    RunCommand,
    WebFetch,
    WebSearch,
    TodoWrite,
    AskQuestion,
    WriteArtifact,
    InvokeSubagent,
    CodeModeBatch,
    ActivateSkill,
    /// Actions taken by the backend on the user's explicit instruction
    /// (rollback, config overrides, workbench edits). Never requested by the agent.
    UserAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    List,
    Search,
    Create,
    Write,
    Append,
    Edit,
    Delete,
    Execute,
}

impl Operation {
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Operation::Create
                | Operation::Write
                | Operation::Append
                | Operation::Edit
                | Operation::Delete
        )
    }
}

/// The only input the engine ever sees.
///
/// Every field is an enum, a canonical path produced by `anchor-fs`, a
/// boolean, or a count. There is deliberately no `String` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermissionRequest {
    pub tool: Tool,
    pub operation: Operation,
    /// Fully resolved absolute path of the target (for `run_command`, the
    /// resolved program). `None` only for requests without a target.
    pub canonical_target: Option<PathBuf>,
    pub workspace_root: PathBuf,
    pub target_exists: bool,
    pub is_append_only: bool,
    /// How many files the operation can touch.
    pub blast_radius_count: u32,
    /// Whether the operation requests network egress.
    pub network: bool,
}

/// Proof that the engine (or the user, via a prompt) approved one specific
/// operation on one specific canonical target. Not `Clone`: one approval
/// authorises one mutation.
#[derive(Debug)]
#[must_use = "an approval must be consumed by the operation it authorises"]
pub struct Approved {
    decision_id: u64,
    tool: Tool,
    operation: Operation,
    canonical_target: Option<PathBuf>,
    _seal: Seal,
}

#[derive(Debug)]
struct Seal;

static APPROVALS_MINTED: AtomicU64 = AtomicU64::new(0);

/// Total approvals minted by this process (instrumentation).
pub fn approvals_minted() -> u64 {
    APPROVALS_MINTED.load(Ordering::SeqCst)
}

impl Approved {
    fn mint(decision_id: u64, req: &PermissionRequest) -> Self {
        APPROVALS_MINTED.fetch_add(1, Ordering::SeqCst);
        Approved {
            decision_id,
            tool: req.tool,
            operation: req.operation,
            canonical_target: req.canonical_target.clone(),
            _seal: Seal,
        }
    }
    pub fn decision_id(&self) -> u64 {
        self.decision_id
    }
    pub fn tool(&self) -> Tool {
        self.tool
    }
    pub fn operation(&self) -> Operation {
        self.operation
    }
    pub fn target(&self) -> Option<&Path> {
        self.canonical_target.as_deref()
    }
}

/// A decision that needs the user. Display [`PendingPrompt::request`]'s
/// canonical target — never anything model-supplied — then call
/// [`Engine::resolve_prompt`].
#[derive(Debug)]
pub struct PendingPrompt {
    decision_id: u64,
    request: PermissionRequest,
    rule: String,
}

impl PendingPrompt {
    pub fn request(&self) -> &PermissionRequest {
        &self.request
    }
    pub fn rule(&self) -> &str {
        &self.rule
    }
    pub fn decision_id(&self) -> u64 {
        self.decision_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Denial {
    pub decision_id: u64,
    pub rule: String,
    pub phase: Phase,
}

#[derive(Debug)]
pub enum Decision {
    Allow(Approved),
    Prompt(PendingPrompt),
    Deny(Denial),
}

/// Which evaluation phase produced the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    CapabilityFloor,
    DenyOverride,
    FirstMatch,
    Default,
    /// The audit log could not be written; fail closed.
    AuditFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserResponse {
    Approve,
    Deny,
}

/// Who answered a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Responder {
    Interactive,
    /// Non-interactive responder configured explicitly on the command line.
    Automatic,
}

/// User actions that bypass agent policy but are still audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAction {
    Rollback,
    MarkNotAppendOnly,
    /// Integrate an agent's isolated work into the main workspace.
    Merge,
    /// Undo one merge (spec §7.3).
    RevertMerge,
    /// Direct file edit from the desktop Code Workbench.
    WorkbenchEdit,
}

#[derive(Debug)]
struct PolicyState {
    policy: Policy,
    source_hash: Option<String>,
    last_failed_hash: Option<String>,
}

/// The permission engine.
#[derive(Debug)]
pub struct Engine {
    policy_path: Option<PathBuf>,
    home: PathBuf,
    state: Mutex<PolicyState>,
    audit: Arc<AuditLog>,
    /// Shared by every engine made with [`Engine::for_agent`], so decision
    /// ids stay unique within one process's share of the chain.
    next_decision: Arc<AtomicU64>,
    /// The agent this engine decides for (multi-agent runs); recorded in
    /// every audit entry it writes.
    agent: Option<String>,
    /// Extra floor-protected directories (the backend's state directory,
    /// which lives outside the workspace). Canonical paths.
    protected: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Audit(#[from] AuditError),
    #[error("i/o error reading policy {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Engine {
    /// Create an engine from a policy file. The file is re-read on every
    /// decision; a change takes effect on the next request (spec §5.2(g)).
    pub fn from_file(
        policy_path: &Path,
        home: &Path,
        audit: AuditLog,
    ) -> Result<Self, EngineError> {
        let bytes = std::fs::read(policy_path).map_err(|source| EngineError::Io {
            path: policy_path.to_path_buf(),
            source,
        })?;
        let text = String::from_utf8(bytes).map_err(|_| PolicyError::NotUtf8)?;
        let policy = Policy::parse(&text, home)?;
        let hash = audit::sha256_hex(text.as_bytes());
        let engine = Engine {
            policy_path: Some(policy_path.to_path_buf()),
            home: home.to_path_buf(),
            state: Mutex::new(PolicyState {
                policy,
                source_hash: Some(hash.clone()),
                last_failed_hash: None,
            }),
            audit: Arc::new(audit),
            next_decision: Arc::new(AtomicU64::new(1)),
            agent: None,
            protected: Vec::new(),
        };
        engine.audit.append(
            "policy_loaded",
            serde_json::json!({ "path": policy_path, "sha256": hash }),
        )?;
        Ok(engine)
    }

    /// Create an engine from an in-memory policy (no hot reload).
    pub fn from_policy(policy: Policy, home: &Path, audit: AuditLog) -> Self {
        Engine {
            policy_path: None,
            home: home.to_path_buf(),
            state: Mutex::new(PolicyState {
                policy,
                source_hash: None,
                last_failed_hash: None,
            }),
            audit: Arc::new(audit),
            next_decision: Arc::new(AtomicU64::new(1)),
            agent: None,
            protected: Vec::new(),
        }
    }

    /// An engine for one agent of a multi-agent run. It shares this engine's
    /// policy source, audit chain, decision counter and protected
    /// directories, and tags its audit entries with `agent`.
    pub fn for_agent(&self, agent: &str) -> Engine {
        let state = match self.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        Engine {
            policy_path: self.policy_path.clone(),
            home: self.home.clone(),
            state: Mutex::new(PolicyState {
                policy: state.policy.clone(),
                source_hash: state.source_hash.clone(),
                last_failed_hash: state.last_failed_hash.clone(),
            }),
            audit: Arc::clone(&self.audit),
            next_decision: Arc::clone(&self.next_decision),
            agent: Some(agent.to_string()),
            protected: self.protected.clone(),
        }
    }

    pub fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    /// The shared audit log handle.
    pub fn audit_arc(&self) -> Arc<AuditLog> {
        Arc::clone(&self.audit)
    }

    /// Append to the audit log, adding the engine's agent (if any) to an
    /// object payload.
    pub fn log(&self, kind: &str, mut data: serde_json::Value) -> Result<u64, AuditError> {
        if let (Some(a), Some(obj)) = (&self.agent, data.as_object_mut()) {
            obj.insert("agent".into(), serde_json::Value::String(a.clone()));
        }
        self.audit.append(kind, data)
    }

    /// Add a directory that no request may target, for any tool (including
    /// user-initiated actions). `dir` must be canonical.
    pub fn protect(&mut self, dir: PathBuf) {
        if !self.protected.contains(&dir) {
            self.protected.push(dir);
        }
    }

    fn protected_floor(&self, req: &PermissionRequest) -> Option<&'static str> {
        let t = req.canonical_target.as_deref()?;
        self.protected
            .iter()
            .any(|d| t.starts_with(d))
            .then_some("capability:anchor_state_dir")
    }

    fn eval(&self, policy: &Policy, req: &PermissionRequest) -> (Action, String, Phase) {
        match self.protected_floor(req) {
            Some(rule) => (Action::Deny, rule.to_string(), Phase::CapabilityFloor),
            None => evaluate(policy, req),
        }
    }

    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Re-read the policy file if it changed. An invalid or unreadable file
    /// leaves the last good policy in force and is logged once.
    fn refresh(&self, state: &mut PolicyState) {
        let Some(path) = &self.policy_path else {
            return;
        };
        let text = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                let marker = format!("io:{e}");
                if state.last_failed_hash.as_deref() != Some(&marker) {
                    let _ = self.audit.append(
                        "policy_reload_failed",
                        serde_json::json!({ "path": path, "error": e.to_string(), "kept": "last_good" }),
                    );
                    state.last_failed_hash = Some(marker);
                }
                return;
            }
        };
        let hash = audit::sha256_hex(&text);
        if state.source_hash.as_deref() == Some(&hash)
            || state.last_failed_hash.as_deref() == Some(&hash)
        {
            return;
        }
        let parsed = std::str::from_utf8(&text)
            .map_err(|_| PolicyError::NotUtf8)
            .and_then(|t| Policy::parse(t, &self.home));
        match parsed {
            Ok(p) => {
                let _ = self.audit.append(
                    "policy_reloaded",
                    serde_json::json!({
                        "path": path,
                        "old_sha256": state.source_hash,
                        "new_sha256": hash,
                    }),
                );
                state.policy = p;
                state.source_hash = Some(hash);
                state.last_failed_hash = None;
            }
            Err(e) => {
                let _ = self.audit.append(
                    "policy_reload_failed",
                    serde_json::json!({ "path": path, "sha256": hash, "error": e.to_string(), "kept": "last_good" }),
                );
                state.last_failed_hash = Some(hash);
            }
        }
    }

    /// Decide a request. Every call is logged; if logging fails the request
    /// is denied.
    pub fn decide(&self, req: &PermissionRequest) -> Decision {
        let decision_id = self.next_decision.fetch_add(1, Ordering::SeqCst);
        let (action, rule, phase) = {
            let mut state = match self.state.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            self.refresh(&mut state);
            self.eval(&state.policy, req)
        };
        let logged = self.log(
            "decision",
            serde_json::json!({
                "decision_id": decision_id,
                "request": req,
                "matched_rule": rule,
                "phase": phase,
                "action": action,
            }),
        );
        if logged.is_err() {
            return Decision::Deny(Denial {
                decision_id,
                rule: "audit_log_unavailable".into(),
                phase: Phase::AuditFailure,
            });
        }
        match action {
            Action::Allow => Decision::Allow(Approved::mint(decision_id, req)),
            Action::Prompt => Decision::Prompt(PendingPrompt {
                decision_id,
                request: req.clone(),
                rule,
            }),
            Action::Deny => Decision::Deny(Denial {
                decision_id,
                rule,
                phase,
            }),
        }
    }

    /// Evaluate without minting an approval or logging. Used to filter
    /// candidate files inside an already-decided search (the search itself is
    /// decided and logged by [`Engine::decide`]). Only `Allow` counts.
    pub fn would_allow(&self, req: &PermissionRequest) -> bool {
        let state = match self.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        matches!(self.eval(&state.policy, req).0, Action::Allow)
    }

    /// The action the policy would take, without minting or logging. Used to
    /// skip user interaction (e.g. a diff confirmation) for requests that
    /// will be denied anyway; the denial itself still goes through
    /// [`Engine::decide`] and is logged.
    pub fn peek(&self, req: &PermissionRequest) -> Action {
        let state = match self.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        self.eval(&state.policy, req).0
    }

    /// Record the user's answer to a prompt.
    pub fn resolve_prompt(
        &self,
        prompt: PendingPrompt,
        response: UserResponse,
        responder: Responder,
    ) -> Result<Approved, Denial> {
        let logged = self.log(
            "prompt_response",
            serde_json::json!({
                "decision_id": prompt.decision_id,
                "canonical_target": prompt.request.canonical_target,
                "matched_rule": prompt.rule,
                "response": response,
                "responder": responder,
            }),
        );
        match (response, logged) {
            (UserResponse::Approve, Ok(_)) => {
                Ok(Approved::mint(prompt.decision_id, &prompt.request))
            }
            (UserResponse::Approve, Err(_)) => Err(Denial {
                decision_id: prompt.decision_id,
                rule: "audit_log_unavailable".into(),
                phase: Phase::AuditFailure,
            }),
            (UserResponse::Deny, _) => Err(Denial {
                decision_id: prompt.decision_id,
                rule: "user_denied".into(),
                phase: Phase::FirstMatch,
            }),
        }
    }

    /// Approve an action the *user* initiated (rollback, overrides). The
    /// capability floor still applies; agent policy does not.
    pub fn user_initiated(
        &self,
        action: UserAction,
        req: &PermissionRequest,
    ) -> Result<Approved, Denial> {
        let decision_id = self.next_decision.fetch_add(1, Ordering::SeqCst);
        let floor = capability_floor(req).or_else(|| self.protected_floor(req));
        let logged = self.log(
            "user_initiated",
            serde_json::json!({
                "decision_id": decision_id,
                "action": action,
                "request": req,
                "floor": floor,
            }),
        );
        if logged.is_err() {
            return Err(Denial {
                decision_id,
                rule: "audit_log_unavailable".into(),
                phase: Phase::AuditFailure,
            });
        }
        if let Some(rule) = floor {
            return Err(Denial {
                decision_id,
                rule: rule.into(),
                phase: Phase::CapabilityFloor,
            });
        }
        Ok(Approved::mint(decision_id, req))
    }
}

/// Fixed rules below every policy (spec §5.2(c): capability is the floor).
fn capability_floor(req: &PermissionRequest) -> Option<&'static str> {
    if let Some(t) = &req.canonical_target {
        if !t.is_absolute() {
            return Some("capability:non_canonical_target");
        }
        let state_dir = req.workspace_root.join(".anchor");
        if t.starts_with(&state_dir) && req.tool != Tool::UserAction {
            return Some("capability:anchor_state_dir");
        }
    } else if req.operation.is_mutation() {
        return Some("capability:mutation_without_target");
    }
    if !req.workspace_root.is_absolute() {
        return Some("capability:non_canonical_workspace");
    }
    None
}

/// Pure evaluation: (action, rule name, phase).
pub fn evaluate(policy: &Policy, req: &PermissionRequest) -> (Action, String, Phase) {
    if let Some(rule) = capability_floor(req) {
        return (Action::Deny, rule.to_string(), Phase::CapabilityFloor);
    }
    if let Some(r) = policy
        .rules()
        .iter()
        .find(|r| r.action == Action::Deny && r.matches(req))
    {
        return (Action::Deny, r.name.clone(), Phase::DenyOverride);
    }
    if let Some(r) = policy.rules().iter().find(|r| r.matches(req)) {
        return (r.action, r.name.clone(), Phase::FirstMatch);
    }
    (policy.default_action(), "default".into(), Phase::Default)
}

#[cfg(test)]
mod tests;
