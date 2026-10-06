//! Append-only, hash-chained decision log (spec §5.2(f), ADR-0016).
//!
//! One JSON object per line. Each entry stores the SHA-256 of the previous
//! entry (`prev`) and its own hash (`hash`), computed over the canonical JSON
//! encoding of `{seq, ts_ms, kind, data, prev}`. Editing, deleting or
//! reordering any line breaks the chain, which [`AuditLog::verify`] detects.
//!
//! Several processes may append to one log (the Hub CLI next to a running
//! team, ADR-0023). Each append holds an exclusive lock on `<log>.lock`,
//! verifies and adopts any entries other processes appended since, then
//! writes, so the chain stays linear.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    pub seq: u64,
    pub ts_ms: u64,
    pub kind: String,
    pub data: serde_json::Value,
    pub prev: String,
    pub hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit log i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("audit log line {line}: {msg}")]
    Corrupt { line: u64, msg: String },
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn entry_hash(seq: u64, ts_ms: u64, kind: &str, data: &serde_json::Value, prev: &str) -> String {
    // serde_json's Map is a BTreeMap (no `preserve_order`), so this encoding
    // is canonical: keys are sorted at every level.
    let body = serde_json::json!({
        "seq": seq,
        "ts_ms": ts_ms,
        "kind": kind,
        "data": data,
        "prev": prev,
    });
    sha256_hex(body.to_string().as_bytes())
}

#[derive(Debug)]
struct Tail {
    file: File,
    lock: File,
    /// Length of the log as of our last read or write.
    len: u64,
    next_seq: u64,
    prev: String,
}

fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    tail: Mutex<Tail>,
}

impl AuditLog {
    /// Open (creating if needed) and verify an existing log, positioning at
    /// its tail. A corrupt log is an error: we never append to a broken chain.
    pub fn open(path: &Path) -> Result<Self, AuditError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (next_seq, prev) = if path.exists() {
            let entries = Self::read_entries(path)?;
            Self::verify_entries(&entries)?;
            match entries.last() {
                Some(e) => (e.seq + 1, e.hash.clone()),
                None => (0, GENESIS.to_string()),
            }
        } else {
            (0, GENESIS.to_string())
        };
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(path))?;
        let len = file.metadata()?.len();
        Ok(AuditLog {
            path: path.to_path_buf(),
            tail: Mutex::new(Tail {
                file,
                lock,
                len,
                next_seq,
                prev,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an entry and fsync. Returns its sequence number.
    pub fn append(&self, kind: &str, data: serde_json::Value) -> Result<u64, AuditError> {
        self.append_hashed(kind, data).map(|(seq, _)| seq)
    }

    /// Append an entry and fsync. Returns its sequence number and hash.
    pub fn append_hashed(
        &self,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<(u64, String), AuditError> {
        let mut tail = match self.tail.lock() {
            Ok(t) => t,
            Err(p) => p.into_inner(),
        };
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        tail.lock.lock()?;
        let r = Self::append_locked(&mut tail, ts_ms, kind, data);
        let _ = tail.lock.unlock();
        r
    }

    fn append_locked(
        tail: &mut Tail,
        ts_ms: u64,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<(u64, String), AuditError> {
        Self::adopt_foreign(tail)?;
        let seq = tail.next_seq;
        let hash = entry_hash(seq, ts_ms, kind, &data, &tail.prev);
        let entry = AuditEntry {
            seq,
            ts_ms,
            kind: kind.to_string(),
            data,
            prev: tail.prev.clone(),
            hash: hash.clone(),
        };
        let mut line =
            serde_json::to_string(&entry).map_err(|e| AuditError::Io(std::io::Error::other(e)))?;
        line.push('\n');
        tail.file.write_all(line.as_bytes())?;
        tail.file.sync_data()?;
        tail.len += line.len() as u64;
        tail.next_seq += 1;
        tail.prev = hash.clone();
        Ok((seq, hash))
    }

    /// Verify and adopt entries appended by other processes since our last
    /// read or write. Called with the lock held.
    fn adopt_foreign(tail: &mut Tail) -> Result<(), AuditError> {
        let len = tail.file.metadata()?.len();
        if len == tail.len {
            return Ok(());
        }
        if len < tail.len {
            return Err(AuditError::Corrupt {
                line: tail.next_seq,
                msg: "log shrank while open (truncated)".into(),
            });
        }
        let mut buf = Vec::with_capacity((len - tail.len) as usize);
        let mut f = &tail.file;
        f.seek(SeekFrom::Start(tail.len))?;
        f.take(len - tail.len).read_to_end(&mut buf)?;
        let text = std::str::from_utf8(&buf).map_err(|_| AuditError::Corrupt {
            line: tail.next_seq,
            msg: "foreign append is not UTF-8".into(),
        })?;
        for l in text.lines() {
            let line = tail.next_seq + 1;
            let e: AuditEntry = serde_json::from_str(l).map_err(|e| AuditError::Corrupt {
                line,
                msg: e.to_string(),
            })?;
            let h = entry_hash(e.seq, e.ts_ms, &e.kind, &e.data, &e.prev);
            if e.seq != tail.next_seq || e.prev != tail.prev || h != e.hash {
                return Err(AuditError::Corrupt {
                    line,
                    msg: "foreign append does not extend the chain".into(),
                });
            }
            tail.next_seq += 1;
            tail.prev = e.hash;
        }
        tail.len = len;
        Ok(())
    }

    /// The `(seq, hash)` of the last entry this handle knows about.
    pub fn head(&self) -> (Option<u64>, String) {
        let tail = match self.tail.lock() {
            Ok(t) => t,
            Err(p) => p.into_inner(),
        };
        (tail.next_seq.checked_sub(1), tail.prev.clone())
    }

    pub fn read_entries(path: &Path) -> Result<Vec<AuditEntry>, AuditError> {
        let r = BufReader::new(File::open(path)?);
        let mut out = Vec::new();
        for (i, line) in r.lines().enumerate() {
            let line = line?;
            let e: AuditEntry = serde_json::from_str(&line).map_err(|e| AuditError::Corrupt {
                line: i as u64 + 1,
                msg: e.to_string(),
            })?;
            out.push(e);
        }
        Ok(out)
    }

    fn verify_entries(entries: &[AuditEntry]) -> Result<(), AuditError> {
        let mut prev = GENESIS.to_string();
        for (i, e) in entries.iter().enumerate() {
            let line = i as u64 + 1;
            if e.seq != i as u64 {
                return Err(AuditError::Corrupt {
                    line,
                    msg: format!("sequence gap: expected {i}, found {}", e.seq),
                });
            }
            if e.prev != prev {
                return Err(AuditError::Corrupt {
                    line,
                    msg: "chain broken: prev hash mismatch".into(),
                });
            }
            let h = entry_hash(e.seq, e.ts_ms, &e.kind, &e.data, &e.prev);
            if h != e.hash {
                return Err(AuditError::Corrupt {
                    line,
                    msg: "entry hash mismatch (tampered)".into(),
                });
            }
            prev = e.hash.clone();
        }
        Ok(())
    }

    /// Verify the whole chain. Returns the number of entries.
    pub fn verify(path: &Path) -> Result<u64, AuditError> {
        let entries = Self::read_entries(path)?;
        Self::verify_entries(&entries)?;
        Ok(entries.len() as u64)
    }
}
