//! Parsers for tester output: Python `unittest -v` results and `trace`
//! line-coverage files.
//!
//! Test output comes from the code under test's own process, so these
//! results can be forged by that code (ADR-0024, residual risk). The test
//! command's exit status is authoritative for pass/fail; per-test results
//! serve claim resolution and feedback.

use std::collections::BTreeMap;

use anchor_protocol::verify::{TestOutcome, TestRecord};

/// The unittest summary (`Ran N tests`, `OK`/`FAILED (...)`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestSummary {
    pub ran: u32,
    pub failures: u32,
    pub errors: u32,
    pub skipped: u32,
    /// `OK` (possibly with skips) was printed.
    pub ok: bool,
}

/// `name (id)` at the start of a unittest -v result line. Python 3.11+
/// prints the full id in the parentheses; earlier versions print only the
/// class (`test_m.T`), so the method name is appended.
fn test_id(head: &str) -> Option<String> {
    let open = head.find(" (")?;
    let name = &head[..open];
    let rest = head[open + 2..].strip_suffix(')')?;
    let ident = |s: &str, dots: bool| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || c == '_' || (dots && c == '.'))
    };
    if !ident(name, false) || !ident(rest, true) {
        return None;
    }
    if rest.ends_with(&format!(".{name}")) {
        Some(rest.to_string())
    } else {
        Some(format!("{rest}.{name}"))
    }
}

fn outcome(word: &str) -> Option<TestOutcome> {
    let w = word.trim();
    match w {
        "ok" | "expected failure" => Some(TestOutcome::Passed),
        "FAIL" | "unexpected success" => Some(TestOutcome::Failed),
        "ERROR" => Some(TestOutcome::Error),
        _ if w.starts_with("skipped") => Some(TestOutcome::Skipped),
        _ => None,
    }
}

/// Parse `python -m unittest -v` output (unittest writes to stderr).
pub fn parse_unittest(output: &str) -> (Vec<TestRecord>, TestSummary) {
    let mut results: BTreeMap<String, TestOutcome> = BTreeMap::new();
    let mut pending: Option<String> = None;
    let mut sum = TestSummary::default();
    for line in output.lines() {
        // Failure detail headers are definitive.
        for (prefix, o) in [
            ("FAIL: ", TestOutcome::Failed),
            ("ERROR: ", TestOutcome::Error),
        ] {
            if let Some(id) = line.strip_prefix(prefix).and_then(test_id) {
                results.insert(id, o);
            }
        }
        if let Some((head, tail)) = line.split_once(" ... ") {
            let id = test_id(head).or_else(|| pending.take());
            if let Some(id) = id {
                match outcome(tail) {
                    Some(o) => {
                        results.entry(id).or_insert(o);
                    }
                    // Output the test itself printed; the result follows.
                    None => pending = Some(id),
                }
                continue;
            }
        }
        if let Some(id) = test_id(line) {
            // A docstring line follows; its " ... " line carries the result.
            pending = Some(id);
            continue;
        }
        if let Some(id) = &pending
            && let Some(o) = outcome(line)
        {
            results.entry(id.clone()).or_insert(o);
            pending = None;
            continue;
        }
        if let Some(n) = line
            .strip_prefix("Ran ")
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse().ok())
        {
            sum.ran = n;
        } else if line == "OK" || line.starts_with("OK (") {
            sum.ok = true;
            sum.skipped = count(line, "skipped");
        } else if line.starts_with("FAILED (") {
            sum.failures = count(line, "failures");
            sum.errors = count(line, "errors");
            sum.skipped = count(line, "skipped");
        }
    }
    let records = results
        .into_iter()
        .map(|(id, outcome)| TestRecord { id, outcome })
        .collect();
    (records, sum)
}

fn count(line: &str, key: &str) -> u32 {
    line.split(['(', ')', ','])
        .filter_map(|p| p.trim().strip_prefix(&format!("{key}=")))
        .find_map(|n| n.parse().ok())
        .unwrap_or(0)
}

/// The `trace --coverdir` file name for a workspace-relative Python
/// source (`pkg/m.py` → `pkg.m.cover`); `None` for non-Python paths.
pub fn cover_name(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".py")?;
    if stem.is_empty() || stem.split('/').any(|c| c.is_empty() || c.contains('.')) {
        return None;
    }
    Some(format!("{}.cover", stem.replace('/', ".")))
}

/// Lines `trace --missing` marked as executable but never run (1-based).
/// A `.cover` file reproduces every source line in order.
pub fn parse_cover(text: &str) -> Vec<u32> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with(">>>>>> "))
        .map(|(i, _)| i as u32 + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unittest_results_and_summary() {
        let out = "\
test_high (test_m.T.test_high) ... ok
test_low (test_m.T.test_low) ... FAIL
test_doc (test_m.T.test_doc)
Checks the docstring case. ... ok
test_skip (test_m.T.test_skip) ... skipped 'later'
test_noisy (test_m.T.test_noisy) ... printed by the test
ERROR
test_old (test_m.T) ... ok

======================================================================
FAIL: test_low (test_m.T.test_low)
----------------------------------------------------------------------
Traceback (most recent call last):
AssertionError: 1 != 0

----------------------------------------------------------------------
Ran 6 tests in 0.001s

FAILED (failures=1, errors=1, skipped=1)
";
        let (r, s) = parse_unittest(out);
        let get = |id: &str| r.iter().find(|t| t.id == id).map(|t| t.outcome);
        assert_eq!(get("test_m.T.test_high"), Some(TestOutcome::Passed));
        assert_eq!(get("test_m.T.test_low"), Some(TestOutcome::Failed));
        assert_eq!(get("test_m.T.test_doc"), Some(TestOutcome::Passed));
        assert_eq!(get("test_m.T.test_skip"), Some(TestOutcome::Skipped));
        assert_eq!(get("test_m.T.test_noisy"), Some(TestOutcome::Error));
        assert_eq!(get("test_m.T.test_old"), Some(TestOutcome::Passed));
        assert_eq!(r.len(), 6);
        assert_eq!(
            s,
            TestSummary {
                ran: 6,
                failures: 1,
                errors: 1,
                skipped: 1,
                ok: false
            }
        );
        let (_, s) = parse_unittest("Ran 2 tests in 0.0s\n\nOK (skipped=1)\n");
        assert!(s.ok);
        assert_eq!((s.ran, s.skipped), (2, 1));
    }

    /// Verbatim `python3 -I -m unittest discover -v` output (Python 3.13),
    /// including a test whose stderr lands between its docstring and its
    /// result.
    #[test]
    fn real_python_313_output() {
        let out = "\
test_low (test_m.T.test_low) ... ok
test_doc (test_more.U.test_doc)
Docstring line. ... noise
ok
test_err (test_more.U.test_err) ... ERROR
test_fail (test_more.U.test_fail) ... FAIL
test_skip (test_more.U.test_skip) ... skipped 'later'

======================================================================
ERROR: test_err (test_more.U.test_err)
----------------------------------------------------------------------
Traceback (most recent call last):
RuntimeError: boom

----------------------------------------------------------------------
Ran 5 tests in 0.002s

FAILED (failures=1, errors=1, skipped=1)
";
        let (r, s) = parse_unittest(out);
        let got: Vec<(&str, TestOutcome)> = r.iter().map(|t| (t.id.as_str(), t.outcome)).collect();
        assert_eq!(
            got,
            [
                ("test_m.T.test_low", TestOutcome::Passed),
                ("test_more.U.test_doc", TestOutcome::Passed),
                ("test_more.U.test_err", TestOutcome::Error),
                ("test_more.U.test_fail", TestOutcome::Failed),
                ("test_more.U.test_skip", TestOutcome::Skipped),
            ]
        );
        assert_eq!(
            (s.ran, s.failures, s.errors, s.skipped, s.ok),
            (5, 1, 1, 1, false)
        );
    }

    #[test]
    fn coverage_files() {
        assert_eq!(cover_name("pkg/m.py").as_deref(), Some("pkg.m.cover"));
        assert_eq!(cover_name("m.py").as_deref(), Some("m.cover"));
        assert_eq!(cover_name("README.md"), None);
        assert_eq!(cover_name("a.b/m.py"), None);
        let cover = "    1: def clamp(x, lo, hi):\n    1:     if x < lo:\n    1:         return lo\n>>>>>>     if x > hi:\n>>>>>>         return hi\n\n       # comment\n";
        assert_eq!(parse_cover(cover), vec![4, 5]);
    }
}
