//! Anchor backend (Layer 1): the only component with OS authority.
//!
//! [`open_workspace`] wires together the safety primitives for one
//! workspace; [`session::run`] runs an agent session against it.

use std::path::{Path, PathBuf};

use anchor_checkpoint::CheckpointStore;
use anchor_fs::Workspace;
use anchor_guard::Guard;
use anchor_policy::{AuditLog, Engine};

pub mod eval;
pub mod interact;
pub mod iso;
pub mod mediator;
pub mod queue;
pub mod session;
pub mod team;
pub mod ui;
pub mod verify;

use interact::Interaction;
use mediator::Mediator;

/// The policy shipped with Anchor; copied into a workspace on first use.
pub const DEFAULT_POLICY: &str = include_str!("../../../config/policy.default.toml");

/// Per-workspace backend state: policy, audit log, checkpoints, the
/// durability store. It lives **outside** the workspace (ADR-0022): agent
/// commands get read-write access to the whole workspace, and a sandbox
/// cannot carve a subtree out of a granted path, so state inside the
/// workspace would be writable by any approved command.
#[derive(Debug, Clone)]
pub struct StatePaths {
    pub dir: PathBuf,
    pub policy: PathBuf,
    pub audit: PathBuf,
    pub workspace_config: PathBuf,
    pub checkpoints: PathBuf,
    pub snapshots: PathBuf,
}

/// Environment override for the state base directory.
pub const STATE_DIR_ENV: &str = "ANCHOR_STATE_DIR";

/// The base directory holding every workspace's state:
/// `$ANCHOR_STATE_DIR`, else the platform's per-user state directory.
pub fn default_state_base() -> PathBuf {
    if let Some(d) = std::env::var_os(STATE_DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = home_dir();
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("anchor")
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support/anchor")
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/state"))
            .join("anchor")
    }
}

impl StatePaths {
    /// State paths for the workspace with canonical root `root`, under
    /// `base` (see [`default_state_base`]). The directory name is derived
    /// from the canonical root, so each workspace gets its own.
    pub fn for_workspace(root: &Path, base: &Path) -> Self {
        let id = anchor_policy::audit::sha256_hex(root.to_string_lossy().as_bytes());
        Self::in_dir(base.join("ws").join(&id[..16]))
    }

    fn in_dir(dir: PathBuf) -> Self {
        StatePaths {
            policy: dir.join("policy.toml"),
            audit: dir.join("audit.jsonl"),
            workspace_config: dir.join("workspace.toml"),
            checkpoints: dir.join("checkpoints"),
            snapshots: dir.join("snapshots"),
            dir,
        }
    }

    /// Create the state directory (owner-only on Unix), canonicalise it,
    /// and refuse layouts where the workspace and the state overlap.
    pub fn create(root: &Path, base: &Path) -> Result<Self, OpenError> {
        let wanted = Self::for_workspace(root, base);
        create_private_dir(&wanted.dir)?;
        let dir = anchor_fs::normalize_verbatim(&std::fs::canonicalize(&wanted.dir)?);
        if dir.starts_with(root) || root.starts_with(&dir) {
            return Err(OpenError::StateOverlap {
                state: dir,
                workspace: root.to_path_buf(),
            });
        }
        let paths = Self::in_dir(dir);
        // Record which workspace this directory belongs to (for humans).
        let marker = paths.dir.join("workspace-root");
        if !marker.exists() {
            std::fs::write(&marker, format!("{}\n", root.display()))?;
        }
        Ok(paths)
    }
}

pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let mode = std::fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
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
    #[error("snapshot store: {0}")]
    Snapshot(#[from] anchor_snapshot::Error),
    #[error("workspace.toml [snapshot]: {0}")]
    SnapshotConfig(String),
    #[error("an interrupted restore could not be recovered; refusing to open: {0}")]
    Recovery(String),
    #[error("checkpoints: {0}")]
    Checkpoint(#[from] anchor_checkpoint::CheckpointError),
    #[error(
        "state directory {state} overlaps the workspace {workspace}; set {STATE_DIR_ENV} to a directory outside it"
    )]
    StateOverlap { state: PathBuf, workspace: PathBuf },
}

pub struct Opened {
    pub paths: StatePaths,
    pub mediator: Mediator,
    pub checkpoints: CheckpointStore,
    /// Set if opening finished an interrupted restore.
    pub recovered: Option<anchor_snapshot::RestoreReport>,
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

/// Open a workspace with the default state location: canonical root, state
/// directory, policy (installing the default if none exists), audit log,
/// guard, snapshot and checkpoint stores. Finishes any interrupted restore
/// first.
pub fn open_workspace(
    root: &Path,
    policy_override: Option<&Path>,
    interact: Box<dyn Interaction>,
    exec_read_only: Vec<PathBuf>,
) -> Result<Opened, OpenError> {
    open_workspace_in(
        root,
        &default_state_base(),
        policy_override,
        interact,
        exec_read_only,
    )
}

/// [`open_workspace`] with an explicit state base directory.
pub fn open_workspace_in(
    root: &Path,
    state_base: &Path,
    policy_override: Option<&Path>,
    interact: Box<dyn Interaction>,
    exec_read_only: Vec<PathBuf>,
) -> Result<Opened, OpenError> {
    let ws = Workspace::open(root)?;
    let paths = StatePaths::create(ws.root(), state_base)?;
    if ws.root().join(".anchor").exists() {
        eprintln!(
            "anchor: note: {} is from an older layout and is no longer used; state now lives in {}",
            ws.root().join(".anchor").display(),
            paths.dir.display()
        );
    }
    if policy_override.is_none() && !paths.policy.exists() {
        std::fs::write(&paths.policy, DEFAULT_POLICY)?;
    }
    let policy_path = policy_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.policy.clone());
    let audit = AuditLog::open(&paths.audit)?;
    let mut engine = Engine::from_file(&policy_path, &home_dir(), audit)?;
    engine.protect(paths.dir.clone());
    let guard = Guard::load(&paths.workspace_config)?;
    let snap = SnapshotConfig::load(&paths.workspace_config, &ws)?;
    let snapshots = anchor_snapshot::Store::open(&paths.snapshots)?;
    let checkpoints = CheckpointStore::open(&paths.checkpoints)?;
    let path_var = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let exec_read_only = safe_read_grants(exec_read_only, &paths.dir);
    let mut mediator = Mediator::new(
        ws,
        engine,
        guard,
        std::sync::Arc::new(std::sync::Mutex::new(snapshots)),
        snap.options,
        snap.keep_turns,
        interact,
        path_var,
        exec_read_only,
    );
    // A restore killed part-way must be undone before anything else
    // touches the workspace (spec §5.5(d)).
    let recovered = mediator.recover().map_err(OpenError::Recovery)?;
    Ok(Opened {
        paths,
        mediator,
        checkpoints,
        recovered,
    })
}

/// `[snapshot]` in `workspace.toml`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotSection {
    /// Workspace-relative paths left out of snapshots (and so never
    /// restored).
    #[serde(default)]
    exclude: Vec<String>,
    max_files: Option<usize>,
    max_file_mb: Option<u64>,
    keep_turns: Option<usize>,
}

struct SnapshotConfig {
    options: anchor_snapshot::SnapshotOptions,
    keep_turns: usize,
}

impl SnapshotConfig {
    fn load(path: &Path, ws: &Workspace) -> Result<Self, OpenError> {
        let bad = |m: String| OpenError::SnapshotConfig(m);
        let section: SnapshotSection = match std::fs::read_to_string(path) {
            Ok(text) => {
                let table: toml::Table = toml::from_str(&text).map_err(|e| bad(e.to_string()))?;
                match table.get("snapshot") {
                    Some(v) => v.clone().try_into().map_err(|e| bad(format!("{e}")))?,
                    None => SnapshotSection::default(),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SnapshotSection::default(),
            Err(e) => return Err(e.into()),
        };
        let mut options = anchor_snapshot::SnapshotOptions::default();
        for e in &section.exclude {
            let rel = ws
                .confine(e)
                .map_err(|err| bad(format!("exclude {e:?}: {err}")))?;
            if rel.as_os_str().is_empty() {
                return Err(bad("exclude must not be the whole workspace".into()));
            }
            options.exclude.push(rel);
        }
        if let Some(n) = section.max_files {
            options.max_entries = n;
        }
        if let Some(mb) = section.max_file_mb {
            options.max_file_bytes = mb << 20;
        }
        Ok(SnapshotConfig {
            options,
            keep_turns: section.keep_turns.unwrap_or(200).max(1),
        })
    }
}

/// Filter read-only grants for agent commands. Each is canonicalized (so
/// `~/bin/..`, a symlink to home or a parent such as `/home` is seen for
/// what it is) and dropped, with a warning, if it is `/`, contains the
/// home directory, or overlaps the state directory.
pub fn safe_read_grants(grants: Vec<PathBuf>, state_dir: &Path) -> Vec<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let home = canon(&home_dir());
    let state = canon(state_dir);
    let mut out: Vec<PathBuf> = Vec::new();
    for g in grants {
        let Ok(c) = std::fs::canonicalize(&g) else {
            continue; // missing: nothing to grant
        };
        let why = if c.parent().is_none() {
            Some("the filesystem root")
        } else if home.starts_with(&c) {
            Some("the home directory or one of its parents")
        } else if state.starts_with(&c) || c.starts_with(&state) {
            Some("Anchor's state directory")
        } else {
            None
        };
        match why {
            Some(w) => eprintln!(
                "anchor: not granting {} to agent commands: it would expose {w}",
                g.display()
            ),
            None if !out.contains(&c) => out.push(c),
            None => {}
        }
    }
    out
}

/// Default read-only view for agent commands: system directories plus every
/// absolute directory on PATH (so toolchains can execute). Filtered again
/// by [`safe_read_grants`] when a workspace opens.
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

#[cfg(all(test, unix))]
mod grant_tests {
    use super::*;

    /// Review finding S-8: `~/bin/..`, a parent of home or a symlink to it
    /// must not slip through as a read-only grant.
    #[test]
    fn grants_exposing_home_root_or_state_are_dropped() {
        let home = std::fs::canonicalize(home_dir()).unwrap();
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        let state = base.join("state");
        let tool = base.join("toolchain");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&tool).unwrap();
        let link = base.join("home-link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let grants = vec![
            PathBuf::from("/"),
            home.join("bin/.."),
            home.clone(),
            link,
            home.parent().unwrap().to_path_buf(),
            state.clone(),
            state.join("sub"),
            tool.clone(),
            tool.join("."),
        ];
        assert_eq!(safe_read_grants(grants, &state), vec![tool]);
    }
}
