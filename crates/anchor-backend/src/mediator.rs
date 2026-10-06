//! The tool mediator: every agent tool request passes through here.
//!
//! Pipeline for each request:
//!
//! 1. parameters validated as hostile input (paths via `anchor-fs`,
//!    globs/regexes via `anchor_policy::params`, programs via `anchor-exec`);
//! 2. target resolved to its canonical path;
//! 3. append-only guard / read ledger checked (mutations);
//! 4. structured `PermissionRequest` decided by the engine (prompting with
//!    the canonical path only);
//! 5. the workspace snapshotted, once per turn, before its first mutation
//!    (spec §5.5(a); `anchor-snapshot`);
//! 6. the operation executed on descriptors, consuming the approval;
//! 7. the outcome audited.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anchor_exec::{CommandSpec, FsConfinement, Limits, Network};
use anchor_fs::{Expect, FileContent, FsError, Kind, Resolved, Workspace};
use anchor_guard::{AppendOnlyStatus, Guard, GuardError, WriteBasis};
use anchor_policy::audit::hex;
use anchor_policy::{
    Action, Approved, Decision, Engine, Operation, PermissionRequest, Tool, UserAction, params,
};
use anchor_protocol::{DeniedBy, EditOp, ToolOutcome, ToolRequest};

use crate::interact::{Interaction, PromptView};
use anchor_snapshot::{Change, Id, RestoreReport, SnapshotOptions, SnapshotReport, Store};

/// One snapshot store shared by every mediator of a workspace (main and
/// isolated agent roots). Each whole store operation holds the lock, so
/// snapshots, restores and gc never interleave.
pub type SharedStore = Arc<Mutex<Store>>;

/// Ties a mediator to the Coordination Hub as one agent. The identity is
/// fixed here, by the backend; nothing the runtime sends can change it.
#[derive(Clone)]
pub struct HubBinding {
    pub hub: Arc<anchor_hub::Hub>,
    pub agent: anchor_hub::AgentId,
}

impl std::fmt::Debug for HubBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubBinding")
            .field("agent", &self.agent)
            .finish()
    }
}

fn hub_outcome(e: anchor_hub::tools::ToolError) -> ToolOutcome {
    use anchor_hub::HubError;
    use anchor_hub::tools::ToolError;
    match e {
        ToolError::Hub(
            h @ (HubError::Forbidden(_) | HubError::Conflict(_) | HubError::Unauthenticated),
        ) => denied(DeniedBy::Hub, h.to_string()),
        other => error(other.to_string()),
    }
}

/// Largest tool output returned to the model in one result.
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;
const MAX_SEARCH_FILES: usize = 20_000;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GLOB_RESULTS: usize = 1000;
const MAX_GREP_FILE_BYTES: u64 = 2 << 20;

fn denied(by: DeniedBy, detail: impl Into<String>) -> ToolOutcome {
    ToolOutcome::Denied {
        by,
        detail: detail.into(),
    }
}

fn error(detail: impl Into<String>) -> ToolOutcome {
    ToolOutcome::Error {
        detail: detail.into(),
    }
}

fn fs_outcome(e: FsError) -> ToolOutcome {
    match e {
        FsError::NotFound => error("not found"),
        FsError::NotAFile | FsError::NotADirectory | FsError::TooLarge | FsError::Io(_) => {
            error(e.to_string())
        }
        other => denied(DeniedBy::PathResolution, other.to_string()),
    }
}

fn guard_outcome(e: GuardError) -> ToolOutcome {
    denied(DeniedBy::Guard, e.to_string())
}

/// Counters for instrumentation and the end-of-session report.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub tool_calls: u64,
    pub denied: u64,
    /// File mutations performed through `anchor-fs`.
    pub mutations: u64,
    pub commands: u64,
}

pub struct Mediator {
    ws: Workspace,
    engine: Engine,
    guard: Guard,
    snapshots: SharedStore,
    snap_opts: SnapshotOptions,
    keep_turns: usize,
    hub: Option<HubBinding>,
    session: String,
    /// The turn whose pre-turn snapshot has been taken.
    snapshotted: Option<u64>,
    interact: Box<dyn Interaction>,
    turn: u64,
    path_var: String,
    exec_read_only: Vec<PathBuf>,
    exec_limits: Limits,
    pub stats: Stats,
}

impl std::fmt::Debug for Mediator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mediator")
            .field("root", &self.ws.root())
            .field("turn", &self.turn)
            .field("stats", &self.stats)
            .finish()
    }
}

/// Render the lines a write would delete, for the explicit-diff
/// confirmation of spec §5.3(c).
fn deletion_diff(old: &[u8], new: &[u8]) -> String {
    let old = String::from_utf8_lossy(old);
    let new = String::from_utf8_lossy(new);
    let new_lines: std::collections::HashSet<&str> = new.lines().collect();
    let removed: Vec<&str> = old.lines().filter(|l| !new_lines.contains(l)).collect();
    let mut out = format!(
        "  The agent has not read this file in full this session. This change deletes {} line(s):\n",
        removed.len()
    );
    for l in removed.iter().take(40) {
        out.push_str("  - ");
        out.push_str(l);
        out.push('\n');
    }
    if removed.len() > 40 {
        out.push_str(&format!("  ... and {} more\n", removed.len() - 40));
    }
    out
}

impl Mediator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ws: Workspace,
        engine: Engine,
        guard: Guard,
        snapshots: SharedStore,
        snap_opts: SnapshotOptions,
        keep_turns: usize,
        interact: Box<dyn Interaction>,
        path_var: String,
        exec_read_only: Vec<PathBuf>,
    ) -> Self {
        Mediator {
            ws,
            engine,
            guard,
            snapshots,
            snap_opts,
            keep_turns,
            hub: None,
            session: String::new(),
            snapshotted: None,
            interact,
            turn: 0,
            path_var,
            exec_read_only,
            exec_limits: Limits::default(),
            stats: Stats::default(),
        }
    }

    /// A mediator for an isolated agent root, sharing this one's store and
    /// execution settings.
    pub fn isolated(
        &self,
        ws: Workspace,
        snap_opts: SnapshotOptions,
        engine: Engine,
        guard: Guard,
        interact: Box<dyn Interaction>,
    ) -> Mediator {
        let mut m = Mediator::new(
            ws,
            engine,
            guard,
            self.snapshots.clone(),
            snap_opts,
            self.keep_turns,
            interact,
            self.path_var.clone(),
            self.exec_read_only.clone(),
        );
        m.exec_limits = self.exec_limits.clone();
        m
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
    pub fn interact(&mut self) -> &mut dyn Interaction {
        self.interact.as_mut()
    }
    /// Label future snapshots with this session id.
    pub fn set_session(&mut self, id: &str) {
        self.session = id.to_string();
        self.snapshotted = None;
    }

    /// The snapshot store, locked for the caller's whole operation.
    pub fn snapshots(&self) -> MutexGuard<'_, Store> {
        lock_store(&self.snapshots)
    }

    pub fn shared_store(&self) -> SharedStore {
        self.snapshots.clone()
    }

    pub fn snapshot_options(&self) -> &SnapshotOptions {
        &self.snap_opts
    }

    /// Bind this mediator to the Hub as `binding.agent`: hub tools become
    /// available and file mutations need an edit claim.
    pub fn bind_hub(&mut self, binding: HubBinding) {
        self.hub = Some(binding);
    }

    pub fn hub(&self) -> Option<&HubBinding> {
        self.hub.as_ref()
    }

    pub fn set_turn(&mut self, turn: u64) {
        if turn != self.turn {
            self.snapshotted = None;
        }
        self.turn = turn;
    }
    pub fn set_exec_limits(&mut self, l: Limits) {
        self.exec_limits = l;
    }

    fn request(
        &self,
        tool: Tool,
        op: Operation,
        r: Option<&Resolved>,
        append_only: bool,
    ) -> PermissionRequest {
        PermissionRequest {
            tool,
            operation: op,
            canonical_target: r.map(|r| r.canonical.clone()),
            workspace_root: self.ws.root().to_path_buf(),
            target_exists: r.is_some_and(|r| r.exists()),
            is_append_only: append_only,
            blast_radius_count: 1,
            network: false,
        }
    }

    /// If the policy will deny this request, decide it now (so the denial is
    /// logged) and skip any further user interaction.
    fn deny_early(&mut self, req: &PermissionRequest) -> Option<ToolOutcome> {
        if self.engine.peek(req) == Action::Deny {
            return self.authorize(req.clone(), None).err();
        }
        None
    }

    /// Decide; on `prompt`, ask the user with the canonical target.
    fn authorize(
        &mut self,
        req: PermissionRequest,
        detail: Option<String>,
    ) -> Result<Approved, ToolOutcome> {
        match self.engine.decide(&req) {
            Decision::Allow(a) => Ok(a),
            Decision::Deny(d) => Err(denied(
                DeniedBy::Policy,
                format!("denied by rule {:?} ({:?})", d.rule, d.phase),
            )),
            Decision::Prompt(p) => {
                let view = PromptView {
                    tool: req.tool,
                    operation: req.operation,
                    canonical_target: req.canonical_target.clone(),
                    rule: p.rule().to_string(),
                    detail,
                };
                let resp = self.interact.confirm(&view);
                let responder = self.interact.responder();
                self.engine
                    .resolve_prompt(p, resp, responder)
                    .map_err(|d| denied(DeniedBy::User, format!("denied ({})", d.rule)))
            }
        }
    }

    fn audit_mutation(
        &self,
        approval_id: u64,
        canonical: &Path,
        before: Option<[u8; 32]>,
        after: [u8; 32],
        basis: Option<WriteBasis>,
    ) {
        let _ = self.engine.audit().append(
            "mutation",
            serde_json::json!({
                "decision_id": approval_id,
                "turn": self.turn,
                "canonical_target": canonical,
                "sha256_before": before.map(|h| hex(&h)),
                "sha256_after": hex(&after),
                "guard_basis": basis,
            }),
        );
    }

    /// Handle one tool request. Never panics on hostile input.
    pub fn handle(&mut self, req: ToolRequest) -> ToolOutcome {
        self.stats.tool_calls += 1;
        let out = match req {
            ToolRequest::ReadFile { path } => self.read_file(&path),
            ToolRequest::ListDir { path } => self.list_dir(&path),
            ToolRequest::Glob { pattern } => self.glob(&pattern),
            ToolRequest::Grep { pattern, path_glob } => self.grep(&pattern, path_glob.as_deref()),
            ToolRequest::WriteFile { path, content } => self.write_file(&path, content.as_bytes()),
            ToolRequest::EditFile { path, edit } => self.edit_file(&path, edit),
            ToolRequest::RunCommand {
                program,
                args,
                network,
            } => self.run_command(&program, args, network),
            ToolRequest::Hub { tool, args } => self.hub_tool(&tool, args),
            // Runtime-local: the runtime records these itself and reports
            // them with `Done`/`Verdicts`; there is nothing to execute.
            ToolRequest::SubmitClaims { .. } | ToolRequest::SubmitVerdicts { .. } => {
                ToolOutcome::Error {
                    detail: "runtime-local tool; the backend does not execute it".into(),
                }
            }
        };
        if matches!(out, ToolOutcome::Denied { .. }) {
            self.stats.denied += 1;
        }
        out
    }

    fn read_file(&mut self, path: &str) -> ToolOutcome {
        let r = match self.ws.resolve(path) {
            Ok(r) => r,
            Err(e) => return fs_outcome(e),
        };
        if !r.exists() {
            return error("not found");
        }
        let req = self.request(Tool::ReadFile, Operation::Read, Some(&r), false);
        if let Err(o) = self.authorize(req, None) {
            return o;
        }
        let c = match self.ws.read(&r) {
            Ok(c) => c,
            Err(e) => return fs_outcome(e),
        };
        if c.bytes.len() > MAX_OUTPUT_BYTES {
            // Not recorded as a full read: the model did not see it all.
            return ToolOutcome::Ok {
                output: format!(
                    "{}\n[truncated: file is {} bytes; only the first {MAX_OUTPUT_BYTES} are shown. \
                     This does not count as a full read.]",
                    String::from_utf8_lossy(&c.bytes[..MAX_OUTPUT_BYTES]),
                    c.bytes.len()
                ),
            };
        }
        match String::from_utf8(c.bytes) {
            Ok(text) => {
                self.guard.record_full_read(&r.canonical, c.sha256);
                ToolOutcome::Ok { output: text }
            }
            Err(e) => ToolOutcome::Ok {
                output: format!("[binary file, {} bytes; not shown]", e.into_bytes().len()),
            },
        }
    }

    fn list_dir(&mut self, path: &str) -> ToolOutcome {
        let r = match self.ws.resolve(path) {
            Ok(r) => r,
            Err(e) => return fs_outcome(e),
        };
        let req = self.request(Tool::ListDir, Operation::List, Some(&r), false);
        if let Err(o) = self.authorize(req, None) {
            return o;
        }
        match self.ws.list_dir(&r) {
            Ok(entries) => ToolOutcome::Ok {
                output: entries
                    .iter()
                    .filter(|e| !(r.rel.as_os_str().is_empty() && e.name == ".anchor"))
                    .map(|e| match e.kind {
                        Kind::Dir => format!("{}/", e.name),
                        Kind::File => e.name.clone(),
                        Kind::Symlink | Kind::Other => format!("{} (special)", e.name),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            Err(e) => fs_outcome(e),
        }
    }

    /// Workspace-relative regular files, never following symlinks and never
    /// descending into `.git` or `.anchor`.
    fn walk(&self) -> Vec<PathBuf> {
        let root = self.ws.root().to_path_buf();
        let mut out = Vec::new();
        let walker = ignore::WalkBuilder::new(&root)
            .hidden(false)
            .follow_links(false)
            .git_ignore(true)
            .require_git(false)
            .filter_entry(|e| {
                let n = e.file_name();
                n != ".git" && n != ".anchor"
            })
            .build();
        for ent in walker.flatten() {
            if ent.file_type().is_some_and(|t| t.is_file())
                && let Ok(rel) = ent.path().strip_prefix(&root)
            {
                out.push(rel.to_path_buf());
            }
            if out.len() >= MAX_SEARCH_FILES {
                break;
            }
        }
        out.sort();
        out
    }

    fn search_request(&self, tool: Tool, n: usize) -> PermissionRequest {
        PermissionRequest {
            tool,
            operation: Operation::Search,
            canonical_target: Some(self.ws.root().to_path_buf()),
            workspace_root: self.ws.root().to_path_buf(),
            target_exists: true,
            is_append_only: false,
            blast_radius_count: n.min(u32::MAX as usize) as u32,
            network: false,
        }
    }

    /// Whether the policy would allow reading this file directly.
    fn readable(&self, tool: Tool, canonical: &Path) -> bool {
        let req = PermissionRequest {
            tool,
            operation: Operation::Read,
            canonical_target: Some(canonical.to_path_buf()),
            workspace_root: self.ws.root().to_path_buf(),
            target_exists: true,
            is_append_only: false,
            blast_radius_count: 1,
            network: false,
        };
        self.engine.would_allow(&req)
    }

    fn glob(&mut self, pattern: &str) -> ToolOutcome {
        let m = match params::validate_glob(pattern) {
            Ok(m) => m,
            Err(e) => return denied(DeniedBy::ParameterValidation, e.to_string()),
        };
        let files = self.walk();
        let req = self.search_request(Tool::Glob, files.len());
        if let Err(o) = self.authorize(req, None) {
            return o;
        }
        let mut hits = Vec::new();
        for rel in files {
            if m.is_match(&rel) && self.readable(Tool::Glob, &self.ws.root().join(&rel)) {
                hits.push(rel.to_string_lossy().replace('\\', "/"));
                if hits.len() >= MAX_GLOB_RESULTS {
                    hits.push(format!("[truncated at {MAX_GLOB_RESULTS} results]"));
                    break;
                }
            }
        }
        ToolOutcome::Ok {
            output: hits.join("\n"),
        }
    }

    fn grep(&mut self, pattern: &str, path_glob: Option<&str>) -> ToolOutcome {
        let re = match params::validate_regex(pattern) {
            Ok(r) => r,
            Err(e) => return denied(DeniedBy::ParameterValidation, e.to_string()),
        };
        let filter = match path_glob.map(params::validate_glob).transpose() {
            Ok(f) => f,
            Err(e) => return denied(DeniedBy::ParameterValidation, e.to_string()),
        };
        let files = self.walk();
        let req = self.search_request(Tool::Grep, files.len());
        if let Err(o) = self.authorize(req, None) {
            return o;
        }
        let mut out = Vec::new();
        'files: for rel in files {
            if filter.as_ref().is_some_and(|f| !f.is_match(&rel)) {
                continue;
            }
            // Re-resolve through the descriptor-based path: the walker's
            // view is advisory only.
            let Ok(r) = self.ws.resolve(&rel.to_string_lossy()) else {
                continue;
            };
            if r.meta.is_none_or(|m| m.len > MAX_GREP_FILE_BYTES)
                || !self.readable(Tool::Grep, &r.canonical)
            {
                continue;
            }
            let Ok(FileContent { bytes, .. }) = self.ws.read(&r) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!("{}:{}:{}", rel.display(), i + 1, line));
                    if out.len() >= MAX_GREP_MATCHES {
                        out.push(format!("[truncated at {MAX_GREP_MATCHES} matches]"));
                        break 'files;
                    }
                }
            }
        }
        ToolOutcome::Ok {
            output: out.join("\n"),
        }
    }

    /// Read the current content of a resolved existing target (for guard
    /// checks and pre-images).
    fn current(&self, r: &Resolved) -> Result<Option<FileContent>, ToolOutcome> {
        if !r.exists() {
            return Ok(None);
        }
        if r.is_dir() {
            return Err(error("target is a directory"));
        }
        self.ws.read(r).map(Some).map_err(fs_outcome)
    }

    fn hub_tool(&mut self, tool: &str, args: serde_json::Value) -> ToolOutcome {
        let Some(b) = &self.hub else {
            return denied(DeniedBy::Hub, "this session is not part of a team");
        };
        let caller = anchor_hub::Caller::Agent(b.agent);
        match anchor_hub::tools::call(&b.hub, caller, Some(&self.ws), tool, args) {
            Ok(v) => ToolOutcome::Ok {
                output: v.to_string(),
            },
            Err(e) => hub_outcome(e),
        }
    }

    /// In a team session, a file may change only under an edit claim this
    /// agent holds. A path another agent has claimed is refused; a free
    /// path is claimed on the agent's behalf. (The merge queue re-checks
    /// every changed path; this is the early signal.)
    fn check_claim(&mut self, r: &Resolved) -> Result<(), ToolOutcome> {
        let Some(b) = &self.hub else {
            return Ok(());
        };
        let rel = claim_rel(&r.rel);
        match b.hub.edit_blocker(b.agent, &rel) {
            Ok(Some(c)) => {
                return Err(denied(
                    DeniedBy::Claim,
                    format!(
                        "{rel} is claimed by agent {} ({} claim on {})",
                        c.agent_id, c.claim_type, c.path
                    ),
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(denied(DeniedBy::Claim, e.to_string())),
        }
        match b.hub.holds_edit(b.agent, &rel) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return Err(denied(DeniedBy::Claim, e.to_string())),
        }
        let caller = anchor_hub::Caller::Agent(b.agent);
        match b
            .hub
            .request_claim(caller, &rel, anchor_hub::ClaimType::Edit, false, None)
        {
            Ok(_) => Ok(()),
            Err(e) => Err(denied(
                DeniedBy::Claim,
                format!("could not claim {rel}: {e}"),
            )),
        }
    }

    fn write_file(&mut self, path: &str, content: &[u8]) -> ToolOutcome {
        let r = match self.ws.resolve(path) {
            Ok(r) => r,
            Err(e) => return fs_outcome(e),
        };
        if let Err(o) = self.check_claim(&r) {
            return o;
        }
        let cur = match self.current(&r) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let status = self
            .guard
            .classify(&r.rel, cur.as_ref().map(|c| c.bytes.as_slice()));
        let cur_sha = cur.as_ref().map(|c| c.sha256);
        let op = if r.exists() {
            Operation::Write
        } else {
            Operation::Create
        };
        let req = self.request(Tool::WriteFile, op, Some(&r), status.is_append_only());
        if let Some(o) = self.deny_early(&req) {
            return o;
        }
        let mut detail = None;
        let basis = match self.guard.check_write(&r.canonical, status, cur_sha, false) {
            Ok(b) => b,
            Err(GuardError::NotReadInSession | GuardError::StaleRead) => {
                // Spec §5.3(c): the alternative to a full read is an explicit
                // diff of what will be deleted, confirmed by the user.
                let old = cur.as_ref().map(|c| c.bytes.as_slice()).unwrap_or_default();
                let view = PromptView {
                    tool: Tool::WriteFile,
                    operation: Operation::Write,
                    canonical_target: Some(r.canonical.clone()),
                    rule: "guard:unread_overwrite".into(),
                    detail: Some(deletion_diff(old, content)),
                };
                if self.interact.confirm(&view) != anchor_policy::UserResponse::Approve {
                    return guard_outcome(GuardError::NotReadInSession);
                }
                detail = Some("deletion diff already confirmed".to_string());
                match self.guard.check_write(&r.canonical, status, cur_sha, true) {
                    Ok(b) => b,
                    Err(e) => return guard_outcome(e),
                }
            }
            Err(e) => return guard_outcome(e),
        };
        let approval = match self.authorize(req, detail) {
            Ok(a) => a,
            Err(o) => return o,
        };
        if let Err(e) = self.ensure_turn_snapshot() {
            return error(format!("refusing to mutate: snapshot failed: {e}"));
        }
        let expect = match &cur {
            None => Expect::Absent,
            Some(c) => Expect::Existing {
                id: c.meta.id,
                sha256: c.sha256,
            },
        };
        let id = approval.decision_id();
        match self.ws.write(&r, content, expect, approval) {
            Ok(rcpt) => {
                self.stats.mutations += 1;
                self.guard.record_own_write(&r.canonical, rcpt.new_sha256);
                self.audit_mutation(id, &r.canonical, cur_sha, rcpt.new_sha256, Some(basis));
                ToolOutcome::Ok {
                    output: format!(
                        "wrote {} bytes to {}",
                        rcpt.bytes_written,
                        r.canonical.display()
                    ),
                }
            }
            Err(e) => fs_outcome(e),
        }
    }

    fn edit_file(&mut self, path: &str, edit: EditOp) -> ToolOutcome {
        let r = match self.ws.resolve(path) {
            Ok(r) => r,
            Err(e) => return fs_outcome(e),
        };
        if let Err(o) = self.check_claim(&r) {
            return o;
        }
        let cur = match self.current(&r) {
            Ok(Some(c)) => c,
            Ok(None) => return error("not found (use write_file to create files)"),
            Err(o) => return o,
        };
        let status = self.guard.classify(&r.rel, Some(&cur.bytes));
        let (new, op) = match &edit {
            EditOp::Append { text } => {
                let mut n = cur.bytes.clone();
                n.extend_from_slice(text.as_bytes());
                (n, Operation::Append)
            }
            EditOp::Replace { old, new } => {
                let text = match std::str::from_utf8(&cur.bytes) {
                    Ok(t) => t,
                    Err(_) => return error("file is not UTF-8"),
                };
                match text.matches(old.as_str()).count() {
                    1 => (
                        text.replacen(old.as_str(), new, 1).into_bytes(),
                        Operation::Edit,
                    ),
                    0 => return error("replace.old not found"),
                    n => return error(format!("replace.old matches {n} times; must be unique")),
                }
            }
        };
        let req = self.request(Tool::EditFile, op, Some(&r), status.is_append_only());
        if let Some(o) = self.deny_early(&req) {
            return o;
        }
        let mut detail = None;
        let basis = match self
            .guard
            .check_edit(&r.canonical, status, &cur.bytes, &new, false)
        {
            Ok(b) => b,
            Err(GuardError::NotReadInSession | GuardError::StaleRead)
                if !status.is_append_only() =>
            {
                let view = PromptView {
                    tool: Tool::EditFile,
                    operation: op,
                    canonical_target: Some(r.canonical.clone()),
                    rule: "guard:unread_overwrite".into(),
                    detail: Some(deletion_diff(&cur.bytes, &new)),
                };
                if self.interact.confirm(&view) != anchor_policy::UserResponse::Approve {
                    return guard_outcome(GuardError::NotReadInSession);
                }
                detail = Some("deletion diff already confirmed".to_string());
                match self
                    .guard
                    .check_edit(&r.canonical, status, &cur.bytes, &new, true)
                {
                    Ok(b) => b,
                    Err(e) => return guard_outcome(e),
                }
            }
            Err(e) => return guard_outcome(e),
        };
        let approval = match self.authorize(req, detail) {
            Ok(a) => a,
            Err(o) => return o,
        };
        if let Err(e) = self.ensure_turn_snapshot() {
            return error(format!("refusing to mutate: snapshot failed: {e}"));
        }
        let id = approval.decision_id();
        let result = match &edit {
            EditOp::Append { text } => {
                self.ws
                    .append_verified(&r, text.as_bytes(), cur.sha256, approval)
            }
            EditOp::Replace { .. } => self.ws.write(
                &r,
                &new,
                Expect::Existing {
                    id: cur.meta.id,
                    sha256: cur.sha256,
                },
                approval,
            ),
        };
        match result {
            Ok(rcpt) => {
                self.stats.mutations += 1;
                self.guard.record_own_write(&r.canonical, rcpt.new_sha256);
                self.audit_mutation(
                    id,
                    &r.canonical,
                    Some(cur.sha256),
                    rcpt.new_sha256,
                    Some(basis),
                );
                ToolOutcome::Ok {
                    output: format!(
                        "edited {} ({} bytes)",
                        r.canonical.display(),
                        rcpt.bytes_written
                    ),
                }
            }
            Err(e) => fs_outcome(e),
        }
    }

    fn run_command(&mut self, program: &str, args: Vec<String>, network: bool) -> ToolOutcome {
        let prog = match anchor_exec::resolve_program(program, &self.path_var) {
            Ok(p) => p,
            Err(e) => return denied(DeniedBy::ParameterValidation, e.to_string()),
        };
        let req = PermissionRequest {
            tool: Tool::RunCommand,
            operation: Operation::Execute,
            canonical_target: Some(prog.clone()),
            workspace_root: self.ws.root().to_path_buf(),
            target_exists: true,
            is_append_only: false,
            // A command can touch any file in the workspace.
            blast_radius_count: u32::MAX,
            network,
        };
        let argv = serde_json::to_string(&args).unwrap_or_default();
        let detail = Some(format!(
            "  argv (passed verbatim, no shell): {argv}\n  network: {}",
            if network { "REQUESTED" } else { "denied" }
        ));
        let approval = match self.authorize(req, detail) {
            Ok(a) => a,
            Err(o) => return o,
        };
        drop(approval);
        if let Err(e) = self.ensure_turn_snapshot() {
            return error(format!("refusing to run: workspace snapshot failed: {e}"));
        }
        let spec = CommandSpec {
            program: prog,
            args,
            cwd: self.ws.root().to_path_buf(),
            env: vec![
                ("PATH".into(), self.path_var.clone()),
                ("HOME".into(), self.ws.root().display().to_string()),
                ("LANG".into(), "C.UTF-8".into()),
            ],
            limits: self.exec_limits.clone(),
            network: if network {
                Network::Allow
            } else {
                Network::Deny
            },
            fs: Some(FsConfinement {
                read_write: vec![self.ws.root().to_path_buf()],
                read_only: self.exec_read_only.clone(),
            }),
        };
        match anchor_exec::run(&spec) {
            Ok(o) => {
                self.stats.commands += 1;
                let _ = self.engine.audit().append(
                    "command",
                    serde_json::json!({
                        "turn": self.turn,
                        "program": spec.program,
                        "args": spec.args,
                        "exit": format!("{:?}", o.exit),
                        "network_isolated": o.isolation.network_isolated(),
                        "network_method": o.isolation.network.map(|m| format!("{m:?}")),
                        "fs_confined": o.isolation.fs_confined,
                    }),
                );
                ToolOutcome::Ok {
                    output: format!(
                        "exit: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}{}{}",
                        o.exit,
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr),
                        if o.truncated {
                            "\n[output truncated]"
                        } else {
                            ""
                        },
                        if o.isolation.dropped_root {
                            "\n[note: the backend runs as root; the command ran as uid 65534 so resource limits apply]"
                        } else {
                            ""
                        }
                    ),
                }
            }
            Err(anchor_exec::ExecError::SandboxUnavailable(m)) => {
                let _ = self.engine.audit().append(
                    "command_refused",
                    serde_json::json!({
                        "turn": self.turn,
                        "program": spec.program,
                        "reason": "sandbox_unavailable",
                        "detail": m,
                    }),
                );
                denied(DeniedBy::Sandbox, m)
            }
            Err(e) => error(e.to_string()),
        }
    }

    // ---------------------------------------------------------------------
    // Rollback (user-initiated)
    // ---------------------------------------------------------------------

    /// Snapshot the workspace before the first mutation of the current
    /// turn (spec §5.5(a): every mutating turn is snapshotted first). The
    /// ref is numbered by the backend, never by the runtime; the session
    /// and turn go in the label.
    fn ensure_turn_snapshot(&mut self) -> Result<(), String> {
        if self.snapshotted == Some(self.turn) {
            return Ok(());
        }
        let store = lock_store(&self.snapshots);
        let root = self.snap_opts.root.clone();
        let turns = root.list_refs(&store, "turns").map_err(|e| e.to_string())?;
        let seq = turns
            .last()
            .and_then(|(n, _)| n.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        let label = format!("session {} turn {}", self.session, self.turn);
        let r = anchor_snapshot::take_snapshot(
            &store,
            &self.ws,
            &self.snap_opts,
            &root.ref_name("turns", &format!("{seq:010}")),
            &label,
        )
        .map_err(|e| e.to_string())?;
        self.audit_snapshot(&label, &r);
        self.snapshotted = Some(self.turn);
        if !root.is_main() && turns.len() >= self.keep_turns {
            // An agent root prunes its own turns; gc only knows main's.
            for (leaf, _) in &turns[..turns.len() + 1 - self.keep_turns.max(1)] {
                let _ = store.delete_ref(&root.ref_name("turns", leaf));
            }
        }
        if seq as usize > self.keep_turns + 20 {
            // Amortised: prune old turns and unreachable objects.
            match anchor_snapshot::gc(&store, self.keep_turns, 20) {
                Ok(g) => {
                    let _ = self.engine.audit().append(
                        "snapshot_gc",
                        serde_json::json!({
                            "refs_pruned": g.refs_pruned,
                            "objects_removed": g.objects_removed,
                            "bytes_freed": g.bytes_freed,
                        }),
                    );
                }
                Err(e) => self.interact.notify(&format!("snapshot gc skipped: {e}")),
            }
        }
        Ok(())
    }

    fn audit_snapshot(&self, label: &str, r: &SnapshotReport) {
        let _ = self.engine.audit().append(
            "snapshot",
            serde_json::json!({
                "label": label,
                "id": r.id().hex(),
                "files": r.files,
                "bytes": r.bytes,
                "hashed_files": r.hashed_files,
                "new_objects": r.new_objects,
                "new_bytes": r.new_bytes,
                "untracked": r.untracked.iter().map(|(p, u)| format!("{p} ({u:?})")).collect::<Vec<_>>(),
                "ms": r.total_ms,
            }),
        );
    }

    /// The snapshot taken before `turn` of the most recent session that
    /// has snapshots.
    pub fn snapshot_for_turn(&self, turn: u64) -> Result<Id, String> {
        let store = lock_store(&self.snapshots);
        let refs = self
            .snap_opts
            .root
            .list_refs(&store, "turns")
            .map_err(|e| e.to_string())?;
        let mut latest_session: Option<String> = None;
        for (_, id) in refs.iter().rev() {
            let label = store.snapshot(id).map_err(|e| e.to_string())?.label;
            let Some((session, t)) = parse_label(&label) else {
                continue;
            };
            let s = latest_session.get_or_insert_with(|| session.to_string());
            if s != session {
                break;
            }
            if t == turn {
                return Ok(*id);
            }
        }
        Err(format!(
            "no snapshot before turn {turn} in the latest session (turns that made no changes have none)"
        ))
    }

    /// What restoring `target` would change.
    pub fn preview_restore(&mut self, target: &Id) -> Result<Vec<Change>, String> {
        let store = lock_store(&self.snapshots);
        let now = anchor_snapshot::snapshot(&store, &self.ws, &self.snap_opts, "preview")
            .map_err(|e| e.to_string())?;
        anchor_snapshot::diff(&store, &now.id(), target).map_err(|e| e.to_string())
    }

    /// Restore the workspace to its state before `turn` (latest session).
    pub fn rollback_to(&mut self, turn: u64) -> Result<Vec<String>, String> {
        let id = self.snapshot_for_turn(turn)?;
        self.rollback_to_snapshot(&id)
    }

    /// Restore the workspace to snapshot `id`, atomically and verified
    /// (user-initiated; every operation is approved, audited and executed
    /// through `anchor-fs`).
    pub fn rollback_to_snapshot(&mut self, id: &Id) -> Result<Vec<String>, String> {
        let root = self.ws.root().to_path_buf();
        let engine = &self.engine;
        let mut auth = |r: &Resolved, op: Operation| {
            engine
                .user_initiated(UserAction::Rollback, &user_request(&root, r, op))
                .map_err(|d| d.rule)
        };
        let store = lock_store(&self.snapshots);
        let result = anchor_snapshot::restore(&store, &self.ws, &self.snap_opts, id, &mut auth);
        drop(store);
        self.snapshotted = None;
        match result {
            Ok(r) => {
                self.audit_restore("rollback", &r);
                let mut log = r.ops.clone();
                log.extend(
                    r.skipped
                        .iter()
                        .map(|p| format!("left alone (untracked): {p}")),
                );
                Ok(log)
            }
            Err(e) => {
                let _ = self.engine.audit().append(
                    "rollback_failed",
                    serde_json::json!({ "target": id.hex(), "error": e.to_string() }),
                );
                Err(e.to_string())
            }
        }
    }

    /// Finish an interrupted restore by returning to its pre-restore state.
    pub fn recover(&mut self) -> Result<Option<RestoreReport>, String> {
        let root = self.ws.root().to_path_buf();
        let engine = &self.engine;
        let mut auth = |r: &Resolved, op: Operation| {
            engine
                .user_initiated(UserAction::Rollback, &user_request(&root, r, op))
                .map_err(|d| d.rule)
        };
        let store = lock_store(&self.snapshots);
        let r = anchor_snapshot::recover(&store, &self.ws, &self.snap_opts, &mut auth)
            .map_err(|e| e.to_string())?;
        drop(store);
        if let Some(r) = &r {
            self.audit_restore("restore_recovered", r);
        }
        Ok(r)
    }

    fn audit_restore(&self, kind: &str, r: &RestoreReport) {
        let _ = self.engine.audit().append(
            kind,
            serde_json::json!({
                "pre": r.pre.map(|i| i.hex()),
                "target": r.target.map(|i| i.hex()),
                "post": r.post.map(|i| i.hex()),
                "operations": r.ops.len(),
                "skipped": r.skipped,
            }),
        );
    }

    /// User action: persist a "not append-only" override.
    pub fn mark_not_append_only(&mut self, rel: &Path) -> Result<AppendOnlyStatus, String> {
        self.guard
            .mark_not_append_only(rel)
            .map_err(|e| e.to_string())?;
        let _ = self.engine.audit().append(
            "user_override",
            serde_json::json!({ "action": "not_append_only", "path": rel }),
        );
        Ok(self.guard.classify(rel, None))
    }
}

/// Lock a shared store. A panic elsewhere while holding the lock leaves
/// the store itself consistent (every operation is crash-safe on disk), so
/// a poisoned lock is still usable.
pub fn lock_store(s: &SharedStore) -> MutexGuard<'_, Store> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

/// A workspace-relative path in the Hub's claim form (`/`-separated).
fn claim_rel(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `session <id> turn <n>` → (id, n).
fn parse_label(label: &str) -> Option<(&str, u64)> {
    let rest = label.strip_prefix("session ")?;
    let (session, turn) = rest.rsplit_once(" turn ")?;
    Some((session, turn.parse().ok()?))
}

fn user_request(root: &Path, r: &Resolved, op: Operation) -> PermissionRequest {
    PermissionRequest {
        tool: Tool::UserAction,
        operation: op,
        canonical_target: Some(r.canonical.clone()),
        workspace_root: root.to_path_buf(),
        target_exists: r.exists(),
        is_append_only: false,
        blast_radius_count: 1,
        network: false,
    }
}
