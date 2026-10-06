//! Interim durability store (constraint 4, ahead of the Phase 3 snapshot
//! engine).
//!
//! Before any mutation in a turn, the pre-image of every target is stored
//! content-addressed (`blobs/<sha256>`), and the turn's manifest records
//! `rel path -> sha | absent`. For `run_command`, which can touch anything,
//! the whole workspace is captured first (manifest kind `full`).
//!
//! Blobs and manifests are written atomically (temp + fsync + rename). Blobs
//! are verified against their hash on read, and manifest paths are confined
//! to the workspace on restore (Threat 9).
//!
//! The Phase 3 engine (ADR-0006) replaces this behind the same interface.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anchor_policy::audit::hex;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum PreimageError {
    #[error("pre-image store i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pre-image store corrupt: {0}")]
    Corrupt(String),
    #[error(
        "workspace too large for the interim snapshot ({files} files, {bytes} bytes); refusing"
    )]
    TooLarge { files: usize, bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    /// Only the listed files were touched.
    Files,
    /// The full workspace was captured; files not listed did not exist.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub turn: u64,
    pub kind: ManifestKind,
    /// Workspace-relative path -> sha256 hex of the pre-image, or `None` if
    /// the file did not exist before the turn.
    pub entries: BTreeMap<String, Option<String>>,
}

/// Limits for full-workspace capture.
pub const FULL_CAPTURE_MAX_FILES: usize = 50_000;
pub const FULL_CAPTURE_MAX_BYTES: u64 = 2 << 30;

#[derive(Debug)]
pub struct PreimageStore {
    dir: PathBuf,
    current: Option<Manifest>,
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl PreimageStore {
    pub fn open(dir: &Path) -> Result<Self, PreimageError> {
        fs::create_dir_all(dir.join("blobs"))?;
        fs::create_dir_all(dir.join("turns"))?;
        Ok(PreimageStore {
            dir: dir.to_path_buf(),
            current: None,
        })
    }

    fn manifest_path(&self, turn: u64) -> PathBuf {
        self.dir.join("turns").join(format!("{turn:010}.json"))
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<String, PreimageError> {
        let sha = hex(&anchor_fs::sha256(bytes));
        let p = self.dir.join("blobs").join(&sha);
        if !p.exists() {
            atomic_write(&p, bytes)?;
        }
        Ok(sha)
    }

    /// Read a blob and verify it against its name.
    pub fn get_blob(&self, sha: &str) -> Result<Vec<u8>, PreimageError> {
        if !valid_sha(sha) {
            return Err(PreimageError::Corrupt(format!("invalid blob id {sha:?}")));
        }
        let bytes = fs::read(self.dir.join("blobs").join(sha))?;
        if hex(&anchor_fs::sha256(&bytes)) != sha {
            return Err(PreimageError::Corrupt(format!(
                "blob {sha} fails hash check"
            )));
        }
        Ok(bytes)
    }

    fn load_or_new(&mut self, turn: u64, kind: ManifestKind) -> Result<(), PreimageError> {
        if self.current.as_ref().is_some_and(|m| m.turn == turn) {
            return Ok(());
        }
        let p = self.manifest_path(turn);
        self.current = Some(if p.exists() {
            self.manifest(turn)?
        } else {
            Manifest {
                turn,
                kind,
                entries: BTreeMap::new(),
            }
        });
        Ok(())
    }

    fn flush(&self) -> Result<(), PreimageError> {
        if let Some(m) = &self.current {
            let bytes =
                serde_json::to_vec_pretty(m).map_err(|e| PreimageError::Corrupt(e.to_string()))?;
            atomic_write(&self.manifest_path(m.turn), &bytes)?;
        }
        Ok(())
    }

    /// Record the pre-image of one file for `turn` (first record wins).
    pub fn record(
        &mut self,
        turn: u64,
        rel: &Path,
        content: Option<&[u8]>,
    ) -> Result<(), PreimageError> {
        self.load_or_new(turn, ManifestKind::Files)?;
        let key = rel.to_string_lossy().replace('\\', "/");
        if self
            .current
            .as_ref()
            .is_some_and(|m| m.entries.contains_key(&key))
        {
            return Ok(());
        }
        let sha = content.map(|c| self.put_blob(c)).transpose()?;
        if let Some(m) = self.current.as_mut() {
            m.entries.insert(key, sha);
        }
        self.flush()
    }

    /// Capture the whole workspace for `turn` (once per turn).
    pub fn record_full(
        &mut self,
        turn: u64,
        files: impl Iterator<Item = (PathBuf, Vec<u8>)>,
    ) -> Result<(), PreimageError> {
        self.load_or_new(turn, ManifestKind::Full)?;
        if self
            .current
            .as_ref()
            .is_some_and(|m| m.kind == ManifestKind::Full && !m.entries.is_empty())
        {
            return Ok(());
        }
        let mut entries = BTreeMap::new();
        let (mut n, mut bytes) = (0usize, 0u64);
        for (rel, content) in files {
            n += 1;
            bytes += content.len() as u64;
            if n > FULL_CAPTURE_MAX_FILES || bytes > FULL_CAPTURE_MAX_BYTES {
                return Err(PreimageError::TooLarge { files: n, bytes });
            }
            let sha = self.put_blob(&content)?;
            entries.insert(rel.to_string_lossy().replace('\\', "/"), Some(sha));
        }
        if let Some(m) = self.current.as_mut() {
            // Records made earlier in this turn hold the true pre-turn state
            // of those files (the capture above may already include the
            // turn's own edits), so they take precedence.
            for (k, v) in &m.entries {
                entries.insert(k.clone(), v.clone());
            }
            m.kind = ManifestKind::Full;
            m.entries = entries;
        }
        self.flush()
    }

    pub fn turns(&self) -> Result<Vec<u64>, PreimageError> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.dir.join("turns"))? {
            let name = e?.file_name().to_string_lossy().to_string();
            if let Some(n) = name
                .strip_suffix(".json")
                .and_then(|n| n.parse::<u64>().ok())
            {
                out.push(n);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    pub fn manifest(&self, turn: u64) -> Result<Manifest, PreimageError> {
        let bytes = fs::read(self.manifest_path(turn))?;
        let m: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| PreimageError::Corrupt(e.to_string()))?;
        if m.turn != turn {
            return Err(PreimageError::Corrupt("manifest turn mismatch".into()));
        }
        for sha in m.entries.values().flatten() {
            if !valid_sha(sha) {
                return Err(PreimageError::Corrupt(format!("invalid blob id {sha:?}")));
            }
        }
        Ok(m)
    }
}
