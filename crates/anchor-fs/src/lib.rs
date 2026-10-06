//! Canonical path resolution and TOCTOU-safe file access (spec §5.1).
//!
//! # Guarantees
//!
//! * **Confinement.** Every tool-supplied path is first confined lexically to
//!   the workspace (`..` cannot climb above the root; absolute paths must lie
//!   under the canonical root), then resolved *by the kernel* relative to a
//!   handle on the workspace root, refusing any resolution that escapes it.
//! * **Canonical display.** [`Resolved::canonical`] is read back from the
//!   opened handle (`/proc/self/fd` on Linux, `F_GETPATH` on macOS,
//!   `GetFinalPathNameByHandleW` on Windows), never derived from the input.
//! * **No re-resolution.** After resolution every operation uses the
//!   canonical, symlink-free relative path with symlink following *disabled*
//!   (`RESOLVE_NO_SYMLINKS` / `O_NOFOLLOW_ANY` / `FILE_FLAG_OPEN_REPARSE_POINT`),
//!   and verifies the file identity (device + inode / file ID) recorded at
//!   resolution. A symlink swapped in after approval makes the operation
//!   fail; it can never redirect it.
//! * **Atomic replacement.** Writes go to a temporary file in the same
//!   directory handle, are fsynced, and are swapped into place atomically
//!   (`RENAME_EXCHANGE` / `RENAME_SWAP`). The swapped-out file is checked to
//!   be the approved one; if not, the swap is reversed.
//! * **Hardlinks.** Regular files with more than one link are refused.
//! * **No fallback.** Any failure refuses the operation.
//! * **Approval required.** Mutations consume an [`anchor_policy::Approved`]
//!   whose canonical target must equal the resolved target.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anchor_policy::{Approved, Operation};
use sha2::{Digest, Sha256};

mod sys;

/// Largest file `read` will load.
pub const MAX_READ_BYTES: u64 = 64 << 20;

/// Identity of a file independent of its name (device + inode, or volume
/// serial + file index). Two names are the same file iff their ids match;
/// this is how case- and Unicode-normalisation aliases are unified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub id: FileId,
    pub kind: Kind,
    pub nlink: u64,
    pub len: u64,
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) mode: u32,
}

/// The result of resolving a tool-supplied path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Absolute canonical path. The only path ever shown to the user.
    pub canonical: PathBuf,
    /// `canonical` relative to the workspace root (symlink-free).
    pub rel: PathBuf,
    /// Metadata if the target exists.
    pub meta: Option<Meta>,
    /// Directories between the deepest existing ancestor and the target
    /// that would have to be created (only for non-existent targets).
    pub missing_dirs: Vec<OsString>,
}

impl Resolved {
    pub fn exists(&self) -> bool {
        self.meta.is_some()
    }
    pub fn is_dir(&self) -> bool {
        matches!(self.meta, Some(Meta { kind: Kind::Dir, .. }))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FsError {
    #[error("path parameter is empty or contains NUL")]
    InvalidParam,
    #[error("absolute path is outside the workspace")]
    AbsoluteOutsideWorkspace,
    #[error("path traverses above the workspace root")]
    TraversalEscapesRoot,
    #[error("path resolves outside the workspace (symlink escape)")]
    EscapesWorkspace,
    #[error("a symlink appeared on a path that was resolved without one (possible swap attack)")]
    SymlinkInStrictPath,
    #[error("target is a dangling symlink")]
    DanglingSymlink,
    #[error("target has {0} hard links; refusing (possible hardlink attack)")]
    HardlinkRefused(u64),
    #[error("target changed between resolution and use")]
    Changed,
    #[error("target content does not match the expected hash")]
    ContentMismatch,
    #[error("target is not a regular file")]
    NotAFile,
    #[error("target is not a directory")]
    NotADirectory,
    #[error("target does not exist")]
    NotFound,
    #[error("file larger than {MAX_READ_BYTES} bytes")]
    TooLarge,
    #[error("approval does not cover this operation on this target")]
    ApprovalMismatch,
    #[error("append verification failed: existing bytes were modified")]
    AppendVerificationFailed,
    #[error("the filesystem does not support atomic exchange; refusing to replace non-atomically")]
    AtomicReplaceUnsupported,
    #[error("i/o error: {0}")]
    Io(String),
}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        FsError::Io(e.to_string())
    }
}

/// Precondition on the current target for a replacement (compare-and-swap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// The target must not exist.
    Absent,
    /// The target must be the file resolved earlier, with this content hash.
    Existing { id: FileId, sha256: [u8; 32] },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    pub bytes: Vec<u8>,
    pub sha256: [u8; 32],
    pub meta: Meta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReceipt {
    pub canonical: PathBuf,
    pub bytes_written: u64,
    pub new_sha256: [u8; 32],
    pub decision_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: String,
    pub kind: Kind,
}

static MUTATIONS: AtomicU64 = AtomicU64::new(0);

/// Number of successful filesystem mutations performed by this process
/// through this crate (instrumentation; compare with
/// `anchor_policy::approvals_minted`).
pub fn mutations_performed() -> u64 {
    MUTATIONS.load(Ordering::SeqCst)
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// A workspace: a canonical root plus an open handle on it.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    handle: sys::Root,
    allow_hardlinked_reads: bool,
}

impl Workspace {
    /// Open a workspace root. The root itself is chosen by the user, so it is
    /// canonicalised by following symlinks once, here.
    pub fn open(path: &Path) -> Result<Self, FsError> {
        let (handle, root) = sys::open_root(path)?;
        Ok(Workspace {
            root,
            handle,
            allow_hardlinked_reads: false,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lexically confine a tool parameter to a root-relative path.
    pub fn confine(&self, param: &str) -> Result<PathBuf, FsError> {
        if param.is_empty() || param.contains('\0') {
            return Err(FsError::InvalidParam);
        }
        let p = Path::new(param);
        let rel_input: &Path = if p.is_absolute() || p.has_root() {
            match strip_root_prefix(p, &self.root) {
                Some(r) => r,
                None => return Err(FsError::AbsoluteOutsideWorkspace),
            }
        } else {
            p
        };
        let mut out = PathBuf::new();
        for c in rel_input.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        return Err(FsError::TraversalEscapesRoot);
                    }
                }
                Component::Normal(n) => out.push(n),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(FsError::AbsoluteOutsideWorkspace);
                }
            }
        }
        Ok(out)
    }

    /// Resolve a tool-supplied path to its canonical form, following
    /// symlinks only while they stay inside the workspace.
    pub fn resolve(&self, param: &str) -> Result<Resolved, FsError> {
        let rel = self.confine(param)?;
        match sys::resolve_follow(&self.handle, &self.root, &rel) {
            Ok((canonical, meta)) => {
                let rel = self.rel_of(&canonical)?;
                Ok(Resolved {
                    canonical,
                    rel,
                    meta: Some(meta),
                    missing_dirs: Vec::new(),
                })
            }
            Err(FsError::NotFound) => self.resolve_missing(&rel),
            Err(e) => Err(e),
        }
    }

    /// Resolve a non-existent target: canonicalise its deepest existing
    /// ancestor (which must be a directory) and append the missing names.
    fn resolve_missing(&self, rel: &Path) -> Result<Resolved, FsError> {
        let mut missing: Vec<OsString> = Vec::new();
        let mut cur = rel.to_path_buf();
        loop {
            let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
                return Err(FsError::NotFound);
            };
            missing.push(name);
            cur.pop();
            match sys::resolve_follow(&self.handle, &self.root, &cur) {
                Ok((canon_parent, meta)) => {
                    if meta.kind != Kind::Dir {
                        return Err(FsError::NotADirectory);
                    }
                    missing.reverse();
                    let canonical: PathBuf = missing.iter().fold(canon_parent.clone(), |mut acc, m| {
                        acc.push(m);
                        acc
                    });
                    // `missing` is [dir.., target]; keep only the directories.
                    let target = missing.pop().ok_or(FsError::NotFound)?;
                    // The existing ancestor must not hold a dangling symlink
                    // under the next name (resolution reported it missing
                    // because its target does not exist).
                    let first = missing.first().unwrap_or(&target);
                    let parent_rel = self.rel_of(&canon_parent)?;
                    if sys::is_symlink(&self.handle, &self.root, &parent_rel, first)? {
                        return Err(FsError::DanglingSymlink);
                    }
                    let rel = self.rel_of(&canonical)?;
                    return Ok(Resolved {
                        canonical,
                        rel,
                        meta: None,
                        missing_dirs: missing,
                    });
                }
                Err(FsError::NotFound) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn rel_of(&self, canonical: &Path) -> Result<PathBuf, FsError> {
        canonical
            .strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .map_err(|_| FsError::EscapesWorkspace)
    }

    /// Read a resolved regular file. Re-opens it without following symlinks
    /// and verifies it is still the same file.
    pub fn read(&self, r: &Resolved) -> Result<FileContent, FsError> {
        let expected = r.meta.ok_or(FsError::NotFound)?;
        if expected.kind != Kind::File {
            return Err(FsError::NotAFile);
        }
        if expected.nlink > 1 && !self.allow_hardlinked_reads {
            return Err(FsError::HardlinkRefused(expected.nlink));
        }
        let (meta, bytes) = sys::read_strict(&self.handle, &self.root, &r.rel)?;
        if meta.id != expected.id {
            return Err(FsError::Changed);
        }
        if meta.nlink > 1 && !self.allow_hardlinked_reads {
            return Err(FsError::HardlinkRefused(meta.nlink));
        }
        let sha256 = sha256(&bytes);
        Ok(FileContent {
            bytes,
            sha256,
            meta,
        })
    }

    /// List a resolved directory (names only, no symlink following).
    pub fn list_dir(&self, r: &Resolved) -> Result<Vec<DirEntryInfo>, FsError> {
        let expected = r.meta.ok_or(FsError::NotFound)?;
        if expected.kind != Kind::Dir {
            return Err(FsError::NotADirectory);
        }
        let (id, mut entries) = sys::list_strict(&self.handle, &self.root, &r.rel)?;
        if id != expected.id {
            return Err(FsError::Changed);
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Atomically create or replace a file. `expect` is checked against the
    /// file actually being replaced; the approval must name this canonical
    /// target and a mutating operation.
    pub fn write(
        &self,
        r: &Resolved,
        content: &[u8],
        expect: Expect,
        approval: Approved,
    ) -> Result<WriteReceipt, FsError> {
        check_approval(&approval, r)?;
        match (&expect, &r.meta) {
            (Expect::Absent, None) => {}
            (Expect::Existing { id, .. }, Some(m)) => {
                if m.id != *id {
                    return Err(FsError::Changed);
                }
                if m.kind != Kind::File {
                    return Err(FsError::NotAFile);
                }
                if m.nlink > 1 {
                    return Err(FsError::HardlinkRefused(m.nlink));
                }
            }
            _ => return Err(FsError::Changed),
        }
        let (parent_rel, name) = split_parent(&r.rel)?;
        // `missing_dirs` lie between the deepest existing ancestor and the
        // target; strip them from the parent to find that ancestor.
        let mut base = parent_rel.clone();
        for _ in &r.missing_dirs {
            base.pop();
        }
        sys::write_atomic(
            &self.handle,
            &self.root,
            &base,
            &r.missing_dirs,
            &name,
            content,
            &expect,
        )?;
        MUTATIONS.fetch_add(1, Ordering::SeqCst);
        Ok(WriteReceipt {
            canonical: r.canonical.clone(),
            bytes_written: content.len() as u64,
            new_sha256: sha256(content),
            decision_id: approval.decision_id(),
        })
    }

    /// Append `suffix` to an existing file, verifying byte-for-byte that the
    /// original content is an unchanged prefix of the result (spec §5.3(b)).
    pub fn append_verified(
        &self,
        r: &Resolved,
        suffix: &[u8],
        original_sha256: [u8; 32],
        approval: Approved,
    ) -> Result<WriteReceipt, FsError> {
        if approval.operation() != Operation::Append {
            return Err(FsError::ApprovalMismatch);
        }
        let current = self.read(r)?;
        if current.sha256 != original_sha256 {
            return Err(FsError::ContentMismatch);
        }
        let mut new = current.bytes.clone();
        new.extend_from_slice(suffix);
        if !is_pure_append(&current.bytes, &new) {
            return Err(FsError::AppendVerificationFailed);
        }
        let receipt = self.write(
            r,
            &new,
            Expect::Existing {
                id: current.meta.id,
                sha256: original_sha256,
            },
            approval,
        )?;
        // Re-read what is now on disk and verify the prefix again.
        let after = self.resolve(&r.rel.to_string_lossy())?;
        let on_disk = self.read(&after)?;
        if !is_pure_append(&current.bytes, &on_disk.bytes)
            || on_disk.bytes.len() != current.bytes.len() + suffix.len()
        {
            return Err(FsError::AppendVerificationFailed);
        }
        Ok(receipt)
    }
}

/// True iff `new` starts with exactly `original` (hash and byte comparison).
pub fn is_pure_append(original: &[u8], new: &[u8]) -> bool {
    new.len() >= original.len()
        && sha256(&new[..original.len()]) == sha256(original)
        && &new[..original.len()] == original
}

fn check_approval(a: &Approved, r: &Resolved) -> Result<(), FsError> {
    if !a.operation().is_mutation() {
        return Err(FsError::ApprovalMismatch);
    }
    match a.target() {
        Some(t) if t == r.canonical => Ok(()),
        _ => Err(FsError::ApprovalMismatch),
    }
}

fn split_parent(rel: &Path) -> Result<(PathBuf, OsString), FsError> {
    let name = rel.file_name().ok_or(FsError::NotAFile)?.to_os_string();
    let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok((parent, name))
}

/// Strip the canonical root from an absolute path, comparing components
/// (case-insensitively on platforms whose default filesystems are).
fn strip_root_prefix<'a>(p: &'a Path, root: &Path) -> Option<&'a Path> {
    if let Ok(r) = p.strip_prefix(root) {
        return Some(r);
    }
    if cfg!(any(target_os = "macos", windows)) {
        let mut pc = p.components();
        for rc in root.components() {
            let c = pc.next()?;
            if !sys::names_equivalent(c.as_os_str(), rc.as_os_str()) {
                return None;
            }
        }
        return Some(pc.as_path());
    }
    None
}

#[cfg(test)]
mod tests;
