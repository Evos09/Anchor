//! Restore (spec §5.5(d)): make the workspace equal a snapshot, atomically
//! as a whole.
//!
//! 1. Snapshot the current state (`pre`). It is kept under `restore/<ms>`,
//!    so a restore can itself be undone.
//! 2. Write the journal `{pre, target}` durably.
//! 3. Walk both trees and apply the difference, one atomic, approval-gated
//!    `anchor-fs` operation at a time. Each operation first checks the path
//!    still holds exactly what `pre` recorded (compare-and-swap), so a file
//!    changed by someone else meanwhile is never clobbered.
//! 4. Verify: snapshot again; it must equal the target.
//! 5. Point the root's head ref at the result and delete the journal.
//!
//! Journals, head refs and `restore` refs belong to the root being
//! restored ([`crate::Root`], set in [`SnapshotOptions::root`]).
//!
//! If any step fails, the same machinery restores `pre`. If the process
//! dies, the journal remains and [`recover`] (run before anything else uses
//! the workspace) restores `pre`. Because every pass is computed from the
//! current state, applying a pass again after an interruption is safe.

use std::io::Read;
use std::path::{Path, PathBuf};

use anchor_fs::{Expect, FsError, Kind, MAX_READ_BYTES, Resolved, Workspace};
use anchor_policy::{Approved, Operation};
use sha2::{Digest, Sha256};

use crate::engine::{SnapshotOptions, name_os, snapshot};
use crate::merge::{Change, diff_trees};
use crate::object::{Id, Node, TreeObj};
use crate::store::{Store, unix_ms};
use crate::{Error, Result};

/// Approves one restore operation on a resolved target (the backend asks
/// its permission engine for a user-initiated approval).
pub type Authorize<'a> =
    dyn FnMut(&Resolved, Operation) -> std::result::Result<Approved, String> + 'a;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Snapshot of the workspace before the restore (`restore/<ms>` ref).
    pub pre: Option<Id>,
    pub target: Option<Id>,
    /// Snapshot of the workspace after the restore (equals the target).
    pub post: Option<Id>,
    /// One line per filesystem operation performed.
    pub ops: Vec<String>,
    /// Paths deliberately left alone (untracked entries).
    pub skipped: Vec<String>,
}

struct Ctx<'s, 'a, 'b> {
    store: &'s Store,
    ws: &'s Workspace,
    auth: &'b mut Authorize<'a>,
    report: &'b mut RestoreReport,
    /// Test hook: abort the process after this many operations.
    abort_after: Option<u64>,
}

/// Symlinks cannot be created on Windows without elevation; there they are
/// recorded but treated like untracked entries.
pub(crate) fn untouchable(n: &Node) -> bool {
    matches!(n, Node::Untracked(_)) || (cfg!(windows) && matches!(n, Node::Symlink { .. }))
}

impl Ctx<'_, '_, '_> {
    fn approve(&mut self, r: &Resolved, op: Operation) -> Result<Approved> {
        (self.auth)(r, op).map_err(Error::Denied)
    }

    fn tick(&mut self, line: String) {
        self.report.ops.push(line);
        let n = self.report.ops.len() as u64;
        if self.abort_after.is_some_and(|k| n >= k) {
            // Test hook: die between two restore operations.
            std::process::abort();
        }
    }

    fn resolve(&self, rel: &Path) -> Result<Resolved> {
        Ok(self.ws.resolve_exact(rel)?)
    }

    fn apply_dir(&mut self, rel: &Path, cur: &TreeObj, tgt: &TreeObj) -> Result<()> {
        let mut names: Vec<&[u8]> = cur
            .entries
            .iter()
            .chain(tgt.entries.iter())
            .map(|e| e.name.as_slice())
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            let os = name_os(name).ok_or_else(|| Error::Corrupt("unrepresentable name".into()))?;
            let path = rel.join(&os);
            let (c, t) = (cur.get(name), tgt.get(name));
            // A tracked node in the target replaces whatever untracked entry
            // (hardlink, FIFO, oversized file) an agent left at its name:
            // skipping it would leave the data unrecoverable while the
            // restore reported success (review finding P-2).
            if let (Some(Node::Untracked(_)), Some(tn)) = (c, t)
                && !untouchable(tn)
            {
                self.replace_untracked(&path, tn)?;
                continue;
            }
            if c.is_some_and(untouchable) || t.is_some_and(untouchable) {
                if c != t {
                    self.report.skipped.push(path.display().to_string());
                }
                continue;
            }
            match (c, t) {
                (Some(x), Some(y)) if x == y => {}
                (Some(Node::Dir { id: a, mode: ma }), Some(Node::Dir { id: b, mode: mb })) => {
                    if a != b {
                        let (ta, tb) = (self.store.tree(a)?, self.store.tree(b)?);
                        self.apply_dir(&path, &ta, &tb)?;
                    }
                    if ma != mb {
                        self.set_mode(&path, *mb)?;
                    }
                }
                (Some(Node::File { id: a, .. }), Some(Node::File { id: b, mode })) => {
                    if a == b {
                        self.set_mode(&path, *mode)?;
                    } else {
                        self.overwrite(&path, a, b, *mode)?;
                    }
                }
                (Some(c), t) => {
                    self.remove_node(&path, c)?;
                    if let Some(t) = t {
                        self.create_node(&path, t)?;
                    }
                }
                (None, Some(t)) => self.create_node(&path, t)?,
                (None, None) => {}
            }
        }
        Ok(())
    }

    fn set_mode(&mut self, rel: &Path, mode: u32) -> Result<()> {
        if cfg!(windows) {
            return Ok(());
        }
        let r = self.resolve(rel)?;
        let m = r.meta.ok_or(FsError::NotFound)?;
        if m.mode & 0o777 == mode {
            return Ok(());
        }
        let a = self.approve(&r, Operation::Write)?;
        self.ws.set_mode(&r, mode, a)?;
        self.tick(format!("mode {:o} {}", mode, r.canonical.display()));
        Ok(())
    }

    /// The current file at `r` must have exactly the content `sha256`.
    fn check_content(&self, rel: &Path, r: &Resolved, sha256: &[u8; 32]) -> Result<()> {
        let m = r.meta.ok_or(FsError::NotFound)?;
        if m.kind != Kind::File {
            return Err(FsError::Changed.into());
        }
        let mut f = self.ws.open_scanned(rel, &m)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        f.finish()?;
        if <[u8; 32]>::from(h.finalize()) != *sha256 {
            return Err(Error::ConcurrentChange(rel.display().to_string()));
        }
        Ok(())
    }

    fn overwrite(&mut self, rel: &Path, cur: &Id, tgt: &Id, mode: u32) -> Result<()> {
        let fc = self.store.file(cur)?;
        let ft = self.store.file(tgt)?;
        let content = self.store.file_content(&ft)?;
        let r = self.resolve(rel)?;
        let m = r.meta.ok_or(FsError::NotFound)?;
        if m.kind != Kind::File {
            return Err(Error::ConcurrentChange(rel.display().to_string()));
        }
        if m.len > MAX_READ_BYTES {
            // Too large for the in-place compare-and-swap: verify, remove,
            // create.
            self.check_content(rel, &r, &fc.sha256)?;
            let a = self.approve(&r, Operation::Delete)?;
            self.ws.remove(&r, a)?;
            let r = self.resolve(rel)?;
            let a = self.approve(&r, Operation::Create)?;
            self.ws
                .write_with_mode(&r, &content, Expect::Absent, Some(mode), a)?;
        } else {
            let a = self.approve(&r, Operation::Write)?;
            self.ws
                .write_with_mode(
                    &r,
                    &content,
                    Expect::Existing {
                        id: m.id,
                        sha256: fc.sha256,
                    },
                    Some(mode),
                    a,
                )
                .map_err(|e| match e {
                    FsError::ContentMismatch => Error::ConcurrentChange(rel.display().to_string()),
                    e => e.into(),
                })?;
        }
        self.tick(format!("restored {}", r.canonical.display()));
        Ok(())
    }

    /// Unlink the untracked entry at `rel` (never following or opening it;
    /// identity-checked) and create `node` in its place. An untracked
    /// directory is left alone and reported.
    fn replace_untracked(&mut self, rel: &Path, node: &Node) -> Result<()> {
        let r = self.resolve(rel)?;
        let Some(m) = r.meta else {
            return self.create_node(rel, node);
        };
        if m.kind == Kind::Dir {
            self.report.skipped.push(rel.display().to_string());
            return Ok(());
        }
        let a = self.approve(&r, Operation::Delete)?;
        self.ws.remove(&r, a)?;
        self.tick(format!("removed untracked {}", r.canonical.display()));
        self.create_node(rel, node)
    }

    fn remove_node(&mut self, rel: &Path, node: &Node) -> Result<()> {
        match node {
            Node::File { id, .. } => {
                let f = self.store.file(id)?;
                let r = self.resolve(rel)?;
                self.check_content(rel, &r, &f.sha256)?;
                let a = self.approve(&r, Operation::Delete)?;
                self.ws.remove(&r, a)?;
                self.tick(format!("removed {}", r.canonical.display()));
            }
            Node::Symlink { target } => {
                let r = self.resolve(rel)?;
                let m = r.meta.ok_or(FsError::NotFound)?;
                if m.kind != Kind::Symlink
                    || crate::engine::name_bytes(&self.ws.read_link_scanned(rel, &m)?).as_ref()
                        != Some(target)
                {
                    return Err(Error::ConcurrentChange(rel.display().to_string()));
                }
                let a = self.approve(&r, Operation::Delete)?;
                self.ws.remove(&r, a)?;
                self.tick(format!("removed link {}", r.canonical.display()));
            }
            Node::Dir { id, .. } => {
                let t = self.store.tree(id)?;
                for e in t.entries.iter().rev() {
                    if untouchable(&e.node) {
                        return Err(Error::Corrupt(format!(
                            "cannot remove {}: it contains an untracked entry",
                            rel.display()
                        )));
                    }
                    let os = name_os(&e.name)
                        .ok_or_else(|| Error::Corrupt("unrepresentable name".into()))?;
                    self.remove_node(&rel.join(os), &e.node)?;
                }
                let r = self.resolve(rel)?;
                if r.meta.map(|m| m.kind) != Some(Kind::Dir) {
                    return Err(Error::ConcurrentChange(rel.display().to_string()));
                }
                let a = self.approve(&r, Operation::Delete)?;
                self.ws.remove_dir(&r, a)?;
                self.tick(format!("removed dir {}", r.canonical.display()));
            }
            Node::Untracked(_) => {}
        }
        Ok(())
    }

    fn create_node(&mut self, rel: &Path, node: &Node) -> Result<()> {
        let r = self.resolve(rel)?;
        if r.exists() {
            return Err(Error::ConcurrentChange(rel.display().to_string()));
        }
        match node {
            Node::File { id, mode } => {
                let content = self.store.file_content(&self.store.file(id)?)?;
                let a = self.approve(&r, Operation::Create)?;
                self.ws
                    .write_with_mode(&r, &content, Expect::Absent, Some(*mode), a)?;
                self.tick(format!("created {}", r.canonical.display()));
            }
            Node::Symlink { target } => {
                let t = name_os(target).ok_or_else(|| Error::Corrupt("bad link target".into()))?;
                let a = self.approve(&r, Operation::Create)?;
                self.ws.create_symlink(&r, &t, a)?;
                self.tick(format!("created link {}", r.canonical.display()));
            }
            Node::Dir { id, mode } => {
                let a = self.approve(&r, Operation::Create)?;
                // Owner-writable while it is filled; final mode afterwards.
                self.ws.create_dir(&r, 0o700, a)?;
                self.tick(format!("created dir {}", r.canonical.display()));
                let t = self.store.tree(id)?;
                self.apply_dir(rel, &TreeObj::default(), &t)?;
                self.set_mode(rel, *mode)?;
            }
            Node::Untracked(_) => {}
        }
        Ok(())
    }
}

fn root_of(store: &Store, snap: &Id) -> Result<TreeObj> {
    store.tree(&store.snapshot(snap)?.root)
}

/// Bring the workspace from the state captured in `from` to `to`.
fn apply(
    store: &Store,
    ws: &Workspace,
    from: &Id,
    to: &Id,
    auth: &mut Authorize<'_>,
    report: &mut RestoreReport,
    abort_after: Option<u64>,
) -> Result<()> {
    let cur = root_of(store, from)?;
    let tgt = root_of(store, to)?;
    let mut ctx = Ctx {
        store,
        ws,
        auth,
        report,
        abort_after,
    };
    ctx.apply_dir(Path::new(""), &cur, &tgt)
}

/// Differences between the workspace now and snapshot `want`, ignoring
/// untouchable entries. Empty means verified.
fn remaining(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    want: &Id,
) -> Result<(Id, Vec<Change>)> {
    let now = snapshot(store, ws, opts, "verify")?;
    store.set_ref(&opts.root.head_ref(), &now.id())?;
    let a = store.snapshot(&now.id())?.root;
    let b = store.snapshot(want)?.root;
    let mut out = Vec::new();
    diff_trees(store, &a, &b, PathBuf::new(), true, &mut out)?;
    Ok((now.id(), out))
}

/// Apply `from` → `to`, then verify. On failure, return to `from`. The
/// journal must already be written.
#[allow(clippy::too_many_arguments)]
fn run_with_fallback(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    from: &Id,
    to: &Id,
    auth: &mut Authorize<'_>,
    report: &mut RestoreReport,
    abort_after: Option<u64>,
) -> Result<Id> {
    let attempt = apply(store, ws, from, to, auth, report, abort_after).and_then(|()| {
        let (post, diff) = remaining(store, ws, opts, to)?;
        if diff.is_empty() {
            Ok(post)
        } else {
            Err(Error::VerifyFailed(
                diff.iter().take(5).map(|c| c.to_string()).collect(),
            ))
        }
    });
    match attempt {
        Ok(post) => Ok(post),
        Err(cause) => {
            // Reverse from wherever we got to.
            let back_result = snapshot(store, ws, opts, "restore-failed").and_then(|mid| {
                // Keep what the workspace held at the failure, so
                // nothing written meanwhile by anyone is lost.
                store.set_ref(
                    &opts.root.ref_name("restore", &format!("{}-mid", unix_ms())),
                    &mid.id(),
                )?;
                let mut rev = RestoreReport::default();
                apply(store, ws, &mid.id(), from, auth, &mut rev, None)?;
                report
                    .ops
                    .extend(rev.ops.into_iter().map(|o| format!("(undo) {o}")));
                remaining(store, ws, opts, from)
            });
            match back_result {
                Ok((_, d)) if d.is_empty() => {
                    store.clear_journal(&opts.root)?;
                    Err(Error::RestoreFailed {
                        cause: cause.to_string(),
                        reverted: true,
                    })
                }
                _ => Err(Error::RestoreFailed {
                    cause: cause.to_string(),
                    reverted: false,
                }),
            }
        }
    }
}

enum Preflight {
    Ok,
    TargetDamaged(Error),
    PreDamaged(Error),
}

/// Read (and so hash-verify) every object a restore from `pre` to `target`
/// needs: trees on both sides, the target's new content, and the pre-state
/// content of every file it replaces or removes.
fn preflight(store: &Store, pre: &Id, target: &Id) -> Result<Preflight> {
    let t = match root_of(store, target) {
        Ok(t) => t,
        Err(e) => return Ok(Preflight::TargetDamaged(e)),
    };
    let p = match root_of(store, pre) {
        Ok(p) => p,
        Err(e) => return Ok(Preflight::PreDamaged(e)),
    };
    let mut out = Preflight::Ok;
    preflight_dir(store, Some(&p), Some(&t), &mut out);
    Ok(out)
}

fn preflight_dir(store: &Store, cur: Option<&TreeObj>, tgt: Option<&TreeObj>, out: &mut Preflight) {
    let empty = TreeObj::default();
    let (cur, tgt) = (cur.unwrap_or(&empty), tgt.unwrap_or(&empty));
    let mut names: Vec<&[u8]> = cur
        .entries
        .iter()
        .chain(tgt.entries.iter())
        .map(|e| e.name.as_slice())
        .collect();
    names.sort();
    names.dedup();
    for name in names {
        if !matches!(out, Preflight::Ok) {
            return;
        }
        let (c, t) = (cur.get(name), tgt.get(name));
        if c == t || c.is_some_and(untouchable) || t.is_some_and(untouchable) {
            continue;
        }
        let load_tree = |id: &Id| store.tree(id);
        let sub = |c: Option<&Node>, t: Option<&Node>, out: &mut Preflight| {
            let ct = match c {
                Some(Node::Dir { id, .. }) => match load_tree(id) {
                    Ok(x) => Some(x),
                    Err(e) => return *out = Preflight::PreDamaged(e),
                },
                _ => None,
            };
            let tt = match t {
                Some(Node::Dir { id, .. }) => match load_tree(id) {
                    Ok(x) => Some(x),
                    Err(e) => return *out = Preflight::TargetDamaged(e),
                },
                _ => None,
            };
            if ct.is_some() || tt.is_some() {
                preflight_dir(store, ct.as_ref(), tt.as_ref(), out);
            }
        };
        if let Some(Node::File { id, .. }) = c
            && let Err(e) = store.file(id).and_then(|f| store.file_content(&f))
        {
            *out = Preflight::PreDamaged(e);
            return;
        }
        if let Some(Node::File { id, .. }) = t
            && let Err(e) = store.file(id).and_then(|f| store.file_content(&f))
        {
            *out = Preflight::TargetDamaged(e);
            return;
        }
        sub(c, t, out);
    }
}

/// Restore the workspace to snapshot `target`. See the module docs.
pub fn restore(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    target: &Id,
    auth: &mut Authorize<'_>,
) -> Result<RestoreReport> {
    restore_inner(
        store,
        ws,
        opts,
        None,
        target,
        auth,
        store.fault.restore_after,
    )
}

/// Restore to `target` only if the workspace still equals `expected_pre`
/// (otherwise [`Error::ConcurrentChange`], with nothing touched). The merge
/// queue uses this so a merge never silently reverts edits made to the
/// workspace after the merge was computed.
pub fn restore_expect(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    expected_pre: &Id,
    target: &Id,
    auth: &mut Authorize<'_>,
) -> Result<RestoreReport> {
    restore_inner(
        store,
        ws,
        opts,
        Some(expected_pre),
        target,
        auth,
        store.fault.merge_after,
    )
}

fn restore_inner(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    expected_pre: Option<&Id>,
    target: &Id,
    auth: &mut Authorize<'_>,
    abort_after: Option<u64>,
) -> Result<RestoreReport> {
    if store.journal(&opts.root)?.is_some() {
        return Err(Error::RecoveryPending);
    }
    // Fail before touching anything if the target is unreadable.
    root_of(store, target)?;
    let mut pre = snapshot(store, ws, opts, "pre-restore")?.id();
    if let Some(want) = expected_pre
        && store.snapshot(&pre)?.root != store.snapshot(want)?.root
    {
        return Err(Error::ConcurrentChange(
            "the workspace changed since the operation was prepared".into(),
        ));
    }
    // Pre-flight, read-only: everything the forward pass writes must be
    // intact, and so must everything the undo pass would need. A damaged
    // pre-restore object (a dedup hit on a corrupted object) is healed by
    // re-hashing the workspace, which still holds that content.
    match preflight(store, &pre, target)? {
        Preflight::Ok => {}
        Preflight::TargetDamaged(e) => return Err(e),
        Preflight::PreDamaged(_) => {
            let heal = SnapshotOptions {
                rehash_all: true,
                ..opts.clone()
            };
            pre = snapshot(store, ws, &heal, "pre-restore")?.id();
            match preflight(store, &pre, target)? {
                Preflight::Ok => {}
                Preflight::TargetDamaged(e) | Preflight::PreDamaged(e) => return Err(e),
            }
        }
    }
    store.set_ref(&opts.root.ref_name("restore", &unix_ms().to_string()), &pre)?;
    store.write_journal(&opts.root, &pre, target)?;
    let mut report = RestoreReport {
        pre: Some(pre),
        target: Some(*target),
        ..Default::default()
    };
    let post = run_with_fallback(
        store,
        ws,
        opts,
        &pre,
        target,
        auth,
        &mut report,
        abort_after,
    )?;
    store.clear_journal(&opts.root)?;
    report.post = Some(post);
    Ok(report)
}

/// If a restore was interrupted, return the workspace to its state before
/// that restore. Must run before anything else uses the workspace.
pub fn recover(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    auth: &mut Authorize<'_>,
) -> Result<Option<RestoreReport>> {
    let Some(j) = store.journal(&opts.root)? else {
        return Ok(None);
    };
    let pre = Id::parse(&j.pre).ok_or_else(|| Error::Corrupt("journal".into()))?;
    let target = Id::parse(&j.target).ok_or_else(|| Error::Corrupt("journal".into()))?;
    let now = snapshot(store, ws, opts, "recovery")?.id();
    let mut report = RestoreReport {
        pre: Some(now),
        target: Some(pre),
        ..Default::default()
    };
    apply(store, ws, &now, &pre, auth, &mut report, None)?;
    let (post, diff) = remaining(store, ws, opts, &pre)?;
    if !diff.is_empty() {
        return Err(Error::VerifyFailed(
            diff.iter().take(5).map(|c| c.to_string()).collect(),
        ));
    }
    let _ = target;
    store.clear_journal(&opts.root)?;
    report.post = Some(post);
    Ok(Some(report))
}

/// Populate an **empty** workspace from a snapshot (e.g. a new worktree for
/// the merge queue).
pub fn materialize(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    snap: &Id,
    auth: &mut Authorize<'_>,
) -> Result<RestoreReport> {
    let scanned = ws.scan(&anchor_fs::ScanOptions {
        max_entries: 1,
        skip: &|p, _| opts.exclude.iter().any(|e| p.starts_with(e)),
    });
    match scanned {
        Ok(d) if d.entries.is_empty() => {}
        _ => return Err(Error::NotEmpty),
    }
    let tgt = root_of(store, snap)?;
    let mut report = RestoreReport {
        target: Some(*snap),
        ..Default::default()
    };
    let mut ctx = Ctx {
        store,
        ws,
        auth,
        report: &mut report,
        abort_after: None,
    };
    ctx.apply_dir(Path::new(""), &TreeObj::default(), &tgt)?;
    let (post, diff) = remaining(store, ws, opts, snap)?;
    if !diff.is_empty() {
        return Err(Error::VerifyFailed(
            diff.iter().take(5).map(|c| c.to_string()).collect(),
        ));
    }
    report.post = Some(post);
    Ok(report)
}
