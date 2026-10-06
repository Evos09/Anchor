//! Anchor backend (Layer 1): the only component with OS authority.
//!
//! [`open_workspace`] wires together the safety primitives for one
//! workspace; [`session::run`] runs an agent session against it.

use std::path::{Path, PathBuf};

use anchor_checkpoint::CheckpointStore;
use anchor_fs::Workspace;
use anchor_guard::Guard;
use anchor_policy::{AuditLog, Engine};

pub mod interact;
pub mod mediator;
pub mod preimage;
pub mod session;

use interact::Interaction;
use mediator::Mediator;
use preimage::PreimageStore;

/// The policy shipped with Anchor; copied into a workspace on first use.
pub const DEFAULT_POLICY: &str = include_str!("../../../config/policy.default.toml");

/// Per-workspace state lives in `<workspace>/.anchor/` (never agent-writable:
/// the permission engine's capability floor denies it).
#[derive(Debug, Clone)]
pub struct StatePaths {
    pub dir: PathBuf,
    pub policy: PathBuf,
    pub audit: PathBuf,
    pub workspace_config: PathBuf,
    pub checkpoints: PathBuf,
    pub preimages: PathBuf,
}

impl StatePaths {
    pub fn for_root(root: &Path) -> Self {
        let dir = root.join(".anchor");
        StatePaths {
            policy: dir.join("policy.toml"),
            audit: dir.join("audit.jsonl"),
            workspace_config: dir.join("workspace.toml"),
            checkpoints: dir.join("checkpoints"),
            preimages: dir.join("preimages"),
            dir,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("workspace: {0}")]
    Fs(#[from] anchor_fs::FsError),
    #[error("state directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("permission engine: {0}")]
    Engine(#[from] anchor_policy::EngineError),
    #[error("audit log: {0}")]
    Audit(#[from] anchor_policy::AuditError),
    #[error("guard: {0}")]
    Guard(#[from] anchor_guard::GuardError),
    #[error("pre-image store: {0}")]
    Preimage(#[from] preimage::PreimageError),
    #[error("checkpoints: {0}")]
    Checkpoint(#[from] anchor_checkpoint::CheckpointError),
}

pub struct Opened {
    pub paths: StatePaths,
    pub mediator: Mediator,
    pub checkpoints: CheckpointStore,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("paths", &self.paths)
            .finish()
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nonexistent-home"))
}

/// Open a workspace: canonical root, state dir, policy (installing the
/// default if none exists), audit log, guard, pre-image and checkpoint
/// stores.
pub fn open_workspace(
    root: &Path,
    policy_override: Option<&Path>,
    interact: Box<dyn Interaction>,
    exec_read_only: Vec<PathBuf>,
) -> Result<Opened, OpenError> {
    let ws = Workspace::open(root)?;
    let paths = StatePaths::for_root(ws.root());
    std::fs::create_dir_all(&paths.dir)?;
    if policy_override.is_none() && !paths.policy.exists() {
        std::fs::write(&paths.policy, DEFAULT_POLICY)?;
    }
    let policy_path = policy_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.policy.clone());
    let audit = AuditLog::open(&paths.audit)?;
    let engine = Engine::from_file(&policy_path, &home_dir(), audit)?;
    let guard = Guard::load(&paths.workspace_config)?;
    let preimages = PreimageStore::open(&paths.preimages)?;
    let checkpoints = CheckpointStore::open(&paths.checkpoints)?;
    let path_var = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let mediator = Mediator::new(
        ws,
        engine,
        guard,
        preimages,
        interact,
        path_var,
        exec_read_only,
    );
    Ok(Opened {
        paths,
        mediator,
        checkpoints,
    })
}

/// Default read-only view for agent commands: system directories plus every
/// absolute directory on PATH (so toolchains can execute).
pub fn default_exec_read_only() -> Vec<PathBuf> {
    let mut v = anchor_exec::FsConfinement::system_read_only();
    if let Some(p) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&p) {
            // Never grant a PATH entry that would expose `/` or the whole
            // home directory.
            if d.is_absolute()
                && d.exists()
                && d != Path::new("/")
                && d != home_dir()
                && !v.contains(&d)
            {
                v.push(d);
            }
        }
    }
    v
}
