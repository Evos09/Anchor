//! SQLite schema (spec §6.3) and row mapping.
//!
//! Every row carries the sequence number and hash of the audit entry that
//! recorded its latest state (`audit_seq`, `audit_hash`). The entry holds a
//! digest of the row, so [`crate::Hub::verify`] can detect edited, deleted
//! or rolled-back rows (ADR-0023).

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use crate::model::*;

pub(crate) const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE project (
    id INTEGER PRIMARY KEY,
    workspace_path TEXT NOT NULL,
    tech_stack_json TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE TABLE agent (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project(id),
    display_name TEXT NOT NULL,
    model_id TEXT NOT NULL,
    role TEXT NOT NULL,
    family TEXT NOT NULL,
    capabilities_json TEXT NOT NULL,
    permissions_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','lost','retired')),
    last_seen_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT,
    UNIQUE (project_id, display_name)
);
CREATE TABLE agent_token (
    id INTEGER PRIMARY KEY REFERENCES agent(id),
    sha256 TEXT NOT NULL UNIQUE,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE TABLE claim (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project(id),
    agent_id INTEGER NOT NULL REFERENCES agent(id),
    path TEXT NOT NULL,
    claim_type TEXT NOT NULL CHECK (claim_type IN ('read','edit','review')),
    isolated INTEGER NOT NULL CHECK (isolated IN (0,1)),
    lease_expires_at INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','released','expired','revoked')),
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE INDEX claim_active ON claim (status);
CREATE TABLE artifact (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project(id),
    kind TEXT NOT NULL CHECK (kind IN ('diff','test_output','screenshot','log','coverage_report')),
    path TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    created_by_agent_id INTEGER REFERENCES agent(id),
    evidence_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE TABLE message (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project(id),
    from_agent_id INTEGER REFERENCES agent(id),
    to_agent_id INTEGER REFERENCES agent(id),
    room_id TEXT,
    message_type TEXT NOT NULL CHECK (message_type IN ('task','question','decision','review','status')),
    content_json TEXT NOT NULL,
    requires_approval INTEGER NOT NULL CHECK (requires_approval IN (0,1)),
    approved_by TEXT,
    state TEXT NOT NULL CHECK (state IN ('held','delivered','rejected')),
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT,
    CHECK ((to_agent_id IS NULL) <> (room_id IS NULL))
);
CREATE TABLE decision (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project(id),
    agent_id INTEGER REFERENCES agent(id),
    summary TEXT NOT NULL,
    rationale TEXT NOT NULL,
    evidence_ref INTEGER REFERENCES artifact(id),
    status TEXT NOT NULL CHECK (status IN ('proposed','accepted','rejected')),
    decided_reason TEXT,
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE TABLE task (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project(id),
    assigned_to_agent_id INTEGER REFERENCES agent(id),
    description TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','in_progress','review','done','blocked')),
    depends_on_json TEXT NOT NULL,
    created_by_agent_id INTEGER REFERENCES agent(id),
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE TABLE merge (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER REFERENCES agent(id),
    kind TEXT NOT NULL CHECK (kind IN ('merge','revert')),
    base TEXT NOT NULL,
    submitted TEXT NOT NULL,
    pre TEXT,
    merged TEXT,
    status TEXT NOT NULL CHECK (status IN ('queued','applying','merged','rejected')),
    feedback_json TEXT NOT NULL,
    reverts INTEGER REFERENCES merge(id),
    created_at INTEGER NOT NULL,
    audit_seq INTEGER, audit_hash TEXT
);
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Tables whose rows are anchored in the audit log.
pub(crate) const TABLES: &[&str] = &[
    "project",
    "agent",
    "agent_token",
    "claim",
    "artifact",
    "message",
    "decision",
    "task",
    "merge",
];

/// Create the database file with owner-only permissions (SQLite's `-wal`
/// and `-shm` files inherit the database file's mode), then open it.
pub(crate) fn open(path: &Path) -> rusqlite::Result<Connection> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v == 0 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
        ))?;
    } else if v != SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "unsupported hub schema version {v}"
        )));
    }
    Ok(conn)
}

fn parse_enum<T>(s: String, f: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    f(&s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unexpected value {s:?}").into(),
        )
    })
}

fn parse_json<T: serde::de::DeserializeOwned>(s: String) -> rusqlite::Result<T> {
    serde_json::from_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub(crate) const AGENT_COLS: &str = "id, project_id, display_name, model_id, role, family, capabilities_json, permissions_json, status, last_seen_at";
pub(crate) fn agent_row(r: &Row<'_>) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: r.get(0)?,
        project_id: r.get(1)?,
        display_name: r.get(2)?,
        model_id: r.get(3)?,
        role: r.get(4)?,
        family: r.get(5)?,
        capabilities: parse_json(r.get(6)?)?,
        permissions: parse_json(r.get(7)?)?,
        status: parse_enum(r.get(8)?, AgentStatus::parse)?,
        last_seen_at: r.get::<_, i64>(9)? as u64,
    })
}

pub(crate) const CLAIM_COLS: &str =
    "id, project_id, agent_id, path, claim_type, isolated, lease_expires_at, status, created_at";
pub(crate) fn claim_row(r: &Row<'_>) -> rusqlite::Result<Claim> {
    Ok(Claim {
        id: r.get(0)?,
        project_id: r.get(1)?,
        agent_id: r.get(2)?,
        path: r.get(3)?,
        claim_type: parse_enum(r.get(4)?, ClaimType::parse)?,
        isolated: r.get(5)?,
        lease_expires_at: r.get::<_, i64>(6)? as u64,
        status: parse_enum(r.get(7)?, ClaimStatus::parse)?,
        created_at: r.get::<_, i64>(8)? as u64,
    })
}

pub(crate) const MESSAGE_COLS: &str = "id, project_id, from_agent_id, to_agent_id, room_id, message_type, content_json, requires_approval, approved_by, state, created_at";
pub(crate) fn message_row(r: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        project_id: r.get(1)?,
        from_agent_id: r.get(2)?,
        to_agent_id: r.get(3)?,
        room_id: r.get(4)?,
        message_type: parse_enum(r.get(5)?, MessageType::parse)?,
        content: parse_json(r.get(6)?)?,
        requires_approval: r.get(7)?,
        approved_by: r.get(8)?,
        state: parse_enum(r.get(9)?, MessageState::parse)?,
        created_at: r.get::<_, i64>(10)? as u64,
    })
}

pub(crate) const DECISION_COLS: &str = "id, project_id, agent_id, summary, rationale, evidence_ref, status, decided_reason, created_at";
pub(crate) fn decision_row(r: &Row<'_>) -> rusqlite::Result<Decision> {
    Ok(Decision {
        id: r.get(0)?,
        project_id: r.get(1)?,
        agent_id: r.get(2)?,
        summary: r.get(3)?,
        rationale: r.get(4)?,
        evidence_ref: r.get(5)?,
        status: parse_enum(r.get(6)?, DecisionStatus::parse)?,
        decided_reason: r.get(7)?,
        created_at: r.get::<_, i64>(8)? as u64,
    })
}

pub(crate) const TASK_COLS: &str = "id, project_id, assigned_to_agent_id, description, status, depends_on_json, created_by_agent_id, created_at";
pub(crate) fn task_row(r: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        project_id: r.get(1)?,
        assigned_to_agent_id: r.get(2)?,
        description: r.get(3)?,
        status: parse_enum(r.get(4)?, TaskStatus::parse)?,
        depends_on: parse_json(r.get(5)?)?,
        created_by_agent_id: r.get(6)?,
        created_at: r.get::<_, i64>(7)? as u64,
    })
}

pub(crate) const ARTIFACT_COLS: &str =
    "id, project_id, kind, path, sha256, created_by_agent_id, evidence_json, created_at";
pub(crate) fn artifact_row(r: &Row<'_>) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        id: r.get(0)?,
        project_id: r.get(1)?,
        kind: parse_enum(r.get(2)?, ArtifactKind::parse)?,
        path: r.get(3)?,
        sha256: r.get(4)?,
        created_by_agent_id: r.get(5)?,
        evidence: parse_json(r.get(6)?)?,
        created_at: r.get::<_, i64>(7)? as u64,
    })
}

pub(crate) const MERGE_COLS: &str =
    "id, agent_id, kind, base, submitted, pre, merged, status, feedback_json, reverts, created_at";
pub(crate) fn merge_row(r: &Row<'_>) -> rusqlite::Result<MergeEntry> {
    Ok(MergeEntry {
        seq: r.get(0)?,
        agent_id: r.get(1)?,
        kind: parse_enum(r.get(2)?, MergeKind::parse)?,
        base: r.get(3)?,
        submitted: r.get(4)?,
        pre: r.get(5)?,
        merged: r.get(6)?,
        status: parse_enum(r.get(7)?, MergeStatus::parse)?,
        feedback: parse_json(r.get(8)?)?,
        reverts: r.get(9)?,
        created_at: r.get::<_, i64>(10)? as u64,
    })
}

/// The audited content of a row: every column except the audit anchor and
/// (for agents) `last_seen_at`, which changes on every call and carries no
/// authority.
pub(crate) fn row_state(
    conn: &Connection,
    table: &str,
    id: i64,
) -> rusqlite::Result<Option<Value>> {
    let one = |sql: String| -> rusqlite::Result<Option<Value>> {
        conn.query_row(&sql, params![id], |r| {
            let n = r.as_ref().column_count();
            let mut obj = serde_json::Map::new();
            for i in 0..n {
                let name = r.as_ref().column_name(i)?.to_string();
                let v: rusqlite::types::Value = r.get(i)?;
                let jv = match v {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(x) => json!(x),
                    rusqlite::types::Value::Real(x) => json!(x),
                    rusqlite::types::Value::Text(s) => json!(s),
                    rusqlite::types::Value::Blob(b) => json!(b),
                };
                obj.insert(name, jv);
            }
            Ok(Value::Object(obj))
        })
        .optional()
    };
    let cols = match table {
        "project" => "id, workspace_path, tech_stack_json, created_at",
        "agent" => {
            "id, project_id, display_name, model_id, role, family, capabilities_json, permissions_json, status"
        }
        "agent_token" => "id, sha256",
        "claim" => CLAIM_COLS,
        "artifact" => ARTIFACT_COLS,
        "message" => MESSAGE_COLS,
        "decision" => DECISION_COLS,
        "task" => TASK_COLS,
        "merge" => MERGE_COLS,
        _ => return Ok(None),
    };
    one(format!("SELECT {cols} FROM {table} WHERE id = ?1"))
}

pub(crate) fn row_digest(state: &Value) -> String {
    // serde_json maps are ordered, so this encoding is canonical.
    anchor_policy::audit::sha256_hex(state.to_string().as_bytes())
}
