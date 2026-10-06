use std::collections::BTreeMap;

use anchor_protocol::verify::*;

use super::*;

fn change(path: &str, lines: &[(u32, &str)]) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        status: FileStatus::Modified,
        is_test: is_test_path(path),
        added_lines: lines.iter().map(|l| l.0).collect(),
        removed_lines: 0,
        added_text: lines.iter().map(|l| l.1.to_string()).collect(),
        diff: format!("--- a/{path}\n+++ b/{path}\n"),
    }
}

fn cmd(name: &str, ok: bool) -> CommandRecord {
    CommandRecord {
        name: name.into(),
        argv: vec!["python3".into()],
        ok,
        exit: Some(if ok { 0 } else { 1 }),
        timed_out: false,
        elapsed_ms: 5,
        user_cpu_ms: Some(1),
        max_rss_bytes: Some(1 << 20),
        output_tail: if ok {
            "OK".into()
        } else {
            "AssertionError: 1 != 0".into()
        },
        artifact: format!("evidence:1/1/{name}.log"),
    }
}

fn claim(id: &str, files: &[&str], tests: &[&str]) -> Claim {
    Claim {
        id: id.into(),
        text: format!("claim {id}"),
        files: files.iter().map(|s| s.to_string()).collect(),
        tests: tests.iter().map(|s| s.to_string()).collect(),
    }
}

/// A passing change to `m.py` (lines 2-3) with a test, fully covered.
fn input() -> ReviewInput {
    let claims = vec![claim("c1", &["m.py"], &["test_m.T.test_low"])];
    let changes = vec![
        change("m.py", &[(2, "    if x < lo:"), (3, "        return lo")]),
        change("test_m.py", &[(5, "    def test_low(self):")]),
    ];
    let tests = vec![TestRecord {
        id: "test_m.T.test_low".into(),
        outcome: TestOutcome::Passed,
    }];
    let coverage = vec![FileCoverage {
        path: "m.py".into(),
        instrumented: true,
        uncovered_lines: vec![4, 5],
    }];
    let (claim_refs, _) = resolve(&claims, &changes, &tests, Some(&coverage));
    ReviewInput {
        claims,
        changes,
        commands: vec![cmd("test", true), cmd("coverage", true)],
        tests,
        coverage: Some(coverage),
        claim_refs,
        truncated: false,
    }
}

fn verdict_of(i: &ReviewInput) -> (Verdict, String) {
    let v = RuleReviewer.review(i).unwrap();
    (v[0].verdict, v[0].reason_code.clone())
}

#[test]
fn rule_reviewer_supports_consistent_evidence() {
    let i = input();
    assert_eq!(
        i.claim_refs["c1"],
        ["diff:m.py", "coverage:m.py", "test:test_m.T.test_low"]
    );
    assert_eq!(
        verdict_of(&i),
        (Verdict::Supported, "evidence_consistent".into())
    );
    let d = decide(&i, vec![], vec![], &mut RuleReviewer);
    assert!(d.approved, "{d:?}");
}

#[test]
fn rule_reviewer_rejects_inconsistent_evidence() {
    // A changed line the tests never ran.
    let mut i = input();
    i.changes[0].added_lines.push(4);
    i.changes[0].added_text.push("    if x > hi:".into());
    assert_eq!(
        verdict_of(&i),
        (Verdict::Unverifiable, "uncovered_change".into())
    );
    // A file the tests never imported at all.
    let mut i = input();
    i.coverage.as_mut().unwrap()[0].instrumented = false;
    assert_eq!(verdict_of(&i).1, "uncovered_change");
    // No coverage configured.
    let mut i = input();
    i.coverage = None;
    assert_eq!(verdict_of(&i).1, "no_coverage");
    // A cited test that failed, or never ran.
    let mut i = input();
    i.tests[0].outcome = TestOutcome::Failed;
    assert_eq!(
        verdict_of(&i),
        (Verdict::Refuted, "cited_test_failed".into())
    );
    let mut i = input();
    i.claims[0].tests.push("test_m.T.test_ghost".into());
    assert_eq!(verdict_of(&i).1, "cited_test_missing");
    // A listed file that is not part of the change.
    let mut i = input();
    i.claims[0].files.push("other.py".into());
    assert_eq!(verdict_of(&i).1, "file_not_changed");
    // Suspicious constructs in non-test code.
    let mut i = input();
    i.changes[0].added_text[1] = "        os._exit(0)".into();
    assert_eq!(
        verdict_of(&i),
        (Verdict::Refuted, "suspicious_construct".into())
    );
    // ...but test files may import unittest.
    let mut i = input();
    i.changes[1].added_text[0] = "import unittest".into();
    assert_eq!(verdict_of(&i).0, Verdict::Supported);
    // Truncated evidence is never enough.
    let mut i = input();
    i.truncated = true;
    assert_eq!(verdict_of(&i).1, "truncated");
}

#[test]
fn claims_without_evidence_are_rejected_automatically() {
    let i = input();
    let claims = vec![
        claim("nodiff", &["unchanged.py"], &["test_m.T.test_low"]),
        claim("notest", &["m.py"], &[]),
        claim("ok", &["m.py"], &["test_m.T.test_low"]),
    ];
    let (refs, auto) = resolve(&claims, &i.changes, &i.tests, i.coverage.as_deref());
    let codes: Vec<(&str, &str)> = auto
        .iter()
        .map(|v| (v.claim_id.as_str(), v.reason_code.as_str()))
        .collect();
    assert_eq!(
        codes,
        [
            ("nodiff", "no_diff_for_claim"),
            ("notest", "no_test_for_claim")
        ]
    );
    assert!(refs["nodiff"].iter().all(|r| !r.starts_with("diff:")));
    // The merge decision rejects them without consulting the reviewer.
    let mut i2 = i.clone();
    i2.claims = claims;
    i2.claim_refs = refs;
    let mut r = Counting(0);
    let d = decide(&i2, vec![], auto, &mut r);
    assert!(!d.approved);
    assert_eq!(r.0, 0);
}

/// Counts reviews and supports everything (a rubber stamp).
struct Counting(u32);
impl Reviewer for Counting {
    fn name(&self) -> &str {
        "counting"
    }
    fn review(&mut self, i: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
        self.0 += 1;
        Ok(i.claims
            .iter()
            .map(|c| ClaimVerdict {
                claim_id: c.id.clone(),
                verdict: Verdict::Supported,
                reason_code: "ok".into(),
                evidence_refs: vec![],
                note: String::new(),
            })
            .collect())
    }
}

#[test]
fn decision_requires_build_tests_rules_and_every_claim() {
    // Failing build or tests: rejected, and no review is spent.
    for name in ["build", "test"] {
        let mut i = input();
        i.commands.push(cmd(name, false));
        let mut r = Counting(0);
        let d = decide(&i, vec![], vec![], &mut r);
        assert!(!d.approved);
        assert_eq!(r.0, 0);
        assert_eq!(d.findings[0].code, format!("{name}_failed"));
    }
    // A finding from the caller (e.g. protected paths) blocks too.
    let mut r = Counting(0);
    let d = decide(
        &input(),
        vec![Finding::new(
            Stage::Rules,
            "tests_tampered",
            serde_json::json!({}),
        )],
        vec![],
        &mut r,
    );
    assert!(!d.approved && r.0 == 0);
    // Unclaimed non-test changes.
    let mut i = input();
    i.changes.push(change("extra.py", &[(1, "x = 1")]));
    let d = decide(&i, vec![], vec![], &mut Counting(0));
    assert_eq!(d.findings[0].code, "unclaimed_change");
    // No claims, no tests.
    let mut i = input();
    i.claims.clear();
    i.tests.clear();
    let codes: Vec<String> = decide(&i, vec![], vec![], &mut Counting(0))
        .findings
        .into_iter()
        .map(|f| f.code)
        .collect();
    assert!(codes.contains(&"no_claims".into()) && codes.contains(&"no_tests".into()));
    // A reviewer that skips a claim does not approve it.
    struct Silent;
    impl Reviewer for Silent {
        fn name(&self) -> &str {
            "silent"
        }
        fn review(&mut self, _: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
            Ok(vec![])
        }
    }
    let d = decide(&input(), vec![], vec![], &mut Silent);
    assert_eq!(d.findings[0].code, "missing_verdict");
}

#[test]
fn feedback_carries_failures_hunks_and_refs() {
    let mut i = input();
    i.changes[0].added_lines.push(4);
    i.changes[0].added_text.push("    if x > hi:".into());
    i.changes[0].diff = "--- a/m.py\n+++ b/m.py\n@@ -1,1 +1,4 @@\n+    if x > hi:\n".into();
    i.tests.push(TestRecord {
        id: "test_m.T.test_high".into(),
        outcome: TestOutcome::Failed,
    });
    let d = decide(&i, vec![], vec![], &mut RuleReviewer);
    assert!(!d.approved);
    let fb = d.feedback(&i);
    assert_eq!(fb["failing_tests"][0], "test_m.T.test_high");
    assert_eq!(fb["rejected_claims"][0]["reason_code"], "uncovered_change");
    assert!(fb["hunks"].as_str().unwrap().contains("+    if x > hi:"));
    let text = render_feedback(&fb);
    assert!(text.contains("uncovered_change") && text.contains("test_m.T.test_high"));
    // Feedback is a JSON value the Hub stores; keep it reasonably small.
    assert!(serde_json::to_vec(&fb).unwrap().len() < 64 << 10);
}

#[test]
fn wire_types_validate() {
    let i = input();
    i.validate().unwrap();
    let mut bad = i.clone();
    bad.claims.push(bad.claims[0].clone());
    assert!(bad.validate().unwrap_err().contains("duplicate"));
    let v = ClaimVerdict {
        claim_id: "c1".into(),
        verdict: Verdict::Supported,
        reason_code: "Bad Code".into(),
        evidence_refs: vec![],
        note: String::new(),
    };
    assert!(v.validate().is_err());
    let mut c = claim("c 1", &[], &[]);
    assert!(c.validate().is_err());
    c.id = "c1".into();
    c.validate().unwrap();
    // Verdicts round-trip in the ADR-0009 shape.
    let j = serde_json::to_value(RuleReviewer.review(&i).unwrap()).unwrap();
    assert_eq!(j[0]["verdict"], "supported");
    let back: Vec<ClaimVerdict> = serde_json::from_value(j).unwrap();
    validate_verdicts(&back).unwrap();
    let _ = BTreeMap::<String, String>::new();
}

#[test]
fn test_paths() {
    for p in [
        "tests/test_m.py",
        "pkg/tests/helpers.py",
        "test_m.py",
        "m_test.py",
    ] {
        assert!(is_test_path(p), "{p}");
    }
    for p in ["m.py", "testing.py", "pkg/contest.py", "README.md"] {
        assert!(!is_test_path(p), "{p}");
    }
}
