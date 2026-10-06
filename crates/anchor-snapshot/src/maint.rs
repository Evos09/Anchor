//! Store maintenance: full verification (fsck) and garbage collection.

use std::collections::HashSet;

use crate::object::{Id, Node};
use crate::store::Store;
use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub refs: usize,
    pub objects: usize,
    pub errors: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Snapshot ids the store must keep: every ref plus an in-flight journal.
fn roots(store: &Store) -> Result<Vec<(String, Id)>> {
    let mut v = store.all_refs()?;
    if let Some(j) = store.journal()? {
        for (n, h) in [("journal.pre", &j.pre), ("journal.target", &j.target)] {
            if let Some(id) = Id::parse(h) {
                v.push((n.to_string(), id));
            }
        }
    }
    Ok(v)
}

/// Walk everything reachable from `snap`. With `chunks`, read and verify
/// every chunk too (otherwise only their ids are collected).
fn walk(store: &Store, snap: &Id, chunks: bool, seen: &mut HashSet<Id>, errors: &mut Vec<String>) {
    if !seen.insert(*snap) {
        return;
    }
    let root = match store.snapshot(snap) {
        Ok(s) => s.root,
        Err(e) => return errors.push(format!("snapshot {snap}: {e}")),
    };
    let mut stack = vec![root];
    while let Some(t) = stack.pop() {
        if !seen.insert(t) {
            continue;
        }
        let tree = match store.tree(&t) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("tree {t}: {e}"));
                continue;
            }
        };
        for e in tree.entries {
            match e.node {
                Node::Dir { id, .. } => stack.push(id),
                Node::File { id, .. } => {
                    if !seen.insert(id) {
                        continue;
                    }
                    match store.file(&id) {
                        Ok(f) => {
                            for (c, len) in f.chunks {
                                if !seen.insert(c) || !chunks {
                                    continue;
                                }
                                match store.chunk(&c) {
                                    Ok(b) if b.len() == len as usize => {}
                                    Ok(_) => errors.push(format!("chunk {c}: wrong length")),
                                    Err(e) => errors.push(format!("chunk {c}: {e}")),
                                }
                            }
                        }
                        Err(e) => errors.push(format!("file {id}: {e}")),
                    }
                }
                Node::Symlink { .. } | Node::Untracked(_) => {}
            }
        }
    }
}

/// Verify every object reachable from every ref (hash, type, encoding,
/// names, chunk lengths).
pub fn verify(store: &Store) -> Result<VerifyReport> {
    let roots = roots(store)?;
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    for (name, id) in &roots {
        let before = errors.len();
        walk(store, id, true, &mut seen, &mut errors);
        for e in &mut errors[before..] {
            *e = format!("{name}: {e}");
        }
    }
    Ok(VerifyReport {
        refs: roots.len(),
        objects: seen.len(),
        errors,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub refs_pruned: usize,
    pub objects_removed: usize,
    pub bytes_freed: u64,
}

/// Keep the newest `keep_turns` turn refs and `keep_restores` pre-restore
/// refs, then delete every object they (and `HEAD` and any journal) do not
/// reach. Refuses if the reachable set cannot be read completely.
pub fn gc(store: &Store, keep_turns: usize, keep_restores: usize) -> Result<GcReport> {
    let mut report = GcReport::default();
    for (ns, keep) in [("turns", keep_turns), ("restore", keep_restores)] {
        let refs = store.list_refs(ns)?;
        if refs.len() > keep {
            for (name, _) in &refs[..refs.len() - keep] {
                store.delete_ref(name)?;
                report.refs_pruned += 1;
            }
        }
    }
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    for (_, id) in roots(store)? {
        walk(store, &id, false, &mut seen, &mut errors);
    }
    if !errors.is_empty() {
        return Err(Error::Corrupt(format!(
            "refusing to collect garbage while the store is damaged: {}",
            errors.join("; ")
        )));
    }
    for (id, path) in store.all_objects()? {
        if !seen.contains(&id) {
            let len = std::fs::symlink_metadata(&path)
                .map(|m| m.len())
                .unwrap_or(0);
            std::fs::remove_file(&path)?;
            report.objects_removed += 1;
            report.bytes_freed += len;
        }
    }
    // The index may name collected file objects; it is only a cache.
    let _ = std::fs::remove_file(store.index_path());
    Ok(report)
}
