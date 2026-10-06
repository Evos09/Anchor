//! Merge-queue substrate (spec §5.5(f)): diffs between snapshots and a
//! path-level three-way merge. Phase 4's merge queue builds on these; there
//! is no line-level merge yet, so two different edits to one file are a
//! conflict.

use std::path::{Path, PathBuf};

use crate::engine::name_os;
use crate::object::{Id, Node, ObjType, SnapshotObj, TreeEntry, TreeObj};
use crate::restore::untouchable;
use crate::store::{Batch, Store, unix_ms};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    ModeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let k = match self.kind {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "modified",
            ChangeKind::ModeChanged => "mode changed",
        };
        write!(f, "{k}: {}", self.path.display())
    }
}

fn name_path(parent: &Path, name: &[u8]) -> Result<PathBuf> {
    Ok(parent.join(name_os(name).ok_or_else(|| Error::Corrupt("unrepresentable name".into()))?))
}

/// Changes from tree `a` to tree `b`. With `skip_untouchable`, entries a
/// restore leaves alone are ignored on either side.
pub fn diff_trees(
    store: &Store,
    a: &Id,
    b: &Id,
    prefix: PathBuf,
    skip_untouchable: bool,
    out: &mut Vec<Change>,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let (ta, tb) = (store.tree(a)?, store.tree(b)?);
    diff_objs(store, &ta, &tb, &prefix, skip_untouchable, out)
}

fn diff_objs(
    store: &Store,
    ta: &TreeObj,
    tb: &TreeObj,
    prefix: &Path,
    skip: bool,
    out: &mut Vec<Change>,
) -> Result<()> {
    let mut names: Vec<&[u8]> = ta
        .entries
        .iter()
        .chain(tb.entries.iter())
        .map(|e| e.name.as_slice())
        .collect();
    names.sort();
    names.dedup();
    for n in names {
        let (x, y) = (ta.get(n), tb.get(n));
        if skip && (x.is_some_and(untouchable) || y.is_some_and(untouchable)) {
            continue;
        }
        let path = name_path(prefix, n)?;
        let kind = match (x, y) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(Node::Dir { id: i, mode: m }), Some(Node::Dir { id: j, mode: k })) => {
                if i != j {
                    diff_trees(store, i, j, path.clone(), skip, out)?;
                }
                if m == k {
                    continue;
                }
                ChangeKind::ModeChanged
            }
            (Some(Node::File { id: i, .. }), Some(Node::File { id: j, .. })) if i == j => {
                ChangeKind::ModeChanged
            }
            (Some(_), Some(_)) => ChangeKind::Modified,
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Removed,
            (None, None) => continue,
        };
        out.push(Change { path, kind });
    }
    Ok(())
}

/// Changes from snapshot `a` to snapshot `b`.
pub fn diff(store: &Store, a: &Id, b: &Id) -> Result<Vec<Change>> {
    let (ra, rb) = (store.snapshot(a)?.root, store.snapshot(b)?.root);
    let mut out = Vec::new();
    diff_trees(store, &ra, &rb, PathBuf::new(), false, &mut out)?;
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// The merged snapshot (published; not yet named by a ref). Where a
    /// path conflicts, it holds `ours`.
    pub snapshot: Id,
    pub conflicts: Vec<PathBuf>,
}

/// Path-level three-way merge of snapshots `ours` and `theirs` against
/// their common `base`.
pub fn merge3(
    store: &Store,
    base: &Id,
    ours: &Id,
    theirs: &Id,
    label: &str,
) -> Result<MergeResult> {
    let rb = store.snapshot(base)?.root;
    let so = store.snapshot(ours)?;
    let rt = store.snapshot(theirs)?.root;
    let batch = store.batch();
    let mut conflicts = Vec::new();
    let tb = store.tree(&rb)?;
    let to = store.tree(&so.root)?;
    let tt = store.tree(&rt)?;
    let root = merge_trees(store, &batch, &tb, &to, &tt, Path::new(""), &mut conflicts)?;
    let snap = SnapshotObj {
        root,
        created_unix_ms: unix_ms(),
        files: 0,
        bytes: 0,
        label: label.chars().take(64).collect(),
    };
    let id = batch.put(ObjType::Snapshot, &snap.encode())?;
    batch.publish()?;
    Ok(MergeResult {
        snapshot: id,
        conflicts,
    })
}

fn merge_trees(
    store: &Store,
    batch: &Batch<'_>,
    b: &TreeObj,
    o: &TreeObj,
    t: &TreeObj,
    prefix: &Path,
    conflicts: &mut Vec<PathBuf>,
) -> Result<Id> {
    let mut names: Vec<&[u8]> = b
        .entries
        .iter()
        .chain(o.entries.iter())
        .chain(t.entries.iter())
        .map(|e| e.name.as_slice())
        .collect();
    names.sort();
    names.dedup();
    let mut entries = Vec::new();
    for n in names {
        let (nb, no, nt) = (b.get(n), o.get(n), t.get(n));
        let pick: Option<Node> = if no == nt {
            no.cloned()
        } else if no == nb {
            nt.cloned()
        } else if nt == nb {
            no.cloned()
        } else {
            match (no, nt) {
                (Some(Node::Dir { id: io, mode: mo }), Some(Node::Dir { id: it, mode: mt })) => {
                    let base_tree = match nb {
                        Some(Node::Dir { id, .. }) => store.tree(id)?,
                        _ => TreeObj::default(),
                    };
                    let mode = if mo == mt {
                        *mo
                    } else {
                        let bm = match nb {
                            Some(Node::Dir { mode, .. }) => Some(*mode),
                            _ => None,
                        };
                        if Some(*mo) == bm { *mt } else { *mo }
                    };
                    let id = merge_trees(
                        store,
                        batch,
                        &base_tree,
                        &store.tree(io)?,
                        &store.tree(it)?,
                        &name_path(prefix, n)?,
                        conflicts,
                    )?;
                    Some(Node::Dir { id, mode })
                }
                _ => {
                    conflicts.push(name_path(prefix, n)?);
                    no.cloned()
                }
            }
        };
        if let Some(node) = pick {
            entries.push(TreeEntry {
                name: n.to_vec(),
                node,
            });
        }
    }
    batch.put(ObjType::Tree, &TreeObj { entries }.encode())
}
