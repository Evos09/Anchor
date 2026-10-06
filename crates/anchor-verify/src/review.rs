//! Claim resolution, reviewers and the merge decision (spec §8.2).

use std::collections::{BTreeMap, BTreeSet};

use anchor_protocol::verify::{
    ChangedFile, Claim, ClaimVerdict, FileStatus, ReviewInput, TestOutcome, TestRecord, Verdict,
};
use serde::Serialize;
use serde_json::{Value, json};

/// Whether a workspace-relative path is a test file: under a `tests` or
/// `test` directory, or named `test_*.py` / `*_test.py`.
pub fn is_test_path(path: &str) -> bool {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let file = parts.pop().unwrap_or("");
    parts.iter().any(|d| *d == "tests" || *d == "test")
        || (file.starts_with("test_") && file.ends_with(".py"))
        || file.ends_with("_test.py")
}

/// Evidence references for each claim: `diff:<path>` for each listed file
/// that changed, `test:<id>` for each cited test that ran, and
/// `coverage:<path>` where coverage exists. Claims that resolve to no diff
/// or to no test are rejected before any reviewer sees them (§8.2(b): a
/// claim without an artifact reference is rejected automatically).
pub fn resolve(
    claims: &[Claim],
    changes: &[ChangedFile],
    tests: &[TestRecord],
    coverage: Option<&[anchor_protocol::verify::FileCoverage]>,
) -> (BTreeMap<String, Vec<String>>, Vec<ClaimVerdict>) {
    let changed: BTreeSet<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    let ran: BTreeSet<&str> = tests.iter().map(|t| t.id.as_str()).collect();
    let covered: BTreeSet<&str> = coverage
        .unwrap_or_default()
        .iter()
        .map(|c| c.path.as_str())
        .collect();
    let mut refs = BTreeMap::new();
    let mut rejected = Vec::new();
    for c in claims {
        let mut r = Vec::new();
        let mut has_diff = false;
        let mut has_test = false;
        for f in &c.files {
            if changed.contains(f.as_str()) {
                r.push(format!("diff:{f}"));
                has_diff = true;
            }
            if covered.contains(f.as_str()) {
                r.push(format!("coverage:{f}"));
            }
        }
        for t in &c.tests {
            if ran.contains(t.as_str()) {
                r.push(format!("test:{t}"));
                has_test = true;
            }
        }
        let reason = match (has_diff, has_test) {
            (false, _) => Some("no_diff_for_claim"),
            (_, false) => Some("no_test_for_claim"),
            _ => None,
        };
        if let Some(reason) = reason {
            rejected.push(ClaimVerdict {
                claim_id: c.id.clone(),
                verdict: Verdict::Unverifiable,
                reason_code: reason.into(),
                evidence_refs: r.clone(),
                note: "a claim must name a changed file and a test that ran".into(),
            });
        }
        refs.insert(c.id.clone(), r);
    }
    (refs, rejected)
}

/// A reviewer judges each claim against the diff and the evidence.
pub trait Reviewer {
    fn name(&self) -> &str;
    fn review(&mut self, input: &ReviewInput) -> Result<Vec<ClaimVerdict>, String>;
}

/// Constructs in non-test code that can fake or interfere with test
/// evidence: early process exit, tracer tampering, builtin patching,
/// dynamic imports, test-framework imports.
const SUSPICIOUS: &[&str] = &[
    "os._exit",
    "sys.exit",
    "settrace",
    "gettrace",
    "builtins",
    "__import__",
    "import unittest",
    "from unittest",
];

/// The deterministic baseline reviewer (no model). A claim is supported
/// only if every listed file changed, every cited test ran and passed,
/// every changed executable line in its non-test Python files ran under
/// the tests, and no added non-test line uses a suspicious construct.
///
/// It checks that evidence is consistent, not that behaviour is right: a
/// bug whose tests pass and whose lines all run gets through (the
/// `semantic` category of the seeded-bug corpus).
#[derive(Debug, Default)]
pub struct RuleReviewer;

impl Reviewer for RuleReviewer {
    fn name(&self) -> &str {
        "rule"
    }

    fn review(&mut self, input: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
        let changes: BTreeMap<&str, &ChangedFile> =
            input.changes.iter().map(|c| (c.path.as_str(), c)).collect();
        let tests: BTreeMap<&str, TestOutcome> = input
            .tests
            .iter()
            .map(|t| (t.id.as_str(), t.outcome))
            .collect();
        let coverage: Option<BTreeMap<&str, _>> = input
            .coverage
            .as_ref()
            .map(|cs| cs.iter().map(|c| (c.path.as_str(), c)).collect());
        let mut out = Vec::new();
        for c in &input.claims {
            let refs = input.claim_refs.get(&c.id).cloned().unwrap_or_default();
            let verdict = |v: Verdict, code: &str, note: String| ClaimVerdict {
                claim_id: c.id.clone(),
                verdict: v,
                reason_code: code.into(),
                evidence_refs: refs.clone(),
                note,
            };
            out.push('claim: {
                if input.truncated {
                    break 'claim verdict(
                        Verdict::Unverifiable,
                        "truncated",
                        "the evidence was cut to fit".into(),
                    );
                }
                if let Some(f) = c.files.iter().find(|f| !changes.contains_key(f.as_str())) {
                    break 'claim verdict(
                        Verdict::Refuted,
                        "file_not_changed",
                        format!("{f} is not part of the change"),
                    );
                }
                for t in &c.tests {
                    match tests.get(t.as_str()) {
                        None => {
                            break 'claim verdict(
                                Verdict::Refuted,
                                "cited_test_missing",
                                format!("{t} did not run"),
                            );
                        }
                        Some(TestOutcome::Passed) => {}
                        Some(o) => {
                            break 'claim verdict(
                                Verdict::Refuted,
                                "cited_test_failed",
                                format!("{t}: {o:?}"),
                            );
                        }
                    }
                }
                for f in &c.files {
                    let ch = changes[f.as_str()];
                    if ch.is_test {
                        continue;
                    }
                    if let Some(s) = ch
                        .added_text
                        .iter()
                        .find(|l| SUSPICIOUS.iter().any(|p| l.contains(p)))
                    {
                        break 'claim verdict(
                            Verdict::Refuted,
                            "suspicious_construct",
                            format!("{f}: {}", s.trim()),
                        );
                    }
                    if !f.ends_with(".py")
                        || !matches!(ch.status, FileStatus::Added | FileStatus::Modified)
                        || ch.added_lines.is_empty()
                    {
                        continue;
                    }
                    let Some(cov) = &coverage else {
                        break 'claim verdict(
                            Verdict::Unverifiable,
                            "no_coverage",
                            "no coverage command is configured".into(),
                        );
                    };
                    let missed: Vec<u32> = match cov.get(f.as_str()) {
                        Some(fc) if fc.instrumented => ch
                            .added_lines
                            .iter()
                            .copied()
                            .filter(|l| fc.uncovered_lines.contains(l))
                            .collect(),
                        _ => ch.added_lines.clone(),
                    };
                    if !missed.is_empty() {
                        break 'claim verdict(
                            Verdict::Unverifiable,
                            "uncovered_change",
                            format!("{f}: changed lines {missed:?} never ran under the tests"),
                        );
                    }
                }
                verdict(
                    Verdict::Supported,
                    "evidence_consistent",
                    "diff, tests and coverage agree".into(),
                )
            });
        }
        Ok(out)
    }
}

/// Which stage rejected a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Build,
    Tests,
    Rules,
    Claims,
    Review,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub stage: Stage,
    pub code: String,
    pub detail: Value,
}

impl Finding {
    pub fn new(stage: Stage, code: &str, detail: Value) -> Self {
        Finding {
            stage,
            code: code.into(),
            detail,
        }
    }
}

/// The merge decision (§8.2(d)): approved only when the build and tests
/// pass, every hard rule holds, and the reviewer supports every claim.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub approved: bool,
    pub reviewer: String,
    pub findings: Vec<Finding>,
    pub verdicts: Vec<ClaimVerdict>,
}

/// Decide on a change. `prior` holds findings the caller established
/// (protected paths, regressions, platform); `auto` the claims
/// [`resolve`] rejected. The reviewer runs only if nothing failed before
/// it, so a broken build never costs a review.
pub fn decide(
    input: &ReviewInput,
    prior: Vec<Finding>,
    auto: Vec<ClaimVerdict>,
    reviewer: &mut dyn Reviewer,
) -> Decision {
    let mut findings = prior;
    for cmd in &input.commands {
        if !cmd.ok {
            let stage = if cmd.name == "build" {
                Stage::Build
            } else {
                Stage::Tests
            };
            findings.push(Finding::new(
                stage,
                &format!("{}_failed", cmd.name),
                json!({"exit": cmd.exit, "timed_out": cmd.timed_out, "artifact": cmd.artifact}),
            ));
        }
    }
    if input.tests.is_empty() {
        findings.push(Finding::new(Stage::Tests, "no_tests", json!({})));
    }
    if input.claims.is_empty() {
        findings.push(Finding::new(Stage::Claims, "no_claims", json!({})));
    }
    let claimed: BTreeSet<&str> = input
        .claims
        .iter()
        .flat_map(|c| c.files.iter().map(String::as_str))
        .collect();
    let unclaimed: Vec<&str> = input
        .changes
        .iter()
        .filter(|c| !c.is_test && !claimed.contains(c.path.as_str()))
        .map(|c| c.path.as_str())
        .collect();
    if !unclaimed.is_empty() {
        findings.push(Finding::new(
            Stage::Claims,
            "unclaimed_change",
            json!({"paths": unclaimed}),
        ));
    }
    for v in &auto {
        findings.push(Finding::new(
            Stage::Claims,
            &v.reason_code,
            json!({"claim": v.claim_id}),
        ));
    }
    let mut verdicts = auto;
    if findings.is_empty() {
        match reviewer.review(input) {
            Err(e) => findings.push(Finding::new(
                Stage::Review,
                "reviewer_failed",
                json!({"error": e}),
            )),
            Ok(vs) => {
                let by_id: BTreeMap<&str, &ClaimVerdict> =
                    vs.iter().map(|v| (v.claim_id.as_str(), v)).collect();
                for c in &input.claims {
                    match by_id.get(c.id.as_str()) {
                        None => findings.push(Finding::new(
                            Stage::Review,
                            "missing_verdict",
                            json!({"claim": c.id}),
                        )),
                        Some(v) if v.verdict != Verdict::Supported || input.truncated => findings
                            .push(Finding::new(
                                Stage::Review,
                                &v.reason_code,
                                json!({"claim": c.id, "verdict": v.verdict, "refs": v.evidence_refs}),
                            )),
                        Some(_) => {}
                    }
                }
                verdicts = vs;
            }
        }
    }
    Decision {
        approved: findings.is_empty(),
        reviewer: reviewer.name().to_string(),
        findings,
        verdicts,
    }
}

const FEEDBACK_DIFF_BYTES: usize = 16 << 10;
const FEEDBACK_TAIL_BYTES: usize = 4 << 10;

fn tail(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

impl Decision {
    /// The structured feedback for the executor (§8.2(c)): the failing
    /// output, the relevant diff hunks, and the evidence references that
    /// contradict its claims.
    pub fn feedback(&self, input: &ReviewInput) -> Value {
        let failing_tests: Vec<&str> = input
            .tests
            .iter()
            .filter(|t| matches!(t.outcome, TestOutcome::Failed | TestOutcome::Error))
            .map(|t| t.id.as_str())
            .collect();
        let failed_commands: Vec<Value> = input
            .commands
            .iter()
            .filter(|c| !c.ok)
            .map(|c| {
                json!({"name": c.name, "exit": c.exit, "timed_out": c.timed_out,
                       "output_tail": tail(&c.output_tail, FEEDBACK_TAIL_BYTES),
                       "artifact": c.artifact})
            })
            .collect();
        let rejected: Vec<&ClaimVerdict> = self
            .verdicts
            .iter()
            .filter(|v| v.verdict != Verdict::Supported)
            .collect();
        // Diffs of the files the rejection is about (else all changes).
        let mut wanted: BTreeSet<&str> = BTreeSet::new();
        for v in &rejected {
            if let Some(c) = input.claims.iter().find(|c| c.id == v.claim_id) {
                wanted.extend(c.files.iter().map(String::as_str));
            }
        }
        for f in &self.findings {
            if let Some(ps) = f.detail.get("paths").and_then(Value::as_array) {
                wanted.extend(ps.iter().filter_map(Value::as_str));
            }
        }
        let mut hunks = String::new();
        for c in &input.changes {
            if (wanted.is_empty() || wanted.contains(c.path.as_str()))
                && hunks.len() + c.diff.len() <= FEEDBACK_DIFF_BYTES
            {
                hunks.push_str(&c.diff);
            }
        }
        json!({
            "approved": self.approved,
            "reviewer": self.reviewer,
            "findings": self.findings,
            "failed_commands": failed_commands,
            "failing_tests": failing_tests,
            "rejected_claims": rejected,
            "hunks": hunks,
        })
    }
}

/// Render feedback as text for the executor's next instruction.
pub fn render_feedback(fb: &Value) -> String {
    let mut s = String::from("Your previous change was REJECTED by verification.\n");
    if let Some(fs) = fb["findings"].as_array() {
        for f in fs {
            s.push_str(&format!(
                "- [{}] {}: {}\n",
                f["stage"].as_str().unwrap_or("?"),
                f["code"].as_str().unwrap_or("?"),
                f["detail"]
            ));
        }
    }
    if let Some(ts) = fb["failing_tests"].as_array().filter(|t| !t.is_empty()) {
        s.push_str("Failing tests:\n");
        for t in ts {
            s.push_str(&format!("  {}\n", t.as_str().unwrap_or("?")));
        }
    }
    if let Some(cs) = fb["failed_commands"].as_array() {
        for c in cs {
            s.push_str(&format!(
                "Command `{}` failed (exit {}). Output tail:\n{}\n",
                c["name"].as_str().unwrap_or("?"),
                c["exit"],
                c["output_tail"].as_str().unwrap_or("")
            ));
        }
    }
    if let Some(vs) = fb["rejected_claims"].as_array() {
        for v in vs {
            s.push_str(&format!(
                "Claim {} {}: {} ({})\n",
                v["claim_id"].as_str().unwrap_or("?"),
                v["verdict"].as_str().unwrap_or("?"),
                v["reason_code"].as_str().unwrap_or("?"),
                v["note"].as_str().unwrap_or("")
            ));
        }
    }
    if let Some(h) = fb["hunks"].as_str().filter(|h| !h.is_empty()) {
        s.push_str("Relevant diff:\n");
        s.push_str(h);
    }
    s.push_str(
        "Fix the change, then submit claims again: each claim must name the files it changed \
         and the tests that exercise it.\n",
    );
    s
}
