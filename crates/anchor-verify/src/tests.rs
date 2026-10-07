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

#[test]
fn runner_tampering_is_found() {
    let code = change(
        "calc.py",
        &[
            (1, "import os"),
            (2, "os._exit(0)"),
            (3, "pat = re.compile(r'x')  # eval( in a comment"),
            (4, "import unittest"),
            (5, "print('x', file=sys.stderr)"),
            (6, "my_builtins_list = []"),
            (7, "def run(): return 1"),
        ],
    );
    let test = change(
        "tests/test_calc.py",
        &[
            (1, "import unittest"),
            (2, "unittest.TestResult.addFailure = lambda *a: None"),
            (3, "from os import _exit"),
        ],
    );
    assert_eq!(
        rules::runner_tampering(&[code, test]),
        [
            "calc.py:2: _exit",
            "calc.py:4: unittest",
            "calc.py:5: stderr",
            "tests/test_calc.py:2: TestResult",
            "tests/test_calc.py:3: _exit",
        ]
    );
}

#[test]
fn startup_hooks_and_unreviewable_code_are_found() {
    let mut big = change("big.py", &[]);
    big.status = FileStatus::Opaque;
    let mut gone = change("old/sitecustomize.py", &[]);
    gone.status = FileStatus::Removed;
    let all = [
        change("pkg/evil.pth", &[]),
        change("lib/sitecustomize.py", &[]),
        change("usercustomize/__init__.py", &[]),
        change("calc/__pycache__/calc.cpython-312.pyc", &[]),
        change("tests/conftest.py", &[]),
        change("calc.py", &[]),
        change("calc.cpython-312-x86_64-linux-gnu.so", &[]),
        big,
        gone,
    ];
    assert_eq!(
        rules::startup_hooks(&all),
        [
            "pkg/evil.pth",
            "lib/sitecustomize.py",
            "usercustomize/__init__.py",
            "calc/__pycache__/calc.cpython-312.pyc",
            "tests/conftest.py",
        ]
    );
    assert_eq!(
        rules::unreviewable_code(&all),
        ["calc.cpython-312-x86_64-linux-gnu.so", "big.py"]
    );
}

/// Gives a fixed verdict to every claim, or leaves claims out.
struct Fixed(&'static str, Option<Verdict>);
impl Reviewer for Fixed {
    fn name(&self) -> &str {
        self.0
    }
    fn review(&mut self, i: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
        Ok(match self.1 {
            None => vec![],
            Some(v) => i
                .claims
                .iter()
                .map(|c| ClaimVerdict {
                    claim_id: c.id.clone(),
                    verdict: v,
                    reason_code: format!("{}_says", self.0),
                    evidence_refs: vec![],
                    note: "why".into(),
                })
                .collect(),
        })
    }
}

/// Phase 8: a plugin is an extra gate. The worst verdict per claim
/// stands, so a plugin can block a merge but never approve one.
#[test]
fn combined_reviewers_take_the_worst_verdict() {
    use Verdict::*;
    let i = input();
    let mut all = Combined::new(vec![
        Box::new(Fixed("rule", Some(Supported))),
        Box::new(Fixed("plugin", Some(Supported))),
    ]);
    assert_eq!(all.name(), "rule+plugin");
    assert!(decide(&i, vec![], vec![], &mut all).approved);

    // The plugin refutes what the rule reviewer supports: rejected.
    let mut block = Combined::new(vec![
        Box::new(Fixed("rule", Some(Supported))),
        Box::new(Fixed("plugin", Some(Refuted))),
    ]);
    let d = decide(&i, vec![], vec![], &mut block);
    assert!(!d.approved);
    assert_eq!(d.verdicts[0].reason_code, "plugin_says");
    assert!(d.verdicts[0].note.starts_with("[plugin] "));

    // The plugin supports what the rule reviewer refutes: still rejected.
    let mut no_override = Combined::new(vec![
        Box::new(Fixed("rule", Some(Refuted))),
        Box::new(Fixed("plugin", Some(Supported))),
    ]);
    let d = decide(&i, vec![], vec![], &mut no_override);
    assert!(!d.approved);
    assert_eq!(d.verdicts[0].reason_code, "rule_says");

    // Refuted outranks unverifiable, whichever comes first.
    let mut r = Combined::new(vec![
        Box::new(Fixed("a", Some(Unverifiable))),
        Box::new(Fixed("b", Some(Refuted))),
    ]);
    assert_eq!(r.review(&i).unwrap()[0].verdict, Refuted);

    // A reviewer that leaves the claim out makes it unverifiable.
    let mut silent = Combined::new(vec![
        Box::new(Fixed("rule", Some(Supported))),
        Box::new(Fixed("quiet", None)),
    ]);
    let vs = silent.review(&i).unwrap();
    assert_eq!(vs[0].verdict, Unverifiable);
    assert_eq!(vs[0].reason_code, "missing_verdict");
    assert!(!decide(&i, vec![], vec![], &mut silent).approved);
}
