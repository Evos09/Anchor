//! Verification wire types (spec §8, ADR-0024): what the executor claims,
//! what a reviewer sees, and what it answers. The shapes follow the
//! verifier interface draft (ADR-0009): claims, a diff, evidence, and
//! structured per-claim verdicts whose free text is advisory only.
//!
//! The executor's claims are *selectors* (files and tests). It cannot cite
//! evidence, because evidence is produced after it finishes; the backend
//! resolves selectors to evidence references.

use serde::{Deserialize, Serialize};

use crate::{MAX_CONTENT_BYTES, MAX_PARAM_BYTES, check_param};

/// At most this many claims per change.
pub const MAX_CLAIMS: usize = 64;
/// At most this many files or tests per claim.
pub const MAX_CLAIM_SELECTORS: usize = 64;
/// Claim and note text cap.
pub const MAX_CLAIM_TEXT: usize = 2048;
/// The whole review input stays well inside one frame.
pub const MAX_REVIEW_BYTES: usize = MAX_CONTENT_BYTES / 2;

/// One thing the executor says its change does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    /// Short id chosen by the executor, unique within the change.
    pub id: String,
    pub text: String,
    /// Workspace-relative files whose changes implement the claim.
    #[serde(default)]
    pub files: Vec<String>,
    /// Ids of tests that exercise the claim (as the test runner names
    /// them, e.g. `test_m.T.test_low`).
    #[serde(default)]
    pub tests: Vec<String>,
}

impl Claim {
    pub fn validate(&self) -> Result<(), String> {
        check_id("claim id", &self.id)?;
        check_text("claim text", &self.text)?;
        if self.files.len() > MAX_CLAIM_SELECTORS || self.tests.len() > MAX_CLAIM_SELECTORS {
            return Err(format!(
                "claim {}: at most {MAX_CLAIM_SELECTORS} files and tests",
                self.id
            ));
        }
        for f in &self.files {
            check_param("claim file", f)?;
        }
        for t in &self.tests {
            check_param("claim test", t)?;
        }
        Ok(())
    }
}

pub fn validate_claims(claims: &[Claim]) -> Result<(), String> {
    if claims.len() > MAX_CLAIMS {
        return Err(format!("at most {MAX_CLAIMS} claims"));
    }
    let mut ids = std::collections::BTreeSet::new();
    for c in claims {
        c.validate()?;
        if !ids.insert(c.id.as_str()) {
            return Err(format!("duplicate claim id {:?}", c.id));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Removed,
    /// Not diffed line by line (binary or oversized).
    Opaque,
}

/// One changed file, as the reviewer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
    /// Whether the path is a test file (by the project's test patterns).
    pub is_test: bool,
    /// 1-based new-side line numbers this change adds.
    pub added_lines: Vec<u32>,
    pub removed_lines: u32,
    /// The text of each added line, aligned with `added_lines`.
    pub added_text: Vec<String>,
    /// Unified diff, possibly cut short (then `truncated` is set on the
    /// review input).
    pub diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRecord {
    pub id: String,
    pub outcome: TestOutcome,
}

/// One command the tester ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRecord {
    /// `build`, `test` or `coverage`.
    pub name: String,
    pub argv: Vec<String>,
    pub ok: bool,
    /// Exit code; `None` if killed (signal or timeout).
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub user_cpu_ms: Option<u64>,
    pub max_rss_bytes: Option<u64>,
    /// The end of stdout and stderr.
    pub output_tail: String,
    /// The evidence artifact holding the full output.
    pub artifact: String,
}

/// Line coverage of one source file under the test run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCoverage {
    pub path: String,
    /// False if the file never ran under the tests at all.
    pub instrumented: bool,
    /// Executable lines that never ran (1-based).
    pub uncovered_lines: Vec<u32>,
}

/// Everything a reviewer gets: the claims, the diff and the evidence. A
/// reviewer has no workspace access (ADR-0009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewInput {
    pub claims: Vec<Claim>,
    pub changes: Vec<ChangedFile>,
    pub commands: Vec<CommandRecord>,
    pub tests: Vec<TestRecord>,
    /// `None` when no coverage command is configured.
    pub coverage: Option<Vec<FileCoverage>>,
    /// Evidence references resolved for each claim, by claim id.
    pub claim_refs: std::collections::BTreeMap<String, Vec<String>>,
    /// Some content was cut to fit; verdicts may not be `supported`.
    pub truncated: bool,
}

impl ReviewInput {
    pub fn validate(&self) -> Result<(), String> {
        validate_claims(&self.claims)?;
        let size = serde_json::to_vec(self)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > MAX_REVIEW_BYTES {
            return Err(format!("review input exceeds {MAX_REVIEW_BYTES} bytes"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Supported,
    Refuted,
    Unverifiable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimVerdict {
    pub claim_id: String,
    pub verdict: Verdict,
    /// Machine-readable reason (`snake_case`), e.g. `uncovered_change`.
    pub reason_code: String,
    /// The evidence references the verdict rests on.
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// Advisory explanation; never parsed for decisions.
    #[serde(default)]
    pub note: String,
}

impl ClaimVerdict {
    pub fn validate(&self) -> Result<(), String> {
        check_id("claim_id", &self.claim_id)?;
        if self.reason_code.is_empty()
            || self.reason_code.len() > 64
            || !self
                .reason_code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err("reason_code must be 1..=64 bytes of [a-z0-9_]".into());
        }
        if self.evidence_refs.len() > MAX_CLAIM_SELECTORS {
            return Err(format!("at most {MAX_CLAIM_SELECTORS} evidence refs"));
        }
        for r in &self.evidence_refs {
            check_param("evidence ref", r)?;
        }
        if self.note.len() > MAX_CLAIM_TEXT || self.note.contains('\0') {
            return Err("note too long or contains NUL".into());
        }
        Ok(())
    }
}

pub fn validate_verdicts(v: &[ClaimVerdict]) -> Result<(), String> {
    if v.len() > MAX_CLAIMS {
        return Err(format!("at most {MAX_CLAIMS} verdicts"));
    }
    v.iter().try_for_each(ClaimVerdict::validate)
}

fn check_id(name: &str, s: &str) -> Result<(), String> {
    if s.is_empty()
        || s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(format!("{name} must be 1..=64 bytes of [A-Za-z0-9_.-]"));
    }
    Ok(())
}

fn check_text(name: &str, s: &str) -> Result<(), String> {
    check_param(name, s)?;
    if s.len() > MAX_CLAIM_TEXT.min(MAX_PARAM_BYTES) {
        return Err(format!("{name} exceeds {MAX_CLAIM_TEXT} bytes"));
    }
    Ok(())
}
