//! Change-detection cache: workspace-relative path → the stat fingerprint a
//! file had when it was last hashed, and the resulting file object.
//!
//! A file whose fingerprint is unchanged is not re-read. The fingerprint
//! includes the inode change time, which user code cannot set, so
//! back-dating mtime (`touch -d`) does not hide a change. An entry whose
//! times fall within [`RACY_NS`] of the scan that recorded it is marked
//! racy and always re-hashed next time (a write in the same timestamp
//! tick could otherwise go unnoticed).
//!
//! The index is only a cache: a missing, truncated or corrupt index (it
//! carries a trailing SHA-256) just means every file is hashed again.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::object::Id;

const MAGIC: &[u8; 8] = b"ANCHIX01";

/// Entries modified this close to the recording scan are re-hashed.
pub const RACY_NS: i128 = 2_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub len: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
}

impl Fingerprint {
    pub fn of(m: &anchor_fs::Meta) -> Self {
        Fingerprint {
            len: m.len,
            mtime_ns: m.mtime_ns,
            ctime_ns: m.ctime_ns,
            dev: m.id.dev,
            ino: m.id.ino,
            mode: m.mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub fp: Fingerprint,
    pub file: Id,
    pub racy: bool,
}

#[derive(Debug, Default)]
pub struct Index {
    pub entries: HashMap<Vec<u8>, IndexEntry>,
}

impl Index {
    /// Load the index; any problem yields an empty index.
    pub fn load(path: &Path) -> Index {
        std::fs::read(path)
            .ok()
            .and_then(|b| Self::decode(&b))
            .unwrap_or_default()
    }

    /// The cached file object for `path` if its fingerprint is unchanged
    /// and the entry is not racy.
    pub fn lookup(&self, path: &[u8], fp: &Fingerprint) -> Option<Id> {
        self.entries
            .get(path)
            .filter(|e| !e.racy && e.fp == *fp)
            .map(|e| e.file)
    }

    fn decode(b: &[u8]) -> Option<Index> {
        if b.len() < MAGIC.len() + 4 + 32 || &b[..8] != MAGIC {
            return None;
        }
        let (body, sum) = b.split_at(b.len() - 32);
        if Sha256::digest(body).as_slice() != sum {
            return None;
        }
        let mut r = Cursor { b: &body[8..] };
        let n = r.u32()? as usize;
        let mut entries = HashMap::with_capacity(n.min(1 << 22));
        for _ in 0..n {
            let plen = r.u32()? as usize;
            let path = r.take(plen)?.to_vec();
            let fp = Fingerprint {
                len: r.u64()?,
                mtime_ns: r.i128()?,
                ctime_ns: r.i128()?,
                dev: r.u64()?,
                ino: r.u64()?,
                mode: r.u32()?,
            };
            let file = Id(r.take(32)?.try_into().ok()?);
            let racy = r.take(1)?[0] != 0;
            entries.insert(path, IndexEntry { fp, file, racy });
        }
        if !r.b.is_empty() {
            return None;
        }
        Some(Index { entries })
    }

    pub fn encode(&self) -> Vec<u8> {
        // Order is irrelevant for a cache; skipping the sort saves time.
        let keys: Vec<&Vec<u8>> = self.entries.keys().collect();
        let mut w = Vec::with_capacity(16 + self.entries.len() * 128);
        w.extend_from_slice(MAGIC);
        w.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for k in keys {
            let e = &self.entries[k];
            w.extend_from_slice(&(k.len() as u32).to_le_bytes());
            w.extend_from_slice(k);
            w.extend_from_slice(&e.fp.len.to_le_bytes());
            w.extend_from_slice(&e.fp.mtime_ns.to_le_bytes());
            w.extend_from_slice(&e.fp.ctime_ns.to_le_bytes());
            w.extend_from_slice(&e.fp.dev.to_le_bytes());
            w.extend_from_slice(&e.fp.ino.to_le_bytes());
            w.extend_from_slice(&e.fp.mode.to_le_bytes());
            w.extend_from_slice(&e.file.0);
            w.push(u8::from(e.racy));
        }
        let sum = Sha256::digest(&w);
        w.extend_from_slice(&sum);
        w
    }

    /// Save (temp + rename; no fsync needed for a cache whose corruption
    /// is detected and harmless).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, self.encode())?;
        std::fs::rename(&tmp, path)
    }
}

/// Allocation-free reader over the index bytes.
struct Cursor<'a> {
    b: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.b.len() < n {
            return None;
        }
        let (a, rest) = self.b.split_at(n);
        self.b = rest;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i128(&mut self) -> Option<i128> {
        Some(i128::from_le_bytes(self.take(16)?.try_into().ok()?))
    }
}
