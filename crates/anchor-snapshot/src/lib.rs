//! Snapshot engine (spec §5.5, ADR-0006): content-addressed, chunked,
//! deduplicated snapshots of a workspace, atomic publication, and atomic,
//! verified restore.
//!
//! * [`take_snapshot`]: scan the workspace (never following symlinks),
//!   re-hash only files whose stat fingerprint changed, store new chunks
//!   and trees, publish, then name the snapshot with a ref.
//! * [`restore`]: journaled, compare-and-swap restore through `anchor-fs`
//!   with verification; failures and crashes return the workspace to its
//!   pre-restore state ([`recover`]).
//! * [`diff`], [`merge3`], [`materialize`]: the merge-queue substrate.
//! * [`verify`], [`gc`]: store maintenance.

pub mod chunker;
mod engine;
pub mod index;
mod maint;
mod merge;
pub mod object;
mod restore;
mod store;

pub use engine::{SnapshotOptions, SnapshotReport, snapshot, take_snapshot};
pub use maint::{GcReport, VerifyReport, gc, verify};
pub use merge::{Change, ChangeKind, MergeResult, diff, merge3};
pub use object::{Id, Untracked};
pub use restore::{Authorize, RestoreReport, materialize, recover, restore};
pub use store::{Journal, Store, valid_ref};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("workspace: {0}")]
    Fs(#[from] anchor_fs::FsError),
    #[error("snapshot store i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot store is corrupt: {0}")]
    Corrupt(String),
    #[error("object {0} is missing from the snapshot store")]
    Missing(Id),
    #[error("invalid ref name {0:?}")]
    BadRef(String),
    #[error("the snapshot store is in use by another process")]
    Locked,
    #[error("an interrupted restore must be recovered first")]
    RecoveryPending,
    #[error("{0} changed during the restore; refusing to overwrite it")]
    ConcurrentChange(String),
    #[error("operation not approved: {0}")]
    Denied(String),
    #[error("restore verification failed: {}", .0.join("; "))]
    VerifyFailed(Vec<String>),
    #[error(
        "restore failed ({cause}); {}",
        if *reverted { "the workspace was returned to its pre-restore state" } else { "returning to the pre-restore state ALSO failed; it will be retried when the workspace is next opened" }
    )]
    RestoreFailed { cause: String, reverted: bool },
    #[error("the workspace is not empty")]
    NotEmpty,
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
