//! Isolated agent workspaces (spec §7, ADR-0007): each team agent works in
//! a private directory materialized from the workspace's snapshot store,
//! never in the main workspace. Its changes reach the main workspace only
//! through the merge queue.
//!
//! Layout: `<state_base>/iso/<workspace-hash>/<root>/`, owner-only,
//! outside both the workspace and its protected state directory. The
//! directory is a full copy of the base snapshot (gitignored files
//! included), so it is usable at once.

use std::path::{Path, PathBuf};

use anchor_fs::{Resolved, Workspace};
use anchor_policy::{Engine, Operation, PermissionRequest, Tool, UserAction};
use anchor_snapshot::{Id, Root, SnapshotOptions, Store};

/// Directory holding every isolated root of the workspace at `ws_root`.
pub fn iso_base(state_base: &Path, ws_root: &Path) -> PathBuf {
    let id = anchor_policy::audit::sha256_hex(ws_root.to_string_lossy().as_bytes());
    state_base.join("iso").join(&id[..16])
}

/// The store root (and directory name) of a Hub agent.
pub fn root_for(agent: anchor_hub::AgentId) -> Root {
    // `a<digits>` always satisfies Root's name rules.
    Root::named(&format!("a{agent}")).unwrap_or_default()
}

/// `r-<root>/iso-<leaf>`: the refs an isolated root keeps while its work
/// is pending (`base`, `submitted`, `quarantine`).
pub fn iso_ref(root: &Root, leaf: &str) -> String {
    root.ref_name("iso", leaf)
}

#[derive(Debug)]
pub struct Isolated {
    pub ws: Workspace,
    pub root: Root,
    pub opts: SnapshotOptions,
    /// The snapshot of the main workspace this copy started from.
    pub base: Id,
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

/// Create the isolated directory for `root` under `iso_base` and fill it
/// from `base`. A directory left by an earlier, interrupted run is first
/// preserved in the store (`r-<root>/iso-quarantine`) and then removed.
pub fn create(
    store: &Store,
    main_opts: &SnapshotOptions,
    engine: &Engine,
    iso_base: &Path,
    root: Root,
    base: &Id,
) -> Result<Isolated, String> {
    let name = root.name().ok_or("the main root is not isolated")?;
    let dir = iso_base.join(name);
    let mut opts = main_opts.clone();
    opts.root = root.clone();
    if dir.exists() {
        if let Ok(ws) = Workspace::open(&dir) {
            let label = format!("quarantined isolated workspace {name}");
            if let Ok(r) = anchor_snapshot::snapshot(store, &ws, &opts, &label) {
                store
                    .set_ref(&iso_ref(&root, "quarantine"), &r.id())
                    .map_err(|e| e.to_string())?;
            }
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    crate::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let ws = Workspace::open(&dir).map_err(|e| e.to_string())?;
    let ws_root = ws.root().to_path_buf();
    let mut auth = |r: &Resolved, op: Operation| {
        engine
            .user_initiated(UserAction::Merge, &user_request(&ws_root, r, op))
            .map_err(|d| d.rule)
    };
    anchor_snapshot::materialize(store, &ws, &opts, base, &mut auth).map_err(|e| e.to_string())?;
    store
        .set_ref(&iso_ref(&root, "base"), base)
        .map_err(|e| e.to_string())?;
    store
        .set_ref(&root.head_ref(), base)
        .map_err(|e| e.to_string())?;
    Ok(Isolated {
        ws,
        root,
        opts,
        base: *base,
    })
}

/// Snapshot the isolated root's final state as `r-<root>/iso-submitted`.
pub fn submit(store: &Store, iso: &Isolated) -> Result<Id, String> {
    let label = format!("submitted by {}", iso.root.name().unwrap_or("?"));
    let r = anchor_snapshot::take_snapshot(
        store,
        &iso.ws,
        &iso.opts,
        &iso_ref(&iso.root, "submitted"),
        &label,
    )
    .map_err(|e| e.to_string())?;
    Ok(r.id())
}

/// Remove an isolated root once its work is merged: every `r-<root>/*` ref
/// except a quarantine, then the directory.
pub fn teardown(store: &Store, root: &Root, dir: &Path) -> Result<(), String> {
    let Some(name) = root.name() else {
        return Err("the main root is not isolated".into());
    };
    let keep = iso_ref(root, "quarantine");
    for (r, _) in store
        .list_refs(&format!("r-{name}"))
        .map_err(|e| e.to_string())?
    {
        if r != keep {
            store.delete_ref(&r).map_err(|e| e.to_string())?;
        }
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}
