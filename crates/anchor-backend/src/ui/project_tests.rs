//! The project's own tests, run by the app (0.12.0, ADR-0040): a test
//! command the user confirmed (Anchor suggests one from the project's
//! files but never runs a suggestion by itself), run sandboxed with no
//! network on a scratch **export** of a snapshot, never in the live
//! project, so a test run cannot change the user's files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anchor_protocol::verify::{CommandRecord, TestOutcome, TestRecord};
use anchor_snapshot::Id;
use serde::{Deserialize, Serialize};

use crate::mediator::Mediator;
use crate::verify::{self, CommandEnv};

/// A test run's default time limit.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// The longest time limit the user can set.
pub const MAX_TIMEOUT_SECS: u64 = 1800;
/// Most test output quoted back to the builder.
pub const MAX_FEEDBACK_OUTPUT: usize = 4096;

/// A project's test command, as the user confirmed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSetup {
    pub argv: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl TestSetup {
    pub fn validate(&self) -> Result<(), String> {
        if self.argv.is_empty() || self.argv.len() > 64 {
            return Err("the test command needs a program and at most 63 arguments".into());
        }
        if self
            .argv
            .iter()
            .any(|a| a.is_empty() || a.len() > 1000 || a.contains('\0'))
        {
            return Err("each part of the test command must be 1-1000 characters".into());
        }
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return Err(format!(
                "the time limit must be 1-{MAX_TIMEOUT_SECS} seconds"
            ));
        }
        Ok(())
    }
}

/// What Anchor suggests for a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detected {
    /// The suggested command, if Anchor knows one.
    pub argv: Option<Vec<String>>,
    /// Why (or why not), in a few words for the page.
    pub why: String,
}

fn has_python_tests(root: &Path) -> bool {
    let named = |d: &Path| {
        std::fs::read_dir(d).is_ok_and(|rd| {
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_lowercase();
                n.ends_with(".py") && (n.starts_with("test") || n.ends_with("_test.py"))
            })
        })
    };
    named(root) || named(&root.join("tests")) || named(&root.join("test"))
}

/// The project's likely test command, from its files. `path_var` is the
/// `PATH` commands run with (to see whether pytest is installed).
pub fn detect(root: &Path, path_var: &str) -> Detected {
    let is = |f: &str| root.join(f).is_file();
    let found = |argv: &[&str], why: &str| Detected {
        argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        why: why.into(),
    };
    if is("Cargo.toml") {
        return found(&["cargo", "test"], "a Rust project (Cargo.toml)");
    }
    if is("package.json") {
        let has_test = std::fs::read(root.join("package.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.pointer("/scripts/test")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .is_some_and(|t| !t.contains("no test specified"));
        if has_test {
            return found(&["npm", "test"], "a Node project with a test script");
        }
    }
    if is("go.mod") {
        return found(&["go", "test", "./..."], "a Go module (go.mod)");
    }
    if is("pyproject.toml") || is("pytest.ini") || is("setup.py") || has_python_tests(root) {
        let python = if cfg!(windows) { "python" } else { "python3" };
        let pytest = anchor_exec::resolve_program("pytest", path_var).is_ok();
        return if pytest {
            found(&[python, "-m", "pytest", "-q"], "Python tests (pytest)")
        } else {
            found(
                &[python, "-m", "unittest", "discover", "-v"],
                "Python tests (unittest)",
            )
        };
    }
    if is("pom.xml") || is("build.gradle") || is("build.gradle.kts") {
        return Detected {
            argv: None,
            why: "Java builds are not supported yet; type the command yourself".into(),
        };
    }
    Detected {
        argv: None,
        why: "no tests found; type the command yourself if the project has some".into(),
    }
}

/// One test run's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRun {
    pub ok: bool,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub passed: usize,
    pub failed: usize,
    /// "12 passed", "2 of 12 failed", "exit code 1"...
    pub summary: String,
    pub output_tail: String,
    #[serde(skip)]
    pub record: CommandRecord,
    #[serde(skip)]
    pub tests: Vec<TestRecord>,
}

impl TestRun {
    fn failing(&self) -> Vec<&str> {
        self.tests
            .iter()
            .filter(|t| matches!(t.outcome, TestOutcome::Failed | TestOutcome::Error))
            .map(|t| t.id.as_str())
            .collect()
    }
}

fn summarize(ran: verify::Ran) -> TestRun {
    let (tests, _) = anchor_verify::parse::parse_unittest(&ran.stderr);
    let passed = tests
        .iter()
        .filter(|t| t.outcome == TestOutcome::Passed)
        .count();
    let failed = tests
        .iter()
        .filter(|t| matches!(t.outcome, TestOutcome::Failed | TestOutcome::Error))
        .count();
    let r = &ran.record;
    let summary = if r.timed_out {
        "timed out".to_string()
    } else if !tests.is_empty() && failed > 0 {
        format!("{failed} of {} failed", passed + failed)
    } else if !tests.is_empty() {
        format!("{passed} passed")
    } else {
        match r.exit {
            Some(0) => "passed".into(),
            Some(c) => format!("failed (exit code {c})"),
            None => "failed (stopped)".into(),
        }
    };
    TestRun {
        ok: r.ok,
        exit: r.exit,
        timed_out: r.timed_out,
        passed,
        failed,
        summary,
        output_tail: ran.record.output_tail.clone(),
        record: ran.record,
        tests,
    }
}

/// Do the tests after a change hold, given the run before it? They hold
/// when they pass, or when they already failed at the start and fail no
/// worse now (no test fails that did not fail before).
pub fn holds(before: Option<&TestRun>, now: &TestRun) -> bool {
    if now.ok {
        return true;
    }
    let Some(b) = before.filter(|b| !b.ok) else {
        return false;
    };
    if now.timed_out && !b.timed_out {
        return false;
    }
    if !now.tests.is_empty() && !b.tests.is_empty() {
        let was = b.failing();
        now.failing().iter().all(|t| was.contains(t))
    } else {
        now.exit == b.exit
    }
}

/// Run `setup` on snapshot `snap`, exported to a fresh directory under
/// `scratch`, which is removed afterwards.
pub fn run_on(
    m: &Mediator,
    snap: &Id,
    setup: &TestSetup,
    scratch: &Path,
) -> Result<TestRun, String> {
    setup.validate()?;
    verify::prepare_scratch(scratch)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = scratch.join(format!("tests-{nanos}-{}", std::process::id()));
    verify::prepare_scratch(&dir)?;
    let out = run_in(m, snap, setup, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn run_in(m: &Mediator, snap: &Id, setup: &TestSetup, dir: &Path) -> Result<TestRun, String> {
    let ws = dir.join("ws");
    let home = dir.join("home");
    for d in [&ws, &home] {
        std::fs::create_dir(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    verify::export_shared(&m.shared_store(), snap, &ws)?;
    verify::hand_over(&[&ws])?;
    let mut read_only: Vec<PathBuf> = crate::default_exec_read_only();
    for d in m.exec_read_only() {
        if !read_only.contains(d) {
            read_only.push(d.clone());
        }
    }
    let ran = verify::run_command(
        "test",
        &setup.argv,
        &ws,
        std::slice::from_ref(&ws),
        &CommandEnv {
            path_var: m.path_var(),
            read_only: &read_only,
            home: Some(&home),
            timeout: Duration::from_secs(setup.timeout_secs),
        },
    );
    Ok(summarize(ran))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, f: &str, s: &str) {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn test_commands_are_suggested_from_the_project() {
        let argv = |root: &Path| detect(root, "").argv.map(|a| a.join(" "));
        let py = if cfg!(windows) { "python" } else { "python3" };
        let d = tempfile::tempdir().unwrap();
        assert_eq!(argv(d.path()), None);
        write(d.path(), "tests/test_x.py", "");
        assert_eq!(
            argv(d.path()),
            Some(format!("{py} -m unittest discover -v"))
        );
        write(
            d.path(),
            "package.json",
            r#"{"scripts":{"test":"vitest run"}}"#,
        );
        assert_eq!(argv(d.path()).as_deref(), Some("npm test"));
        write(d.path(), "go.mod", "module x");
        assert_eq!(argv(d.path()).as_deref(), Some("npm test"));
        write(d.path(), "Cargo.toml", "[package]");
        assert_eq!(argv(d.path()).as_deref(), Some("cargo test"));

        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "package.json",
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        assert_eq!(argv(d.path()), None, "npm's placeholder is not a test");
        write(d.path(), "go.mod", "module x");
        assert_eq!(argv(d.path()).as_deref(), Some("go test ./..."));

        let d = tempfile::tempdir().unwrap();
        write(d.path(), "pom.xml", "<project/>");
        let det = detect(d.path(), "");
        assert_eq!(det.argv, None);
        assert!(det.why.contains("not supported"));
    }

    #[test]
    fn setups_are_checked() {
        let ok = TestSetup {
            argv: vec!["npm".into(), "test".into()],
            timeout_secs: 60,
        };
        assert_eq!(ok.validate(), Ok(()));
        for bad in [
            TestSetup {
                argv: vec![],
                ..ok.clone()
            },
            TestSetup {
                argv: vec!["".into()],
                ..ok.clone()
            },
            TestSetup {
                timeout_secs: 0,
                ..ok.clone()
            },
            TestSetup {
                timeout_secs: MAX_TIMEOUT_SECS + 1,
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    fn run(ok: bool, exit: Option<i32>, failing: &[&str]) -> TestRun {
        TestRun {
            ok,
            exit,
            timed_out: false,
            passed: 0,
            failed: failing.len(),
            summary: String::new(),
            output_tail: String::new(),
            record: CommandRecord {
                name: "test".into(),
                argv: vec![],
                ok,
                exit,
                timed_out: false,
                elapsed_ms: 0,
                user_cpu_ms: None,
                max_rss_bytes: None,
                output_tail: String::new(),
                artifact: String::new(),
            },
            tests: failing
                .iter()
                .map(|t| TestRecord {
                    id: t.to_string(),
                    outcome: TestOutcome::Failed,
                })
                .collect(),
        }
    }

    #[test]
    fn tests_hold_when_they_pass_or_fail_no_worse_than_before() {
        let pass = run(true, Some(0), &[]);
        let a = run(false, Some(1), &["t.a"]);
        let ab = run(false, Some(1), &["t.a", "t.b"]);
        assert!(holds(None, &pass));
        assert!(holds(Some(&a), &pass));
        assert!(!holds(None, &a));
        assert!(!holds(Some(&pass), &a), "newly failing");
        assert!(holds(Some(&ab), &a), "fails no worse");
        assert!(holds(Some(&a), &a));
        assert!(!holds(Some(&a), &ab), "a new failure");
        // Unparsed output: the same exit code holds.
        assert!(holds(
            Some(&run(false, Some(2), &[])),
            &run(false, Some(2), &[])
        ));
        assert!(!holds(
            Some(&run(false, Some(2), &[])),
            &run(false, Some(1), &[])
        ));
    }
}
