//! Object model and its strict binary encoding.
//!
//! An object on disk is `[type byte][payload]`, and its id is the SHA-256 of
//! those bytes. Putting the type in the hashed bytes means an object of one
//! type can never be accepted as another. Decoding is strict: it rejects
//! trailing bytes, unknown versions, unsorted or duplicate tree entries and
//! invalid names, and the decoded value must re-encode to the exact input
//! (one canonical encoding per value).

use sha2::{Digest, Sha256};

use crate::chunker::MAX_CHUNK;

/// Object id: SHA-256 of the stored bytes (type byte + payload).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub [u8; 32]);

impl Id {
    pub fn hex(&self) -> String {
        anchor_policy::audit::hex(&self.0)
    }

    /// Parse 64 lowercase hex digits (nothing else: ids name files).
    pub fn parse(s: &str) -> Option<Id> {
        let b = s.as_bytes();
        if b.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in b.chunks(2).enumerate() {
            let v = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            };
            out[i] = (v(pair[0])? << 4) | v(pair[1])?;
        }
        Some(Id(out))
    }
}

impl std::fmt::Debug for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.hex()[..16])
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjType {
    Chunk = 1,
    File = 2,
    Tree = 3,
    Snapshot = 4,
}

impl ObjType {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => ObjType::Chunk,
            2 => ObjType::File,
            3 => ObjType::Tree,
            4 => ObjType::Snapshot,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("malformed object: {0}")]
pub struct DecodeError(pub String);

fn bad<T>(msg: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(msg.into()))
}

/// The id of `ty` + `payload`.
pub fn object_id(ty: ObjType, payload: &[u8]) -> Id {
    let mut h = Sha256::new();
    h.update([ty as u8]);
    h.update(payload);
    Id(h.finalize().into())
}

/// Split stored bytes into type and payload, verifying the id.
pub fn open_object<'a>(id: &Id, stored: &'a [u8]) -> Result<(ObjType, &'a [u8]), DecodeError> {
    let actual = Id(Sha256::digest(stored).into());
    if actual != *id {
        return bad(format!("object {id} fails its hash check"));
    }
    let (&t, payload) = stored
        .split_first()
        .ok_or_else(|| DecodeError("empty object".into()))?;
    let ty = ObjType::from_byte(t).ok_or_else(|| DecodeError(format!("unknown type {t}")))?;
    Ok((ty, payload))
}

// ---------------------------------------------------------------------------
// File
// ---------------------------------------------------------------------------

/// A file's content: its chunks in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileObj {
    pub size: u64,
    /// SHA-256 of the whole content (used for compare-and-swap on restore).
    pub sha256: [u8; 32],
    pub chunks: Vec<(Id, u32)>,
}

const FILE_V: u8 = 1;
const TREE_V: u8 = 1;
const SNAP_V: u8 = 1;

/// Longest symlink target recorded (longer ones are left untracked).
pub const MAX_LINK_TARGET: usize = 4096;
/// Longest file name accepted.
pub const MAX_NAME: usize = 255;

impl FileObj {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::with_capacity(45 + self.chunks.len() * 36);
        w.push(FILE_V);
        w.extend_from_slice(&self.size.to_le_bytes());
        w.extend_from_slice(&self.sha256);
        w.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for (id, len) in &self.chunks {
            w.extend_from_slice(&id.0);
            w.extend_from_slice(&len.to_le_bytes());
        }
        w
    }

    pub fn decode(p: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader(p);
        if r.u8()? != FILE_V {
            return bad("file: unknown version");
        }
        let size = r.u64()?;
        let sha256 = r.arr32()?;
        let n = r.u32()? as usize;
        if n > p.len() / 36 {
            return bad("file: chunk count exceeds payload");
        }
        let mut chunks = Vec::with_capacity(n);
        let mut total = 0u64;
        for _ in 0..n {
            let id = Id(r.arr32()?);
            let len = r.u32()?;
            if len == 0 || len as usize > MAX_CHUNK {
                return bad("file: chunk length out of range");
            }
            total += len as u64;
            chunks.push((id, len));
        }
        r.end()?;
        if total != size {
            return bad("file: chunk lengths do not sum to the size");
        }
        Ok(FileObj {
            size,
            sha256,
            chunks,
        })
    }
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

/// Why an entry is recorded without content. Restore never touches such
/// paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Untracked {
    /// More than one hard link (reading it could copy outside content into
    /// the workspace on restore).
    Hardlinked = 1,
    /// Larger than the configured limit.
    TooLarge = 2,
    /// Socket, fifo, device.
    Special = 3,
    /// Symlink target too long or otherwise unrecordable.
    Unrecordable = 4,
}

impl Untracked {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Untracked::Hardlinked,
            2 => Untracked::TooLarge,
            3 => Untracked::Special,
            4 => Untracked::Unrecordable,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File { id: Id, mode: u32 },
    Dir { id: Id, mode: u32 },
    Symlink { target: Vec<u8> },
    Untracked(Untracked),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: Vec<u8>,
    pub node: Node,
}

/// A directory: entries sorted by name bytes, names unique.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeObj {
    pub entries: Vec<TreeEntry>,
}

/// A valid single path component: non-empty, at most [`MAX_NAME`] bytes,
/// not `.` or `..`, no `/` or NUL (and no `\` or `:` on Windows, where
/// they would be separators or stream names).
pub fn valid_name(n: &[u8]) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && n != b"."
        && n != b".."
        && !n.contains(&b'/')
        && !n.contains(&0)
        && !(cfg!(windows) && (n.contains(&b'\\') || n.contains(&b':')))
}

impl TreeObj {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::with_capacity(5 + self.entries.len() * 48);
        w.push(TREE_V);
        w.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            w.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            w.extend_from_slice(&e.name);
            match &e.node {
                Node::File { id, mode } => {
                    w.push(1);
                    w.extend_from_slice(&mode.to_le_bytes());
                    w.extend_from_slice(&id.0);
                }
                Node::Dir { id, mode } => {
                    w.push(2);
                    w.extend_from_slice(&mode.to_le_bytes());
                    w.extend_from_slice(&id.0);
                }
                Node::Symlink { target } => {
                    w.push(3);
                    w.extend_from_slice(&(target.len() as u32).to_le_bytes());
                    w.extend_from_slice(target);
                }
                Node::Untracked(why) => {
                    w.push(4);
                    w.push(*why as u8);
                }
            }
        }
        w
    }

    pub fn decode(p: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader(p);
        if r.u8()? != TREE_V {
            return bad("tree: unknown version");
        }
        let n = r.u32()? as usize;
        if n > p.len() / 4 {
            return bad("tree: entry count exceeds payload");
        }
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(n);
        for _ in 0..n {
            let nlen = r.u16()? as usize;
            let name = r.bytes(nlen)?.to_vec();
            if !valid_name(&name) {
                return bad(format!(
                    "tree: invalid name {:?}",
                    String::from_utf8_lossy(&name)
                ));
            }
            if let Some(prev) = entries.last() {
                if prev.name >= name {
                    return bad("tree: entries unsorted or duplicated");
                }
            }
            let node = match r.u8()? {
                1 => Node::File {
                    mode: mode_bits(r.u32()?)?,
                    id: Id(r.arr32()?),
                },
                2 => Node::Dir {
                    mode: mode_bits(r.u32()?)?,
                    id: Id(r.arr32()?),
                },
                3 => {
                    let len = r.u32()? as usize;
                    if len == 0 || len > MAX_LINK_TARGET {
                        return bad("tree: symlink target length out of range");
                    }
                    let target = r.bytes(len)?.to_vec();
                    if target.contains(&0) {
                        return bad("tree: symlink target contains NUL");
                    }
                    Node::Symlink { target }
                }
                4 => Node::Untracked(
                    Untracked::from_byte(r.u8()?)
                        .ok_or_else(|| DecodeError("tree: unknown untracked reason".into()))?,
                ),
                k => return bad(format!("tree: unknown entry kind {k}")),
            };
            entries.push(TreeEntry { name, node });
        }
        r.end()?;
        Ok(TreeObj { entries })
    }

    pub fn get(&self, name: &[u8]) -> Option<&Node> {
        self.entries
            .binary_search_by(|e| e.name.as_slice().cmp(name))
            .ok()
            .map(|i| &self.entries[i].node)
    }
}

/// Only permission bits are recorded and restored: setuid, setgid and
/// sticky bits are never recreated (restores may run as root).
fn mode_bits(m: u32) -> Result<u32, DecodeError> {
    if m & !0o777 != 0 {
        return bad("tree: mode has bits outside 0o777");
    }
    Ok(m)
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotObj {
    pub root: Id,
    pub created_unix_ms: u64,
    pub files: u64,
    pub bytes: u64,
    /// Free-form label (e.g. `turn 7`), at most 256 bytes of UTF-8.
    pub label: String,
}

impl SnapshotObj {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::with_capacity(64 + self.label.len());
        w.push(SNAP_V);
        w.extend_from_slice(&self.root.0);
        w.extend_from_slice(&self.created_unix_ms.to_le_bytes());
        w.extend_from_slice(&self.files.to_le_bytes());
        w.extend_from_slice(&self.bytes.to_le_bytes());
        w.extend_from_slice(&(self.label.len() as u16).to_le_bytes());
        w.extend_from_slice(self.label.as_bytes());
        w
    }

    pub fn decode(p: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader(p);
        if r.u8()? != SNAP_V {
            return bad("snapshot: unknown version");
        }
        let root = Id(r.arr32()?);
        let created_unix_ms = r.u64()?;
        let files = r.u64()?;
        let bytes = r.u64()?;
        let llen = r.u16()? as usize;
        if llen > 256 {
            return bad("snapshot: label too long");
        }
        let label = String::from_utf8(r.bytes(llen)?.to_vec())
            .map_err(|_| DecodeError("snapshot: label not UTF-8".into()))?;
        r.end()?;
        Ok(SnapshotObj {
            root,
            created_unix_ms,
            files,
            bytes,
            label,
        })
    }
}

// ---------------------------------------------------------------------------

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return bad("truncated");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.bytes(4)?);
        Ok(u32::from_le_bytes(a))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.bytes(8)?);
        Ok(u64::from_le_bytes(a))
    }
    fn arr32(&mut self) -> Result<[u8; 32], DecodeError> {
        let mut a = [0u8; 32];
        a.copy_from_slice(self.bytes(32)?);
        Ok(a)
    }
    fn end(&self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            bad("trailing bytes")
        }
    }
}
