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
        let mut r = &body[8..];
        let take = |r: &mut &[u8], n: usize| -> Option<Vec<u8>> {
            if r.len() < n {
                return None;
            }
            let (a, rest) = r.split_at(n);
            *r = rest;
            Some(a.to_vec())
        };
        let u32_ = |r: &mut &[u8]| take(r, 4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
        let u64_ = |r: &mut &[u8]| {
            take(r, 8).map(|v| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&v);
                u64::from_le_bytes(a)
            })
        };
        let i128_ = |r: &mut &[u8]| {
            take(r, 16).map(|v| {
                let mut a = [0u8; 16];
                a.copy_from_slice(&v);
                i128::from_le_bytes(a)
            })
        };
        let n = u32_(&mut r)? as usize;
        let mut entries = HashMap::with_capacity(n.min(1 << 22));
        for _ in 0..n {
            let plen = u32_(&mut r)? as usize;
            let path = take(&mut r, plen)?;
            let fp = Fingerprint {
                len: u64_(&mut r)?,
                mtime_ns: i128_(&mut r)?,
                ctime_ns: i128_(&mut r)?,
                dev: u64_(&mut r)?,
                ino: u64_(&mut r)?,
                mode: u32_(&mut r)?,
            };
            let mut id = [0u8; 32];
            id.copy_from_slice(&take(&mut r, 32)?);
            let racy = take(&mut r, 1)?[0] != 0;
            entries.insert(
                path,
                IndexEntry {
                    fp,
                    file: Id(id),
                    racy,
                },
            );
        }
        if !r.is_empty() {
            return None;
        }
        Some(Index { entries })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut keys: Vec<&Vec<u8>> = self.entries.keys().collect();
        keys.sort();
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
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, self.encode())?;
        std::fs::rename(&tmp, path)
    }
}
