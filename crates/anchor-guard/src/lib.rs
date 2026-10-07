//! Append-only file guard and session read ledger (spec §5.3).
//!
//! Rules enforced here (the backend calls these before minting a mutation):
//!
//! * On an append-only file, `write_file` is blocked outright and the only
//!   permitted edit is a pure suffix append, verified byte-for-byte.
//! * On any other existing file, a write or edit requires that the *entire*
//!   file was read in this session **and** that it has not changed since, or
//!   that the user confirmed an explicit diff of what will be deleted.
//!
//! Append-only status comes from (ADR-0010):
//!
//! 1. explicit marking in `.anchor/workspace.toml` (`append_only` globs),
//! 2. an in-file marker `anchor:append-only` in the first 4 KiB,
//! 3. heuristics: well-known file names, or text files that are
//!    predominantly dated entries.
//!
//! A heuristic match can be overridden with a persisted `not_append_only`
//! entry; the override is surfaced as [`AppendOnlyStatus::Overridden`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MARKER: &str = "anchor:append-only";
const MARKER_SCAN_BYTES: usize = 4096;

/// Well-known append-only file stems (case-insensitive, extension ignored).
pub const APPEND_ONLY_STEMS: &[&str] = &["DEVLOG", "CHANGELOG", "CHANGES", "HISTORY", "JOURNAL"];

/// Extensions eligible for the dated-entries heuristic (plain text only, so
/// date-keyed data files such as CSVs are not flagged).
const TEXT_EXTENSIONS: &[&str] = &["", "md", "markdown", "txt", "rst", "org", "log", "adoc"];

static DATED_LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)] // constant pattern, covered by tests
    regex::Regex::new(r"^\s{0,3}(?:#{1,6}\s+|[-*]\s+|\[)?\d{4}-\d{2}-\d{2}\b").unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeuristicReason {
    FileName,
    DatedEntries,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "reason")]
pub enum AppendOnlyStatus {
    No,
    /// Listed in `append_only` in the workspace config.
    Explicit,
    /// Contains the in-file marker.
    Marker,
    /// Flagged by a heuristic.
    Heuristic(HeuristicReason),
    /// A heuristic flagged it but the user overrode it ("not append-only").
    Overridden(HeuristicReason),
}

impl AppendOnlyStatus {
    pub fn is_append_only(self) -> bool {
        matches!(
            self,
            AppendOnlyStatus::Explicit | AppendOnlyStatus::Marker | AppendOnlyStatus::Heuristic(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("file is append-only: write_file is blocked; use an append edit")]
    AppendOnlyWriteBlocked,
    #[error("file is append-only: only a pure suffix append is permitted")]
    AppendOnlyModification,
    #[error("file was not fully read in this session; read it first or confirm a diff")]
    NotReadInSession,
    #[error("file changed since it was read in this session; read it again")]
    StaleRead,
    #[error("workspace config error: {0}")]
    Config(String),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Workspace-relative globs of files that are append-only.
    #[serde(default)]
    pub append_only: Vec<String>,
    /// Workspace-relative paths whose heuristic append-only status the user
    /// overrode.
    #[serde(default)]
    pub not_append_only: Vec<String>,
    /// Workspace-relative paths a heuristic has flagged at some point.
    /// They stay append-only even if the content no longer matches (an
    /// agent could otherwise append headings until the heuristic stops
    /// matching, then rewrite the file; review finding P-1). Kept in the
    /// state directory, out of agents' reach; only `not_append_only`
    /// clears it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_append_only: Vec<String>,
    /// Snapshot settings, interpreted by the backend (ADR-0006). Kept
    /// opaque here so the guard round-trips it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<toml::Table>,
}

/// How a write was authorised by the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteBasis {
    NewFile,
    FullReadInSession,
    UserConfirmedDiff,
}

#[derive(Debug)]
pub struct Guard {
    config_path: PathBuf,
    config: WorkspaceConfig,
    explicit: GlobSet,
    /// canonical path -> sha256 of the full content the agent last read
    /// (or wrote) in this session.
    ledger: HashMap<PathBuf, [u8; 32]>,
    /// [`WorkspaceConfig::detected_append_only`], updated on detection.
    detected: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

fn rel_key(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

impl Guard {
    /// Load the workspace config (a missing file means defaults).
    pub fn load(config_path: &Path) -> Result<Self, GuardError> {
        let config = match std::fs::read_to_string(config_path) {
            Ok(t) => toml::from_str(&t).map_err(|e| GuardError::Config(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => WorkspaceConfig::default(),
            Err(e) => return Err(GuardError::Config(e.to_string())),
        };
        Self::with_config(config_path, config)
    }

    pub fn with_config(config_path: &Path, config: WorkspaceConfig) -> Result<Self, GuardError> {
        let mut b = GlobSetBuilder::new();
        for g in &config.append_only {
            let glob = GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| GuardError::Config(format!("bad append_only glob {g:?}: {e}")))?;
            b.add(glob);
        }
        Ok(Guard {
            config_path: config_path.to_path_buf(),
            explicit: b.build().map_err(|e| GuardError::Config(e.to_string()))?,
            detected: std::sync::Mutex::new(config.detected_append_only.iter().cloned().collect()),
            config,
            ledger: HashMap::new(),
        })
    }

    pub fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    /// Heuristic classification alone.
    pub fn heuristic(rel: &Path, content: Option<&[u8]>) -> Option<HeuristicReason> {
        let file_name = rel.file_name()?.to_string_lossy().to_string();
        let stem = file_name
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if APPEND_ONLY_STEMS.contains(&stem.as_str()) {
            return Some(HeuristicReason::FileName);
        }
        let ext = rel
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
            return None;
        }
        let text = std::str::from_utf8(content?).ok()?;
        let mut dated = 0usize;
        let mut entries = 0usize;
        for line in text.lines() {
            let is_dated = DATED_LINE.is_match(line);
            let is_heading = line.trim_start().starts_with('#');
            if is_dated {
                dated += 1;
            }
            if is_dated || is_heading {
                entries += 1;
            }
        }
        if dated >= 3 && dated * 2 >= entries {
            Some(HeuristicReason::DatedEntries)
        } else {
            None
        }
    }

    /// Classify a file. `rel` is workspace-relative and canonical.
    pub fn classify(&self, rel: &Path, content: Option<&[u8]>) -> AppendOnlyStatus {
        let key = rel_key(rel);
        if self.explicit.is_match(&key) {
            return AppendOnlyStatus::Explicit;
        }
        if let Some(c) = content {
            let head = &c[..c.len().min(MARKER_SCAN_BYTES)];
            if head.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()) {
                return AppendOnlyStatus::Marker;
            }
        }
        let found = Self::heuristic(rel, content);
        let mut detected = self.detected.lock().unwrap_or_else(|p| p.into_inner());
        let reason = match found {
            Some(r) => {
                if detected.insert(key.clone()) {
                    // Best effort: a failed save keeps it for this session.
                    let mut cfg = self.config.clone();
                    cfg.detected_append_only = detected.iter().cloned().collect();
                    let _ = write_config(&self.config_path, &cfg);
                }
                Some(r)
            }
            None if detected.contains(&key) => Some(HeuristicReason::DatedEntries),
            None => None,
        };
        match reason {
            Some(r) if self.config.not_append_only.contains(&key) => {
                AppendOnlyStatus::Overridden(r)
            }
            Some(r) => AppendOnlyStatus::Heuristic(r),
            None => AppendOnlyStatus::No,
        }
    }

    /// Record that the agent read the *entire* file with this content hash.
    pub fn record_full_read(&mut self, canonical: &Path, sha256: [u8; 32]) {
        self.ledger.insert(canonical.to_path_buf(), sha256);
    }

    /// Record the content the agent itself just wrote (it knows it fully).
    pub fn record_own_write(&mut self, canonical: &Path, sha256: [u8; 32]) {
        self.ledger.insert(canonical.to_path_buf(), sha256);
    }

    fn require_read(
        &self,
        canonical: &Path,
        current_sha: [u8; 32],
        diff_confirmed: bool,
    ) -> Result<WriteBasis, GuardError> {
        match self.ledger.get(canonical) {
            Some(h) if *h == current_sha => Ok(WriteBasis::FullReadInSession),
            _ if diff_confirmed => Ok(WriteBasis::UserConfirmedDiff),
            Some(_) => Err(GuardError::StaleRead),
            None => Err(GuardError::NotReadInSession),
        }
    }

    /// Gate a whole-file write. `current_sha` is `None` for a new file.
    pub fn check_write(
        &self,
        canonical: &Path,
        status: AppendOnlyStatus,
        current_sha: Option<[u8; 32]>,
        diff_confirmed: bool,
    ) -> Result<WriteBasis, GuardError> {
        if status.is_append_only() {
            return Err(GuardError::AppendOnlyWriteBlocked);
        }
        match current_sha {
            None => Ok(WriteBasis::NewFile),
            Some(h) => self.require_read(canonical, h, diff_confirmed),
        }
    }

    /// Gate an edit producing `new` from `original`.
    pub fn check_edit(
        &self,
        canonical: &Path,
        status: AppendOnlyStatus,
        original: &[u8],
        new: &[u8],
        diff_confirmed: bool,
    ) -> Result<WriteBasis, GuardError> {
        if status.is_append_only() {
            if !is_pure_append(original, new) {
                return Err(GuardError::AppendOnlyModification);
            }
            // Appending to an append-only file does not require having read
            // it: nothing existing can be lost.
            return Ok(WriteBasis::FullReadInSession);
        }
        let current: [u8; 32] = Sha256::digest(original).into();
        self.require_read(canonical, current, diff_confirmed)
    }

    /// Persist a "not append-only" override (spec §5.3(d)). Written
    /// atomically; survives across sessions.
    pub fn mark_not_append_only(&mut self, rel: &Path) -> Result<(), GuardError> {
        let key = rel_key(rel);
        if !self.config.not_append_only.contains(&key) {
            self.config.not_append_only.push(key);
            self.config.not_append_only.sort();
        }
        self.save()
    }

    fn save(&self) -> Result<(), GuardError> {
        let mut cfg = self.config.clone();
        cfg.detected_append_only = self
            .detected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        write_config(&self.config_path, &cfg)
    }
}

/// Write the workspace config atomically.
fn write_config(config_path: &Path, config: &WorkspaceConfig) -> Result<(), GuardError> {
    let text = toml::to_string_pretty(config).map_err(|e| GuardError::Config(e.to_string()))?;
    let dir = config_path
        .parent()
        .ok_or_else(|| GuardError::Config("config path has no parent".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| GuardError::Config(e.to_string()))?;
    let tmp = dir.join(format!(".workspace.toml.tmp-{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, config_path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        GuardError::Config(e.to_string())
    })
}

/// True iff `new` starts with exactly `original`, checked both by hash and
/// byte-for-byte.
pub fn is_pure_append(original: &[u8], new: &[u8]) -> bool {
    if new.len() < original.len() {
        return false;
    }
    let prefix = &new[..original.len()];
    Sha256::digest(prefix) == Sha256::digest(original) && prefix == original
}

#[cfg(test)]
mod tests;
