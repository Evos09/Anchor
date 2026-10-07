//! Whole-workspace scanning for the snapshot engine (spec §5.5).
//!
//! The scan never follows a symlink: on Unix it walks by directory
//! descriptor (`openat(O_DIRECTORY | O_NOFOLLOW)` + `fstatat(
//! AT_SYMLINK_NOFOLLOW)`), so symlink loops cannot make it hang and a
//! directory swapped for a symlink mid-scan is detected rather than
//! followed. File contents are read afterwards with
//! [`Workspace::open_scanned`], which re-opens strictly and checks the file
//! is the version the scan saw.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{FsError, Kind, Meta, Workspace, split_parent, sys};

/// One directory of a scan. Entries are sorted by name bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDir {
    pub meta: Meta,
    pub entries: Vec<ScanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub name: OsString,
    pub node: ScanNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanNode {
    File(Meta),
    Dir(ScanDir),
    Symlink(Meta),
    /// Sockets, fifos, devices: recorded, never read.
    Other(Meta),
    /// A directory left unscanned: unreadable, or past the depth limit.
    /// Recorded so a snapshot still succeeds, never descended into (review
    /// finding P-5).
    Unscanned(Meta),
}

impl ScanNode {
    pub fn meta(&self) -> &Meta {
        match self {
            ScanNode::File(m)
            | ScanNode::Symlink(m)
            | ScanNode::Other(m)
            | ScanNode::Unscanned(m) => m,
            ScanNode::Dir(d) => &d.meta,
        }
    }
}

pub struct ScanOptions<'a> {
    /// Refuse (fail closed) beyond this many entries.
    pub max_entries: usize,
    /// Return `true` to leave an entry (and, for a directory, its contents)
    /// out of the scan. Receives the workspace-relative path and kind.
    pub skip: &'a dyn Fn(&Path, Kind) -> bool,
}

impl std::fmt::Debug for ScanOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanOptions")
            .field("max_entries", &self.max_entries)
            .finish_non_exhaustive()
    }
}

/// True iff `now` is the same version of the file the scan recorded as
/// `seen`: same identity (where the platform reports one), size, and
/// modification and change times.
pub(crate) fn same_scanned_version(seen: &Meta, now: &Meta) -> bool {
    let ids_match = seen.id == now.id || seen.id.ino == 0 || now.id.ino == 0;
    ids_match
        && seen.kind == now.kind
        && seen.len == now.len
        && seen.mtime_ns == now.mtime_ns
        && seen.ctime_ns == now.ctime_ns
}

/// A regular file opened for reading after a scan. Call
/// [`ScannedFile::finish`] after reading to confirm the file did not change
/// while it was read.
#[derive(Debug)]
pub struct ScannedFile {
    file: std::fs::File,
    expected: Meta,
}

impl Read for ScannedFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl ScannedFile {
    /// Verify the file still is the version that was scanned and opened.
    pub fn finish(self) -> Result<(), FsError> {
        let now = sys::file_meta(&self.file)?;
        if same_scanned_version(&self.expected, &now) {
            Ok(())
        } else {
            Err(FsError::Changed)
        }
    }
}

impl Workspace {
    /// Scan the whole workspace without following symlinks.
    pub fn scan(&self, opts: &ScanOptions<'_>) -> Result<ScanDir, FsError> {
        let mut count = 0usize;
        sys::scan(&self.handle, &self.root, opts, &mut count)
    }

    /// Open a regular file the scan reported as `expected` for streaming
    /// reads. Refuses hardlinked files and any file that is no longer the
    /// scanned version ([`FsError::Changed`]).
    pub fn open_scanned(&self, rel: &Path, expected: &Meta) -> Result<ScannedFile, FsError> {
        let rel = self.confine_path(rel)?;
        let file = sys::open_read_strict(&self.handle, &self.root, &rel)?;
        let now = sys::file_meta(&file)?;
        if now.kind != Kind::File {
            return Err(FsError::NotAFile);
        }
        if now.nlink > 1 {
            return Err(FsError::HardlinkRefused(now.nlink));
        }
        if !same_scanned_version(expected, &now) {
            return Err(FsError::Changed);
        }
        Ok(ScannedFile {
            file,
            expected: now,
        })
    }

    /// Read the target of a symlink the scan reported as `expected`.
    pub fn read_link_scanned(&self, rel: &Path, expected: &Meta) -> Result<OsString, FsError> {
        let rel = self.confine_path(rel)?;
        let (parent, name) = split_parent(&rel)?;
        sys::read_link_strict(&self.handle, &self.root, &parent, &name, expected)
    }
}

/// Join helper used by the platform back ends.
pub(crate) fn child(rel: &Path, name: &std::ffi::OsStr) -> PathBuf {
    if rel.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        rel.join(name)
    }
}
