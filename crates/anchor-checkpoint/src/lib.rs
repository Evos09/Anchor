//! Context checkpointing (spec §5.4).
//!
//! * [`ContextTracker`] follows context-window usage per model and reports
//!   when the configured threshold (default 80%) is reached. The agent must
//!   then **pause** — never compact or truncate silently.
//! * [`CheckpointStore`] persists checkpoints atomically: write to a
//!   temporary file in the same directory, fsync, rename, fsync the
//!   directory. A checkpoint file either exists in full or not at all, so a
//!   process killed mid-turn always leaves the previous valid checkpoint.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anchor_protocol::{Checkpoint, Validate};

pub const DEFAULT_THRESHOLD: f64 = 0.80;
const TMP_PREFIX: &str = ".tmp-";

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("threshold must be in (0, 1], got {0}")]
    BadThreshold(f64),
    #[error("invalid checkpoint: {0}")]
    Invalid(String),
    #[error("checkpoint i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("checkpoint parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContextStatus {
    Ok { ratio: f64 },
    PauseRequired { ratio: f64 },
}

#[derive(Debug, Clone)]
pub struct ContextTracker {
    threshold: f64,
    usage: HashMap<String, (u64, u64)>,
}

impl ContextTracker {
    pub fn new(threshold: f64) -> Result<Self, CheckpointError> {
        if !(threshold > 0.0 && threshold <= 1.0) {
            return Err(CheckpointError::BadThreshold(threshold));
        }
        Ok(ContextTracker {
            threshold,
            usage: HashMap::new(),
        })
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Record usage for a model and say whether the agent must pause.
    pub fn update(&mut self, model_id: &str, used_tokens: u64, window_tokens: u64) -> ContextStatus {
        self.usage
            .insert(model_id.to_string(), (used_tokens, window_tokens.max(1)));
        let ratio = used_tokens as f64 / window_tokens.max(1) as f64;
        if ratio >= self.threshold {
            ContextStatus::PauseRequired { ratio }
        } else {
            ContextStatus::Ok { ratio }
        }
    }

    pub fn ratio(&self, model_id: &str) -> Option<f64> {
        self.usage
            .get(model_id)
            .map(|(u, w)| *u as f64 / *w as f64)
    }
}

#[derive(Debug)]
pub struct CheckpointStore {
    dir: PathBuf,
}

/// Summary of a stored checkpoint.
/// Valid checkpoints, and unreadable files with the reason.
pub type Listing = (Vec<CheckpointInfo>, Vec<(PathBuf, String)>);

#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointInfo {
    pub checkpoint_id: String,
    pub session_id: String,
    pub turn: u64,
    pub path: PathBuf,
}

impl CheckpointStore {
    /// Open a store, creating the directory and removing temporary files left
    /// by a process that was killed mid-write.
    pub fn open(dir: &Path) -> Result<Self, CheckpointError> {
        fs::create_dir_all(dir)?;
        for ent in fs::read_dir(dir)? {
            let ent = ent?;
            if ent.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                let _ = fs::remove_file(ent.path());
            }
        }
        Ok(CheckpointStore {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Persist a checkpoint atomically. Returns its path.
    pub fn save(&self, cp: &Checkpoint) -> Result<PathBuf, CheckpointError> {
        cp.validate().map_err(CheckpointError::Invalid)?;
        let bytes = serde_json::to_vec_pretty(cp).map_err(|e| CheckpointError::Parse(e.to_string()))?;
        let final_path = self.path_for(&cp.checkpoint_id);
        let tmp = self
            .dir
            .join(format!("{TMP_PREFIX}{}-{}", std::process::id(), cp.checkpoint_id));
        let result = (|| -> std::io::Result<()> {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, &final_path)?;
            sync_dir(&self.dir)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(final_path)
    }

    pub fn load(&self, checkpoint_id: &str) -> Result<Checkpoint, CheckpointError> {
        // Validate the id shape before using it as a file name.
        if checkpoint_id.is_empty()
            || checkpoint_id.len() > 128
            || !checkpoint_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(CheckpointError::Invalid("bad checkpoint id".into()));
        }
        Self::load_path(&self.path_for(checkpoint_id))
    }

    pub fn load_path(path: &Path) -> Result<Checkpoint, CheckpointError> {
        let bytes = fs::read(path)?;
        let cp: Checkpoint =
            serde_json::from_slice(&bytes).map_err(|e| CheckpointError::Parse(e.to_string()))?;
        cp.validate().map_err(CheckpointError::Invalid)?;
        Ok(cp)
    }

    /// All valid checkpoints, ordered by (session, turn). Invalid files are
    /// reported in the second vector rather than silently skipped.
    pub fn list(&self) -> Result<Listing, CheckpointError> {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for ent in fs::read_dir(&self.dir)? {
            let path = ent?.path();
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name.starts_with(TMP_PREFIX) || !name.ends_with(".json") {
                continue;
            }
            match Self::load_path(&path) {
                Ok(cp) => ok.push(CheckpointInfo {
                    checkpoint_id: cp.checkpoint_id,
                    session_id: cp.session_id,
                    turn: cp.turn,
                    path,
                }),
                Err(e) => bad.push((path, e.to_string())),
            }
        }
        ok.sort_by(|a, b| {
            (a.session_id.as_str(), a.turn, a.checkpoint_id.as_str()).cmp(&(
                b.session_id.as_str(),
                b.turn,
                b.checkpoint_id.as_str(),
            ))
        });
        Ok((ok, bad))
    }

    /// The most recent valid checkpoint (highest turn) for a session, or
    /// across all sessions.
    pub fn latest(&self, session_id: Option<&str>) -> Result<Option<Checkpoint>, CheckpointError> {
        let (all, _) = self.list()?;
        let best = all
            .into_iter()
            .filter(|c| session_id.is_none_or(|s| s == c.session_id))
            .max_by_key(|c| c.turn);
        match best {
            Some(info) => Ok(Some(Self::load_path(&info.path)?)),
            None => Ok(None),
        }
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    // Directory handles cannot be fsynced through std on Windows; the rename
    // is still atomic (MoveFileEx semantics).
    Ok(())
}

#[cfg(test)]
mod tests;
