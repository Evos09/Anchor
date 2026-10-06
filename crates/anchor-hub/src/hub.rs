//! [`Hub`]: the RPC surface of spec §6.5, transport-independent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anchor_policy::AuditLog;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::db::{self, *};
use crate::model::*;
use crate::path::{ClaimPath, stored};
use crate::{HubError, Result};

/// Milliseconds since the Unix epoch. Injectable so lease and liveness
/// tests are deterministic.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock tests move by hand.
#[derive(Debug)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start_ms: u64) -> ManualClock {
        ManualClock(AtomicU64::new(start_ms))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Hub settings (`[hub]` in `workspace.toml`).
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// Default leases per claim type (ADR-0012).
    pub ttl_read_ms: u64,
    pub ttl_edit_ms: u64,
    pub ttl_review_ms: u64,
    /// Upper bound on any requested lease.
    pub max_ttl_ms: u64,
    /// An agent silent for two intervals is marked lost.
    pub heartbeat_ms: u64,
    /// Message types held for the user's approval, from every agent.
    pub approve_message_types: Vec<MessageType>,
    /// Largest message content (serialized JSON).
    pub max_content_bytes: usize,
}

impl Default for HubConfig {
    fn default() -> Self {
        HubConfig {
            ttl_read_ms: 30 * 60_000,
            ttl_edit_ms: 5 * 60_000,
            ttl_review_ms: 15 * 60_000,
            max_ttl_ms: 2 * 3_600_000,
            heartbeat_ms: 60_000,
            approve_message_types: Vec::new(),
            max_content_bytes: 64 << 10,
        }
    }
}

/// A workspace file whose hash the Hub computed itself. Only
/// [`VerifiedFile::read`] makes one, so an artifact's recorded hash is never
/// the caller's claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    path: String,
    sha256: String,
}

impl VerifiedFile {
    /// Read `rel` through `anchor-fs` (no symlink escapes, no hardlinks)
    /// and hash it.
    pub fn read(ws: &anchor_fs::Workspace, rel: &str) -> Result<VerifiedFile> {
        let path = ClaimPath::file(rel).map_err(HubError::Invalid)?.to_string();
        let r = ws.resolve(&path)?;
        let content = ws.read(&r)?;
        Ok(VerifiedFile {
            path,
            sha256: anchor_policy::audit::hex(&content.sha256),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubVerifyReport {
    pub rows: usize,
    pub hub_entries: usize,
    pub errors: Vec<String>,
}

impl HubVerifyReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug)]
pub struct Hub {
    conn: Mutex<Connection>,
    audit: Arc<AuditLog>,
    clock: Arc<dyn Clock>,
    cfg: HubConfig,
    project_id: i64,
    workspace: PathBuf,
    /// Evidence files (`<state>/evidence`, next to the database).
    evidence_dir: PathBuf,
}

/// Largest evidence file the Hub stores.
pub const MAX_EVIDENCE_BYTES: usize = 8 << 20;

/// Evidence names are `<group>/<file>`, each `[A-Za-z0-9._-]`, no dot-only
/// components.
fn valid_evidence_name(n: &str) -> bool {
    let parts: Vec<&str> = n.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 128
                && !p.chars().all(|c| c == '.')
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

/// Write `bytes` to `path` exactly once: an existing file must already hold
/// the same bytes. Files are owner-read-only.
fn write_once(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(path) {
        return if existing == bytes {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "evidence already recorded with different content",
            ))
        };
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name().and_then(|f| f.to_str()).unwrap_or("e")
    ));
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o400);
        }
        let mut f = o.open(&tmp)?;
        std::io::Write::write_all(&mut f, bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// One call's transaction plus the rows it changed.
struct Tx<'c> {
    tx: Transaction<'c>,
    touched: Vec<(&'static str, i64)>,
    now: u64,
}

impl Tx<'_> {
    fn touch(&mut self, table: &'static str, id: i64) {
        if !self.touched.contains(&(table, id)) {
            self.touched.push((table, id));
        }
    }
}

const MAX_TEXT: usize = 8000;

fn text(field: &str, s: &str, max: usize) -> Result<()> {
    if s.trim().is_empty() || s.len() > max || s.contains('\0') {
        return Err(HubError::Invalid(format!(
            "{field} must be 1-{max} bytes without NUL"
        )));
    }
    Ok(())
}

fn valid_name(field: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(HubError::Invalid(format!(
            "{field} must be 1-40 characters of [A-Za-z0-9_-]"
        )))
    }
}

/// Whether a new claim of type `a` conflicts with an active claim of type
/// `b` on an overlapping path (spec §6.4; an edit and a review are kept
/// apart in both directions so a review never covers a moving target).
fn conflicts(a: ClaimType, a_iso: bool, b: ClaimType, b_iso: bool) -> bool {
    use ClaimType::*;
    matches!((a, b), (Edit, Edit) | (Review, Edit) | (Edit, Review))
        || (a == Edit && b == Read && b_iso)
        || (a == Read && a_iso && b == Edit)
}

fn token_hash(token: &str) -> String {
    anchor_policy::audit::sha256_hex(token.as_bytes())
}

impl Hub {
    /// Open (creating if needed) the Hub database for `workspace`.
    pub fn open(
        db_path: &Path,
        workspace: &Path,
        audit: Arc<AuditLog>,
        clock: Arc<dyn Clock>,
        cfg: HubConfig,
    ) -> Result<Hub> {
        let conn = db::open(db_path)?;
        let mut hub = Hub {
            conn: Mutex::new(conn),
            audit,
            clock,
            cfg,
            project_id: 0,
            workspace: workspace.to_path_buf(),
            evidence_dir: db_path.parent().unwrap_or(Path::new(".")).join("evidence"),
        };
        let existing: Option<(i64, String)> = hub
            .lock()
            .query_row("SELECT id, workspace_path FROM project LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        hub.project_id = match existing {
            Some((id, path)) => {
                if Path::new(&path) != workspace {
                    return Err(HubError::Invalid(format!(
                        "hub database belongs to {path}, not {}",
                        workspace.display()
                    )));
                }
                id
            }
            None => {
                let ws = workspace.to_string_lossy().into_owned();
                hub.mutate(
                    Caller::System,
                    "open_project",
                    json!({ "workspace": ws }),
                    |t| {
                        t.tx.execute(
                            "INSERT INTO project (workspace_path, created_at) VALUES (?1, ?2)",
                            params![ws, t.now as i64],
                        )?;
                        let id = t.tx.last_insert_rowid();
                        t.touch("project", id);
                        Ok(id)
                    },
                )?
            }
        };
        Ok(hub)
    }

    pub fn project_id(&self) -> i64 {
        self.project_id
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn config(&self) -> &HubConfig {
        &self.cfg
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Append a Hub event that changes no row (e.g. a refused
    /// authentication at the transport). Failures are ignored: the
    /// request is refused either way.
    pub fn audit_event(&self, kind: &str, data: Value) {
        let _ = self.audit.append(kind, data);
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // -- transaction + audit plumbing ---------------------------------------

    /// Run one Hub call: sweep expired leases and lost agents, check the
    /// caller, run `f` in a transaction, then audit and commit. A refused
    /// call rolls back and is audited as `hub_denied`.
    fn mutate<T>(
        &self,
        caller: Caller,
        method: &str,
        args: Value,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock();
        self.sweep(&mut conn)?;
        let now = self.clock.now_ms();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut t = Tx {
            tx,
            touched: Vec::new(),
            now,
        };
        let result = self.check_caller(&mut t, caller).and_then(|()| f(&mut t));
        match result {
            Ok(v) => {
                self.commit(t, caller, method, args)?;
                Ok(v)
            }
            Err(e) => {
                drop(t);
                self.audit.append(
                    "hub_denied",
                    json!({
                        "caller": caller,
                        "method": method,
                        "args": args,
                        "error": e.to_string(),
                        "code": e.code(),
                    }),
                )?;
                Err(e)
            }
        }
    }

    fn commit(&self, t: Tx<'_>, caller: Caller, method: &str, args: Value) -> Result<()> {
        let mut rows = Vec::new();
        for (table, id) in &t.touched {
            let state = row_state(&t.tx, table, *id)?
                .ok_or_else(|| HubError::NotFound(format!("{table} {id}")))?;
            rows.push(json!({ "table": table, "id": id, "digest": row_digest(&state) }));
        }
        let (seq, hash) = self.audit.append_hashed(
            &format!("hub_{method}"),
            json!({ "caller": caller, "args": args, "rows": rows }),
        )?;
        for (table, id) in &t.touched {
            t.tx.execute(
                &format!("UPDATE {table} SET audit_seq = ?1, audit_hash = ?2 WHERE id = ?3"),
                params![seq as i64, hash, id],
            )?;
        }
        t.tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('audit_tail', ?1)",
            params![json!({ "seq": seq, "hash": hash }).to_string()],
        )?;
        t.tx.commit()?;
        Ok(())
    }

    /// Agents must exist in this project and not be retired. A call is a
    /// heartbeat; a lost agent that calls again is active again.
    fn check_caller(&self, t: &mut Tx<'_>, caller: Caller) -> Result<()> {
        let Caller::Agent(id) = caller else {
            return Ok(());
        };
        let status: Option<String> =
            t.tx.query_row(
                "SELECT status FROM agent WHERE id = ?1 AND project_id = ?2",
                params![id, self.project_id],
                |r| r.get(0),
            )
            .optional()?;
        match status.as_deref().and_then(AgentStatus::parse) {
            None | Some(AgentStatus::Retired) => Err(HubError::Unauthenticated),
            Some(s) => {
                t.tx.execute(
                    "UPDATE agent SET last_seen_at = ?1 WHERE id = ?2",
                    params![t.now as i64, id],
                )?;
                if s == AgentStatus::Lost {
                    t.tx.execute(
                        "UPDATE agent SET status = 'active' WHERE id = ?1",
                        params![id],
                    )?;
                    t.touch("agent", id);
                }
                Ok(())
            }
        }
    }

    /// Expire leases and mark silent agents lost (ADR-0012). Runs at the
    /// start of every call; audited only when it changes something.
    fn sweep(&self, conn: &mut Connection) -> Result<()> {
        let now = self.clock.now_ms() as i64;
        let lost_before = now - 2 * self.cfg.heartbeat_ms as i64;
        let ids = |conn: &Connection, sql: &str, v: i64| -> Result<Vec<i64>> {
            let mut st = conn.prepare(sql)?;
            let rows = st.query_map(params![v], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
        };
        let expired = ids(
            conn,
            "SELECT id FROM claim WHERE status = 'active' AND lease_expires_at <= ?1",
            now,
        )?;
        let lost = ids(
            conn,
            "SELECT id FROM agent WHERE status = 'active' AND last_seen_at <= ?1",
            lost_before,
        )?;
        if expired.is_empty() && lost.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut t = Tx {
            tx,
            touched: Vec::new(),
            now: now as u64,
        };
        let mut expired_all = expired.clone();
        let mut reset_tasks = Vec::new();
        for a in &lost {
            t.tx.execute("UPDATE agent SET status = 'lost' WHERE id = ?1", params![a])?;
            t.touch("agent", *a);
            expired_all.extend(ids(
                &t.tx,
                "SELECT id FROM claim WHERE status = 'active' AND agent_id = ?1",
                *a,
            )?);
            reset_tasks.extend(ids(
                &t.tx,
                "SELECT id FROM task WHERE status = 'in_progress' AND assigned_to_agent_id = ?1",
                *a,
            )?);
        }
        for c in &expired_all {
            t.tx.execute(
                "UPDATE claim SET status = 'expired' WHERE id = ?1",
                params![c],
            )?;
            t.touch("claim", *c);
        }
        for task in &reset_tasks {
            t.tx.execute(
                "UPDATE task SET status = 'pending' WHERE id = ?1",
                params![task],
            )?;
            t.touch("task", *task);
        }
        self.commit(
            t,
            Caller::System,
            "sweep",
            json!({ "expired_claims": expired_all, "lost_agents": lost, "tasks_reset": reset_tasks }),
        )
    }

    fn require_user(caller: Caller) -> Result<()> {
        match caller {
            Caller::User => Ok(()),
            _ => Err(HubError::Forbidden("only the user may do this".into())),
        }
    }

    fn require_agent(caller: Caller) -> Result<AgentId> {
        match caller {
            Caller::Agent(id) => Ok(id),
            _ => Err(HubError::Forbidden("only an agent may do this".into())),
        }
    }

    fn agent_in(&self, conn: &Connection, id: AgentId) -> Result<Agent> {
        conn.query_row(
            &format!("SELECT {AGENT_COLS} FROM agent WHERE id = ?1 AND project_id = ?2"),
            params![id, self.project_id],
            agent_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("agent {id}")))
    }

    fn perms(&self, conn: &Connection, caller: Caller) -> Result<Option<Permissions>> {
        match caller {
            Caller::Agent(id) => Ok(Some(self.agent_in(conn, id)?.permissions)),
            _ => Ok(None),
        }
    }

    // -- agents --------------------------------------------------------------

    /// Register an agent (user only). Returns the agent and its bearer
    /// token, which is shown once and stored only as a hash.
    pub fn register_agent(&self, caller: Caller, req: NewAgent) -> Result<(Agent, String)> {
        let args = json!({
            "display_name": req.display_name, "model_id": req.model_id,
            "role": req.role, "family": req.family, "permissions": req.permissions,
        });
        self.mutate(caller, "register_agent", args, |t| {
            Self::require_user(caller)?;
            valid_name("display_name", &req.display_name)?;
            for (f, v) in [("model_id", &req.model_id), ("role", &req.role), ("family", &req.family)] {
                text(f, v, 200)?;
            }
            for w in &req.permissions.claim_within {
                ClaimPath::parse(w)
                    .map_err(|e| HubError::Invalid(format!("claim_within {w:?}: {e}")))?;
            }
            let caps = if req.capabilities.is_null() {
                json!({})
            } else {
                req.capabilities.clone()
            };
            if caps.to_string().len() > 16 << 10 {
                return Err(HubError::Invalid("capabilities too large".into()));
            }
            let taken: bool = t.tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM agent WHERE project_id = ?1 AND display_name = ?2)",
                params![self.project_id, req.display_name],
                |r| r.get(0),
            )?;
            if taken {
                return Err(HubError::Invalid(format!(
                    "an agent named {} already exists",
                    req.display_name
                )));
            }
            t.tx.execute(
                "INSERT INTO agent (project_id, display_name, model_id, role, family, capabilities_json, permissions_json, status, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8)",
                params![
                    self.project_id,
                    req.display_name,
                    req.model_id,
                    req.role,
                    req.family,
                    caps.to_string(),
                    serde_json::to_string(&req.permissions).unwrap_or_default(),
                    t.now as i64
                ],
            )?;
            let id = t.tx.last_insert_rowid();
            let mut raw = [0u8; 32];
            getrandom::fill(&mut raw)
                .map_err(|e| HubError::Invalid(format!("no OS randomness: {e}")))?;
            let token = format!("anc_{}", anchor_policy::audit::hex(&raw));
            t.tx.execute(
                "INSERT INTO agent_token (id, sha256) VALUES (?1, ?2)",
                params![id, token_hash(&token)],
            )?;
            t.touch("agent", id);
            t.touch("agent_token", id);
            Ok((self.agent_in(&t.tx, id)?, token))
        })
    }

    /// Map a bearer token to its agent. Retired agents and unknown tokens
    /// are refused. Comparison is on the token's SHA-256, looked up by
    /// index, so timing reveals nothing about the token itself.
    pub fn authenticate(&self, token: &str) -> Result<Caller> {
        if !token.starts_with("anc_") || token.len() != 68 {
            return Err(HubError::Unauthenticated);
        }
        let conn = self.lock();
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT a.id, a.status FROM agent_token t JOIN agent a ON a.id = t.id
                 WHERE t.sha256 = ?1 AND a.project_id = ?2",
                params![token_hash(token), self.project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((id, status)) if status != "retired" => Ok(Caller::Agent(id)),
            _ => Err(HubError::Unauthenticated),
        }
    }

    /// Retire an agent (user only): its token stops working, its claims are
    /// revoked and its in-progress tasks return to pending.
    pub fn retire_agent(&self, caller: Caller, id: AgentId) -> Result<()> {
        self.mutate(caller, "retire_agent", json!({ "agent_id": id }), |t| {
            Self::require_user(caller)?;
            self.agent_in(&t.tx, id)?;
            t.tx.execute(
                "UPDATE agent SET status = 'retired' WHERE id = ?1",
                params![id],
            )?;
            t.touch("agent", id);
            let mut st =
                t.tx.prepare("SELECT id FROM claim WHERE agent_id = ?1 AND status = 'active'")?;
            let claims: Vec<i64> = st
                .query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(st);
            for c in claims {
                t.tx.execute(
                    "UPDATE claim SET status = 'revoked' WHERE id = ?1",
                    params![c],
                )?;
                t.touch("claim", c);
            }
            let mut st = t.tx.prepare(
                "SELECT id FROM task WHERE assigned_to_agent_id = ?1 AND status = 'in_progress'",
            )?;
            let tasks: Vec<i64> = st
                .query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(st);
            for task in tasks {
                t.tx.execute(
                    "UPDATE task SET status = 'pending' WHERE id = ?1",
                    params![task],
                )?;
                t.touch("task", task);
            }
            Ok(())
        })
    }

    pub fn agent(&self, id: AgentId) -> Result<Agent> {
        self.agent_in(&self.lock(), id)
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {AGENT_COLS} FROM agent WHERE project_id = ?1 ORDER BY id"
        ))?;
        let v = st
            .query_map(params![self.project_id], agent_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Record that the caller is alive (the backend calls this for its
    /// runtimes while a model call is in flight).
    pub fn heartbeat(&self, caller: Caller) -> Result<()> {
        Self::require_agent(caller)?;
        self.mutate(caller, "heartbeat", json!({}), |_| Ok(()))
    }

    // -- claims --------------------------------------------------------------

    /// Request a claim (spec §6.4). Granted with a lease if no conflicting
    /// active claim of another agent overlaps; otherwise
    /// [`HubError::Conflict`] names the holder.
    pub fn request_claim(
        &self,
        caller: Caller,
        path: &str,
        claim_type: ClaimType,
        isolated: bool,
        ttl_ms: Option<u64>,
    ) -> Result<Claim> {
        let args = json!({ "path": path, "claim_type": claim_type, "isolated": isolated, "ttl_ms": ttl_ms });
        self.mutate(caller, "request_claim", args, |t| {
            let me = Self::require_agent(caller)?;
            let cp = ClaimPath::parse(path).map_err(HubError::Invalid)?;
            if isolated && claim_type != ClaimType::Read {
                return Err(HubError::Invalid("only read claims can be isolated".into()));
            }
            let perms = self.agent_in(&t.tx, me)?.permissions;
            if !perms.claim_types.contains(&claim_type) {
                return Err(HubError::Forbidden(format!(
                    "{claim_type} claims are not granted to this agent"
                )));
            }
            let within = perms
                .claim_within
                .iter()
                .filter_map(|w| ClaimPath::parse(w).ok())
                .any(|w| w.covers(&cp));
            if !within {
                return Err(HubError::Forbidden(format!(
                    "{cp} is outside the paths this agent may claim"
                )));
            }
            for other in self.active_claims(&t.tx)? {
                if other.agent_id != me
                    && conflicts(claim_type, isolated, other.claim_type, other.isolated)
                    && stored(&other.path).overlaps(&cp)
                {
                    return Err(HubError::Conflict(Box::new(other)));
                }
            }
            let default = match claim_type {
                ClaimType::Read => self.cfg.ttl_read_ms,
                ClaimType::Edit => self.cfg.ttl_edit_ms,
                ClaimType::Review => self.cfg.ttl_review_ms,
            };
            let ttl = ttl_ms.unwrap_or(default).clamp(1000, self.cfg.max_ttl_ms);
            t.tx.execute(
                "INSERT INTO claim (project_id, agent_id, path, claim_type, isolated, lease_expires_at, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
                params![
                    self.project_id,
                    me,
                    cp.to_string(),
                    claim_type.as_str(),
                    isolated,
                    (t.now + ttl) as i64,
                    t.now as i64
                ],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("claim", id);
            self.claim_in(&t.tx, id)
        })
    }

    fn claim_in(&self, conn: &Connection, id: i64) -> Result<Claim> {
        conn.query_row(
            &format!("SELECT {CLAIM_COLS} FROM claim WHERE id = ?1 AND project_id = ?2"),
            params![id, self.project_id],
            claim_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("claim {id}")))
    }

    fn active_claims(&self, conn: &Connection) -> Result<Vec<Claim>> {
        let mut st = conn.prepare(&format!(
            "SELECT {CLAIM_COLS} FROM claim WHERE project_id = ?1 AND status = 'active' ORDER BY id"
        ))?;
        let v = st
            .query_map(params![self.project_id], claim_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    fn own_active_claim(&self, t: &Tx<'_>, caller: Caller, id: i64) -> Result<Claim> {
        let me = Self::require_agent(caller)?;
        let c = self.claim_in(&t.tx, id)?;
        if c.agent_id != me {
            return Err(HubError::Forbidden(
                "the claim belongs to another agent".into(),
            ));
        }
        if c.status != ClaimStatus::Active {
            return Err(HubError::Invalid(format!("the claim is {}", c.status)));
        }
        Ok(c)
    }

    /// Extend the caller's own active claim.
    pub fn renew_claim(&self, caller: Caller, id: i64, ttl_ms: Option<u64>) -> Result<Claim> {
        self.mutate(
            caller,
            "renew_claim",
            json!({ "claim_id": id, "ttl_ms": ttl_ms }),
            |t| {
                let c = self.own_active_claim(t, caller, id)?;
                let default = match c.claim_type {
                    ClaimType::Read => self.cfg.ttl_read_ms,
                    ClaimType::Edit => self.cfg.ttl_edit_ms,
                    ClaimType::Review => self.cfg.ttl_review_ms,
                };
                let ttl = ttl_ms.unwrap_or(default).clamp(1000, self.cfg.max_ttl_ms);
                t.tx.execute(
                    "UPDATE claim SET lease_expires_at = ?1 WHERE id = ?2",
                    params![(t.now + ttl) as i64, id],
                )?;
                t.touch("claim", id);
                self.claim_in(&t.tx, id)
            },
        )
    }

    /// Release the caller's own claim.
    pub fn release_claim(&self, caller: Caller, id: i64) -> Result<()> {
        self.mutate(caller, "release_claim", json!({ "claim_id": id }), |t| {
            self.own_active_claim(t, caller, id)?;
            t.tx.execute(
                "UPDATE claim SET status = 'released' WHERE id = ?1",
                params![id],
            )?;
            t.touch("claim", id);
            Ok(())
        })
    }

    /// Revoke any claim (user only).
    pub fn revoke_claim(&self, caller: Caller, id: i64) -> Result<()> {
        self.mutate(caller, "revoke_claim", json!({ "claim_id": id }), |t| {
            Self::require_user(caller)?;
            let c = self.claim_in(&t.tx, id)?;
            if c.status != ClaimStatus::Active {
                return Err(HubError::Invalid(format!("the claim is {}", c.status)));
            }
            t.tx.execute(
                "UPDATE claim SET status = 'revoked' WHERE id = ?1",
                params![id],
            )?;
            t.touch("claim", id);
            Ok(())
        })
    }

    /// Claims visible to every caller (coordination needs them).
    pub fn list_claims(&self, caller: Caller, active_only: bool) -> Result<Vec<Claim>> {
        self.mutate(
            caller,
            "list_claims",
            json!({ "active_only": active_only }),
            |t| {
                if active_only {
                    return self.active_claims(&t.tx);
                }
                let mut st = t.tx.prepare(&format!(
                    "SELECT {CLAIM_COLS} FROM claim WHERE project_id = ?1 ORDER BY id"
                ))?;
                let v = st
                    .query_map(params![self.project_id], claim_row)?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(v)
            },
        )
    }

    /// Another agent's active claim that forbids `agent` from writing the
    /// workspace file `rel` (used by the backend before every write).
    pub fn edit_blocker(&self, agent: AgentId, rel: &str) -> Result<Option<Claim>> {
        let target = ClaimPath::file(rel).map_err(HubError::Invalid)?;
        let mut conn = self.lock();
        self.sweep(&mut conn)?;
        Ok(self.active_claims(&conn)?.into_iter().find(|c| {
            c.agent_id != agent
                && conflicts(ClaimType::Edit, false, c.claim_type, c.isolated)
                && stored(&c.path).overlaps(&target)
        }))
    }

    /// Whether `agent` holds an active edit claim covering file `rel`.
    pub fn holds_edit(&self, agent: AgentId, rel: &str) -> Result<bool> {
        let target = ClaimPath::file(rel).map_err(HubError::Invalid)?;
        let mut conn = self.lock();
        self.sweep(&mut conn)?;
        Ok(self.active_claims(&conn)?.iter().any(|c| {
            c.agent_id == agent
                && c.claim_type == ClaimType::Edit
                && stored(&c.path).covers(&target)
        }))
    }

    /// Every claim `agent` ever held (the merge queue checks a submission
    /// against these).
    pub fn claim_history(&self, agent: AgentId) -> Result<Vec<Claim>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {CLAIM_COLS} FROM claim WHERE project_id = ?1 AND agent_id = ?2 ORDER BY id"
        ))?;
        let v = st
            .query_map(params![self.project_id, agent], claim_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Active claims (no audit entry; for the backend's own checks).
    pub fn active_claims_snapshot(&self) -> Result<Vec<Claim>> {
        let mut conn = self.lock();
        self.sweep(&mut conn)?;
        self.active_claims(&conn)
    }

    // -- messages ------------------------------------------------------------

    /// Send a message through the Hub (A2S2A). Held for the user's approval
    /// when configured; a held message is invisible to its recipient.
    pub fn send_message(
        &self,
        caller: Caller,
        to: Recipient,
        message_type: MessageType,
        content: Value,
    ) -> Result<Message> {
        let encoded = content.to_string();
        let args = json!({
            "to": to,
            "message_type": message_type,
            "content_sha256": anchor_policy::audit::sha256_hex(encoded.as_bytes()),
            "content_len": encoded.len(),
        });
        self.mutate(caller, "send_message", args, |t| {
            if encoded.len() > self.cfg.max_content_bytes {
                return Err(HubError::Invalid(format!(
                    "message content exceeds {} bytes",
                    self.cfg.max_content_bytes
                )));
            }
            let perms = self.perms(&t.tx, caller)?;
            if let Some(p) = &perms
                && !p.message_types.contains(&message_type)
            {
                return Err(HubError::Forbidden(format!(
                    "{message_type} messages are not granted to this agent"
                )));
            }
            let (to_agent, room) = match &to {
                Recipient::Agent(id) => {
                    if caller == Caller::Agent(*id) {
                        return Err(HubError::Invalid("an agent cannot message itself".into()));
                    }
                    let a = self.agent_in(&t.tx, *id)?;
                    if a.status == AgentStatus::Retired {
                        return Err(HubError::Invalid(format!("agent {id} is retired")));
                    }
                    (Some(*id), None)
                }
                Recipient::Room(r) => {
                    valid_name("room", r)?;
                    (None, Some(r.clone()))
                }
            };
            let held = perms.is_some_and(|p| p.messages_need_approval)
                || (matches!(caller, Caller::Agent(_))
                    && self.cfg.approve_message_types.contains(&message_type));
            let from = match caller {
                Caller::Agent(id) => Some(id),
                _ => None,
            };
            t.tx.execute(
                "INSERT INTO message (project_id, from_agent_id, to_agent_id, room_id, message_type, content_json, requires_approval, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    self.project_id,
                    from,
                    to_agent,
                    room,
                    message_type.as_str(),
                    encoded,
                    held,
                    if held { "held" } else { "delivered" },
                    t.now as i64
                ],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("message", id);
            self.message_in(&t.tx, id)
        })
    }

    fn message_in(&self, conn: &Connection, id: i64) -> Result<Message> {
        conn.query_row(
            &format!("SELECT {MESSAGE_COLS} FROM message WHERE id = ?1 AND project_id = ?2"),
            params![id, self.project_id],
            message_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("message {id}")))
    }

    /// Messages the caller may see: for an agent, delivered messages to it
    /// or in rooms, and everything it sent; for the user, everything.
    pub fn query_messages(&self, caller: Caller, q: MessageQuery) -> Result<Vec<Message>> {
        let args = serde_json::to_value(&q).unwrap_or_default();
        self.mutate(caller, "query_messages", args, |t| {
            let limit = q.limit.unwrap_or(50).clamp(1, 500) as i64;
            let since = q.since_id.unwrap_or(0);
            let me = match caller {
                Caller::Agent(id) => id,
                _ => -1,
            };
            let visibility = if me < 0 {
                "?3 = ?3"
            } else {
                "((to_agent_id = ?3 AND state = 'delivered') OR from_agent_id = ?3
                  OR (room_id IS NOT NULL AND state = 'delivered'))"
            };
            let room_filter = if q.room.is_some() {
                "AND room_id = ?5"
            } else {
                "AND ?5 IS NULL"
            };
            let sql = format!(
                "SELECT {MESSAGE_COLS} FROM message
                 WHERE project_id = ?1 AND id > ?2 AND {visibility} {room_filter}
                 ORDER BY id LIMIT ?4"
            );
            let mut st = t.tx.prepare(&sql)?;
            let v = st
                .query_map(
                    params![self.project_id, since, me, limit, q.room],
                    message_row,
                )?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    /// Release a held message to its recipient (user only).
    pub fn approve_message(&self, caller: Caller, id: i64) -> Result<Message> {
        self.decide_message(caller, id, true)
    }

    pub fn reject_message(&self, caller: Caller, id: i64) -> Result<Message> {
        self.decide_message(caller, id, false)
    }

    fn decide_message(&self, caller: Caller, id: i64, approve: bool) -> Result<Message> {
        let method = if approve {
            "approve_message"
        } else {
            "reject_message"
        };
        self.mutate(caller, method, json!({ "message_id": id }), |t| {
            Self::require_user(caller)?;
            let m = self.message_in(&t.tx, id)?;
            if m.state != MessageState::Held {
                return Err(HubError::Invalid(format!("message {id} is {}", m.state)));
            }
            let state = if approve { "delivered" } else { "rejected" };
            t.tx.execute(
                "UPDATE message SET state = ?1, approved_by = 'user' WHERE id = ?2",
                params![state, id],
            )?;
            t.touch("message", id);
            self.message_in(&t.tx, id)
        })
    }

    // -- decisions -----------------------------------------------------------

    pub fn propose_decision(
        &self,
        caller: Caller,
        summary: &str,
        rationale: &str,
        evidence_ref: Option<i64>,
    ) -> Result<Decision> {
        let args =
            json!({ "summary": summary, "rationale": rationale, "evidence_ref": evidence_ref });
        self.mutate(caller, "propose_decision", args, |t| {
            if let Some(p) = self.perms(&t.tx, caller)?
                && !p.may_propose_decisions
            {
                return Err(HubError::Forbidden("may not propose decisions".into()));
            }
            text("summary", summary, 1000)?;
            text("rationale", rationale, MAX_TEXT)?;
            if let Some(a) = evidence_ref {
                self.artifact_in(&t.tx, a)?;
            }
            let agent = match caller {
                Caller::Agent(id) => Some(id),
                _ => None,
            };
            t.tx.execute(
                "INSERT INTO decision (project_id, agent_id, summary, rationale, evidence_ref, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'proposed', ?6)",
                params![self.project_id, agent, summary, rationale, evidence_ref, t.now as i64],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("decision", id);
            self.decision_in(&t.tx, id)
        })
    }

    fn decision_in(&self, conn: &Connection, id: i64) -> Result<Decision> {
        conn.query_row(
            &format!("SELECT {DECISION_COLS} FROM decision WHERE id = ?1 AND project_id = ?2"),
            params![id, self.project_id],
            decision_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("decision {id}")))
    }

    /// Accept or reject a proposed decision (user only).
    pub fn decide(&self, caller: Caller, id: i64, accept: bool, reason: &str) -> Result<Decision> {
        let method = if accept {
            "accept_decision"
        } else {
            "reject_decision"
        };
        self.mutate(
            caller,
            method,
            json!({ "decision_id": id, "reason": reason }),
            |t| {
                Self::require_user(caller)?;
                let d = self.decision_in(&t.tx, id)?;
                if d.status != DecisionStatus::Proposed {
                    return Err(HubError::Invalid(format!("decision {id} is {}", d.status)));
                }
                if reason.len() > MAX_TEXT {
                    return Err(HubError::Invalid("reason too long".into()));
                }
                let status = if accept { "accepted" } else { "rejected" };
                t.tx.execute(
                    "UPDATE decision SET status = ?1, decided_reason = ?2 WHERE id = ?3",
                    params![status, reason, id],
                )?;
                t.touch("decision", id);
                self.decision_in(&t.tx, id)
            },
        )
    }

    pub fn decisions(&self, caller: Caller) -> Result<Vec<Decision>> {
        self.mutate(caller, "list_decisions", json!({}), |t| {
            let mut st = t.tx.prepare(&format!(
                "SELECT {DECISION_COLS} FROM decision WHERE project_id = ?1 ORDER BY id"
            ))?;
            let v = st
                .query_map(params![self.project_id], decision_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    // -- tasks ---------------------------------------------------------------

    pub fn create_task(
        &self,
        caller: Caller,
        description: &str,
        depends_on: &[i64],
        assign_to: Option<AgentId>,
    ) -> Result<Task> {
        let args =
            json!({ "description": description, "depends_on": depends_on, "assign_to": assign_to });
        self.mutate(caller, "create_task", args, |t| {
            if let Some(p) = self.perms(&t.tx, caller)? {
                if !p.may_create_tasks {
                    return Err(HubError::Forbidden("may not create tasks".into()));
                }
                if assign_to.is_some_and(|a| Caller::Agent(a) != caller) && !p.may_assign_tasks {
                    return Err(HubError::Forbidden("may not assign tasks to others".into()));
                }
            }
            text("description", description, MAX_TEXT)?;
            if depends_on.len() > 64 {
                return Err(HubError::Invalid("too many dependencies".into()));
            }
            for d in depends_on {
                self.task_in(&t.tx, *d)?;
            }
            if let Some(a) = assign_to {
                self.agent_in(&t.tx, a)?;
            }
            let creator = match caller {
                Caller::Agent(id) => Some(id),
                _ => None,
            };
            t.tx.execute(
                "INSERT INTO task (project_id, assigned_to_agent_id, description, status, depends_on_json, created_by_agent_id, created_at)
                 VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
                params![
                    self.project_id,
                    assign_to,
                    description,
                    serde_json::to_string(depends_on).unwrap_or_default(),
                    creator,
                    t.now as i64
                ],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("task", id);
            self.task_in(&t.tx, id)
        })
    }

    fn task_in(&self, conn: &Connection, id: i64) -> Result<Task> {
        conn.query_row(
            &format!("SELECT {TASK_COLS} FROM task WHERE id = ?1 AND project_id = ?2"),
            params![id, self.project_id],
            task_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("task {id}")))
    }

    pub fn assign_task(&self, caller: Caller, id: i64, agent: AgentId) -> Result<Task> {
        self.mutate(
            caller,
            "assign_task",
            json!({ "task_id": id, "agent_id": agent }),
            |t| {
                if let Some(p) = self.perms(&t.tx, caller)?
                    && !p.may_assign_tasks
                {
                    return Err(HubError::Forbidden("may not assign tasks".into()));
                }
                self.task_in(&t.tx, id)?;
                self.agent_in(&t.tx, agent)?;
                t.tx.execute(
                    "UPDATE task SET assigned_to_agent_id = ?1 WHERE id = ?2",
                    params![agent, id],
                )?;
                t.touch("task", id);
                self.task_in(&t.tx, id)
            },
        )
    }

    /// Agents may only move tasks assigned to them; starting a task needs
    /// every dependency done.
    pub fn update_task_status(&self, caller: Caller, id: i64, status: TaskStatus) -> Result<Task> {
        self.mutate(
            caller,
            "update_task_status",
            json!({ "task_id": id, "status": status }),
            |t| {
                let task = self.task_in(&t.tx, id)?;
                if let Caller::Agent(me) = caller
                    && task.assigned_to_agent_id != Some(me)
                {
                    return Err(HubError::Forbidden(
                        "the task is not assigned to this agent".into(),
                    ));
                }
                if status == TaskStatus::InProgress {
                    for d in &task.depends_on {
                        if self.task_in(&t.tx, *d)?.status != TaskStatus::Done {
                            return Err(HubError::Forbidden(format!(
                                "dependency task {d} is not done"
                            )));
                        }
                    }
                }
                t.tx.execute(
                    "UPDATE task SET status = ?1 WHERE id = ?2",
                    params![status.as_str(), id],
                )?;
                t.touch("task", id);
                self.task_in(&t.tx, id)
            },
        )
    }

    pub fn tasks(&self, caller: Caller) -> Result<Vec<Task>> {
        self.mutate(caller, "list_tasks", json!({}), |t| {
            let mut st = t.tx.prepare(&format!(
                "SELECT {TASK_COLS} FROM task WHERE project_id = ?1 ORDER BY id"
            ))?;
            let v = st
                .query_map(params![self.project_id], task_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    pub fn task(&self, caller: Caller, id: i64) -> Result<Task> {
        self.mutate(caller, "get_task", json!({ "task_id": id }), |t| {
            self.task_in(&t.tx, id)
        })
    }

    // -- artifacts -----------------------------------------------------------

    pub fn register_artifact(
        &self,
        caller: Caller,
        kind: ArtifactKind,
        file: &VerifiedFile,
        evidence: Value,
    ) -> Result<Artifact> {
        let args = json!({ "kind": kind, "path": file.path, "sha256": file.sha256 });
        self.mutate(caller, "register_artifact", args, |t| {
            if let Some(p) = self.perms(&t.tx, caller)?
                && !p.may_register_artifacts
            {
                return Err(HubError::Forbidden("may not register artifacts".into()));
            }
            let ev = evidence.to_string();
            if ev.len() > self.cfg.max_content_bytes {
                return Err(HubError::Invalid("evidence too large".into()));
            }
            let creator = match caller {
                Caller::Agent(id) => Some(id),
                _ => None,
            };
            t.tx.execute(
                "INSERT INTO artifact (project_id, kind, path, sha256, created_by_agent_id, evidence_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![self.project_id, kind.as_str(), file.path, file.sha256, creator, ev, t.now as i64],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("artifact", id);
            self.artifact_in(&t.tx, id)
        })
    }

    fn artifact_in(&self, conn: &Connection, id: i64) -> Result<Artifact> {
        conn.query_row(
            &format!("SELECT {ARTIFACT_COLS} FROM artifact WHERE id = ?1 AND project_id = ?2"),
            params![id, self.project_id],
            artifact_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("artifact {id}")))
    }

    /// Store tester evidence (logs, coverage, claims) under the state
    /// directory and register it as an artifact. Backend only: the Hub
    /// writes and hashes the bytes itself, the file is written once and
    /// owner-read-only, and it outlives the isolated workspace it came
    /// from. The artifact path is `evidence:<name>`.
    pub fn register_evidence(
        &self,
        caller: Caller,
        kind: ArtifactKind,
        name: &str,
        bytes: &[u8],
        evidence: Value,
    ) -> Result<Artifact> {
        let sha256 = anchor_policy::audit::sha256_hex(bytes);
        let args = json!({ "kind": kind, "name": name, "sha256": sha256, "bytes": bytes.len() });
        self.mutate(caller, "register_evidence", args, |t| {
            if caller != Caller::System {
                return Err(HubError::Forbidden(
                    "evidence is recorded by the backend".into(),
                ));
            }
            if !valid_evidence_name(name) {
                return Err(HubError::Invalid(format!("bad evidence name {name:?}")));
            }
            if bytes.len() > MAX_EVIDENCE_BYTES {
                return Err(HubError::Invalid("evidence file too large".into()));
            }
            let ev = evidence.to_string();
            if ev.len() > self.cfg.max_content_bytes {
                return Err(HubError::Invalid("evidence too large".into()));
            }
            write_once(&self.evidence_dir.join(name), bytes)
                .map_err(|e| HubError::Invalid(format!("evidence {name}: {e}")))?;
            let path = format!("evidence:{name}");
            t.tx.execute(
                "INSERT INTO artifact (project_id, kind, path, sha256, created_by_agent_id, evidence_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                params![self.project_id, kind.as_str(), path, sha256, ev, t.now as i64],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("artifact", id);
            self.artifact_in(&t.tx, id)
        })
    }

    /// Artifacts, newest last; `prefix` filters on the path (e.g.
    /// `evidence:12-`).
    pub fn list_artifacts(&self, caller: Caller, prefix: Option<&str>) -> Result<Vec<Artifact>> {
        Self::require_backend(caller)?;
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifact WHERE project_id = ?1 ORDER BY id"
        ))?;
        let rows = st
            .query_map(params![self.project_id], artifact_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|a| prefix.is_none_or(|p| a.path.starts_with(p)))
            .collect())
    }

    /// The bytes of an evidence artifact, checked against its recorded
    /// hash.
    pub fn read_evidence(&self, caller: Caller, artifact: &Artifact) -> Result<Vec<u8>> {
        Self::require_backend(caller)?;
        let name = artifact
            .path
            .strip_prefix("evidence:")
            .filter(|n| valid_evidence_name(n))
            .ok_or_else(|| HubError::Invalid(format!("{} is not evidence", artifact.path)))?;
        let bytes = std::fs::read(self.evidence_dir.join(name))
            .map_err(|e| HubError::Invalid(format!("evidence {name}: {e}")))?;
        if anchor_policy::audit::sha256_hex(&bytes) != artifact.sha256 {
            return Err(HubError::Invalid(format!(
                "evidence {name} does not match its recorded hash"
            )));
        }
        Ok(bytes)
    }

    // -- the user's own view (dashboard) -------------------------------------

    /// Every row of `table` for this project, oldest first, read without a
    /// transaction, a sweep or an audit entry. These are the *user's* views
    /// (the dashboard polls them); an agent's reads stay audited because
    /// they are agent actions. Never reachable from the agent endpoints.
    fn view_rows<T>(
        &self,
        cols: &str,
        table: &str,
        after: i64,
        limit: u32,
        row: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {cols} FROM {table} WHERE project_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3"
        ))?;
        let v = st
            .query_map(params![self.project_id, after, limit.max(1) as i64], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Every claim, in any state (unaudited; see [`Hub::view_rows`]).
    pub fn view_claims(&self) -> Result<Vec<Claim>> {
        self.view_rows(CLAIM_COLS, "claim", 0, u32::MAX, claim_row)
    }

    /// Messages with an id above `after`, held ones included (unaudited).
    pub fn view_messages(&self, after: i64, limit: u32) -> Result<Vec<Message>> {
        self.view_rows(MESSAGE_COLS, "message", after, limit, message_row)
    }

    /// Every decision (unaudited).
    pub fn view_decisions(&self) -> Result<Vec<Decision>> {
        self.view_rows(DECISION_COLS, "decision", 0, u32::MAX, decision_row)
    }

    /// Every task (unaudited).
    pub fn view_tasks(&self) -> Result<Vec<Task>> {
        self.view_rows(TASK_COLS, "task", 0, u32::MAX, task_row)
    }

    // -- merge queue entries (backend only) ----------------------------------

    fn require_backend(caller: Caller) -> Result<()> {
        match caller {
            Caller::User | Caller::System => Ok(()),
            Caller::Agent(_) => Err(HubError::Forbidden(
                "merge entries are managed by the backend".into(),
            )),
        }
    }

    pub fn merge_enqueue(
        &self,
        caller: Caller,
        agent: Option<AgentId>,
        kind: MergeKind,
        base: &str,
        submitted: &str,
        reverts: Option<i64>,
    ) -> Result<MergeEntry> {
        let args = json!({ "agent_id": agent, "kind": kind, "base": base, "submitted": submitted, "reverts": reverts });
        self.mutate(caller, "merge_enqueue", args, |t| {
            Self::require_backend(caller)?;
            t.tx.execute(
                "INSERT INTO merge (agent_id, kind, base, submitted, status, feedback_json, reverts, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'queued', '{}', ?5, ?6)",
                params![agent, kind.as_str(), base, submitted, reverts, t.now as i64],
            )?;
            let id = t.tx.last_insert_rowid();
            t.touch("merge", id);
            self.merge_in(&t.tx, id)
        })
    }

    pub fn merge_update(
        &self,
        caller: Caller,
        seq: i64,
        status: MergeStatus,
        pre: Option<&str>,
        merged: Option<&str>,
        feedback: Value,
    ) -> Result<MergeEntry> {
        let args = json!({ "seq": seq, "status": status, "pre": pre, "merged": merged, "feedback": feedback });
        self.mutate(caller, "merge_update", args, |t| {
            Self::require_backend(caller)?;
            self.merge_in(&t.tx, seq)?;
            t.tx.execute(
                "UPDATE merge SET status = ?1, pre = COALESCE(?2, pre), merged = COALESCE(?3, merged), feedback_json = ?4 WHERE id = ?5",
                params![status.as_str(), pre, merged, feedback.to_string(), seq],
            )?;
            t.touch("merge", seq);
            self.merge_in(&t.tx, seq)
        })
    }

    fn merge_in(&self, conn: &Connection, seq: i64) -> Result<MergeEntry> {
        conn.query_row(
            &format!("SELECT {MERGE_COLS} FROM merge WHERE id = ?1"),
            params![seq],
            merge_row,
        )
        .optional()?
        .ok_or_else(|| HubError::NotFound(format!("merge {seq}")))
    }

    pub fn merges(&self) -> Result<Vec<MergeEntry>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!("SELECT {MERGE_COLS} FROM merge ORDER BY id"))?;
        let v = st
            .query_map([], merge_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // -- verification --------------------------------------------------------

    /// Cross-check the database against the audit log (ADR-0023): every row
    /// must be at the state its latest audit entry recorded, every audited
    /// row must still exist, and the log must still contain the last Hub
    /// entry the database knows about.
    pub fn verify(&self) -> Result<HubVerifyReport> {
        let mut report = HubVerifyReport::default();
        let path = self.audit.path().to_path_buf();
        if let Err(e) = AuditLog::verify(&path) {
            report.errors.push(format!("audit chain: {e}"));
            return Ok(report);
        }
        let entries = AuditLog::read_entries(&path)?;
        let mut latest: HashMap<(String, i64), (u64, String, String)> = HashMap::new();
        for e in &entries {
            if !e.kind.starts_with("hub_") {
                continue;
            }
            report.hub_entries += 1;
            for r in e.data["rows"].as_array().into_iter().flatten() {
                if let (Some(t), Some(id), Some(d)) =
                    (r["table"].as_str(), r["id"].as_i64(), r["digest"].as_str())
                {
                    latest.insert((t.to_string(), id), (e.seq, e.hash.clone(), d.to_string()));
                }
            }
        }
        let conn = self.lock();
        for table in TABLES {
            let mut st = conn.prepare(&format!("SELECT id, audit_seq, audit_hash FROM {table}"))?;
            let rows: Vec<(i64, Option<i64>, Option<String>)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, seq, hash) in rows {
                report.rows += 1;
                let Some((lseq, lhash, digest)) = latest.remove(&(table.to_string(), id)) else {
                    report
                        .errors
                        .push(format!("{table} {id} has no audit entry"));
                    continue;
                };
                if seq != Some(lseq as i64) || hash.as_deref() != Some(lhash.as_str()) {
                    report
                        .errors
                        .push(format!("{table} {id} is not at its latest audited state"));
                }
                match row_state(&conn, table, id)? {
                    Some(s) if row_digest(&s) == digest => {}
                    _ => report
                        .errors
                        .push(format!("{table} {id} differs from its audited state")),
                }
            }
        }
        let mut gone: Vec<_> = latest.into_keys().collect();
        gone.sort();
        for (table, id) in gone {
            report.errors.push(format!("{table} {id} was deleted"));
        }
        let tail: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'audit_tail'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(t) = tail.and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            let seq = t["seq"].as_u64().unwrap_or(u64::MAX);
            let found = entries
                .get(seq as usize)
                .is_some_and(|e| Some(e.hash.as_str()) == t["hash"].as_str());
            if !found {
                report.errors.push(format!(
                    "the audit log no longer contains entry {seq} recorded by the hub (truncated or rewritten)"
                ));
            }
        }
        Ok(report)
    }
}
