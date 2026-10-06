//! The Coordination Hub (spec §6, ADR-0023).
//!
//! The single source of truth for shared project state across agents:
//! registered agents, file claims, messages, tasks, decisions, artifacts
//! and the merge queue's entries, persisted in SQLite next to the
//! workspace's other state (ADR-0022).
//!
//! **A2S2A.** Agents never talk to each other directly. Every message goes
//! through [`Hub::send_message`], where it is checked against the sender's
//! permissions, persisted and audited. A message can be held until the user
//! approves it.
//!
//! **Identity.** Every method takes the [`Caller`] that the transport
//! authenticated (the backend's pipe to a runtime, a bearer token checked
//! by [`Hub::authenticate`], or the local user). No parameter can name the
//! caller, so an agent cannot act as another.
//!
//! **Audit.** Every call, refused or not, appends a `hub_<method>` entry to
//! the workspace's hash-chained audit log, and every row records which
//! entry holds its current state. [`Hub::verify`] detects edited, deleted
//! or rolled-back rows and a truncated log.

pub mod a2a;
mod db;
pub mod http;
mod hub;
pub mod mcp;
pub mod model;
mod path;
pub mod tools;

pub use hub::{Clock, Hub, HubConfig, HubVerifyReport, ManualClock, SystemClock, VerifiedFile};
pub use model::*;
pub use path::ClaimPath;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("not authenticated")]
    Unauthenticated,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("claim conflict: {} holds a {} claim on {} until {}", .0.agent_id, .0.claim_type, .0.path, .0.lease_expires_at)]
    Conflict(Box<Claim>),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("hub database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("audit log: {0}")]
    Audit(#[from] anchor_policy::AuditError),
    #[error("workspace: {0}")]
    Fs(#[from] anchor_fs::FsError),
}

impl HubError {
    /// A short machine-readable code (protocol error mapping).
    pub fn code(&self) -> &'static str {
        match self {
            HubError::Unauthenticated => "unauthenticated",
            HubError::Forbidden(_) => "forbidden",
            HubError::Conflict(_) => "conflict",
            HubError::NotFound(_) => "not_found",
            HubError::Invalid(_) => "invalid",
            HubError::Db(_) | HubError::Audit(_) | HubError::Fs(_) => "internal",
        }
    }
}

pub type Result<T> = std::result::Result<T, HubError>;

#[cfg(test)]
mod tests;
