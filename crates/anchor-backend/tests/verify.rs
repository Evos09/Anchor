//! Phase 5 done-when tests: the verification gate (spec §8). A change that
//! fails its build, its tests or its review does not merge; claims without
//! evidence are rejected automatically; tests are protected; the pipeline
//! refuses a reviewer of an executor's family. Real commands (Python's
//! unittest and trace) in the sandbox.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anchor_backend::interact::Scripted;
use anchor_backend::iso;
use anchor_backend::mediator::lock_store;
use anchor_backend::queue::{MergeQueue, Outcome, RejectKind};
use anchor_backend::team::{self, Schedule, Team, TeamAgent};
use anchor_backend::verify::{ReviewerKind, VerifyGate, VerifySpec};
use anchor_backend::{Opened, default_exec_read_only, open_workspace_in};
use anchor_hub::{Caller, ClaimType, NewAgent, Permissions};
use anchor_protocol::ResumeChoice;
use anchor_protocol::verify::Claim;
use serde_json::{Value, json};

static SERIAL: Mutex<()> = Mutex::new(());

const ROLES: &str = include_str!("../../../examples/demo/roles.mock.toml");

fn roles_a() -> String {
    ROLES.replace("\r\n", "\n")
}

/// Roles whose reviewer route runs on `provider` (`mock_a` is family A,
/// the executor's in `roles_a`; `mock_b` is family B), optionally falling
/// back to `fallback`.
fn reviewer_roles(provider: &str, fallback: Option<&str>) -> String {
    let fb = fallback.map_or(String::new(), |f| format!("fallback = [\"{f}\"]\n"));
    roles_a().replace(
        "[roles.reviewer]\nprovider = \"mock_b\"",
        &format!("[roles.reviewer]\nprovider = \"{provider}\"\n{fb}"),
    )
}

pub const CALC: &str = "def clamp(x, lo, hi):\n    return x\n";
pub const TEST_CALC: &str = "\
import unittest
from calc import clamp


class T(unittest.TestCase):
    def test_inside(self):
        self.assertEqual(clamp(5, 0, 10), 5)
";
pub const GOOD_CALC: &str = "\
def clamp(x, lo, hi):
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x
";
pub const TEST_CLAMP: &str = "\
import unittest
from calc import clamp


class C(unittest.TestCase):
    def test_low(self):
        self.assertEqual(clamp(-1, 0, 10), 0)

    def test_high(self):
        self.assertEqual(clamp(11, 0, 10), 10)
";

fn python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    ws: PathBuf,
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join("tests")).unwrap();
    std::fs::write(ws.join("calc.py"), CALC).unwrap();
    std::fs::write(ws.join("tests/__init__.py"), "").unwrap();
    std::fs::write(ws.join("tests/test_calc.py"), TEST_CALC).unwrap();
    Fx {
        _dir: dir,
        base,
        ws,
    }
}

impl Fx {
    fn state_base(&self) -> PathBuf {
        self.base.join("state")
    }
    fn open(&self) -> Opened {
        open_workspace_in(
            &self.ws,
            &self.state_base(),
            None,
            Box::new(Scripted::new(true, ResumeChoice::FreshSession)),
            vec![],
        )
        .unwrap()
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.ws.join(rel)).unwrap_or_else(|_| "<absent>".into())
    }
}

fn spec() -> VerifySpec {
    VerifySpec {
        build: Some(
            ["python3", "-I", "-m", "compileall", "-q", "."]
                .map(String::from)
                .to_vec(),
        ),
        test: ["python3", "-I", "-m", "unittest", "discover", "-v"]
            .map(String::from)
            .to_vec(),
        coverage: Some(
            [
                "python3",
                "-I",
                "-m",
                "trace",
                "--count",
                "--missing",
                "--coverdir",
                "{coverdir}",
                "--ignore-dir",
                "$prefix",
                "--ignore-dir",
                "$exec_prefix",
                "--module",
                "unittest",
                "discover",
            ]
            .map(String::from)
            .to_vec(),
        ),
        protected: vec!["tests/**".into()],
        max_attempts: 3,
        reviewer: ReviewerKind::Rule,
        reviewer_roles: None,
        reviewer_mock_script: None,
        timeout: std::time::Duration::from_secs(120),
    }
}

fn hub_of(opened: &Opened) -> Arc<anchor_hub::Hub> {
    let root = opened.mediator.workspace().root().to_path_buf();
    team::open_hub(&opened.paths, &root, opened.mediator.engine().audit_arc()).unwrap()
}

fn gate(fx: &Fx, opened: &Opened, spec: VerifySpec) -> VerifyGate {
    let hub = hub_of(opened);
    VerifyGate::new(
        spec,
        anchor_backend::verify::scratch_base(&fx.state_base(), &fx.ws),
        Some(hub),
        anchor_backend::verify::rule_reviewer(),
        std::env::var("PATH").unwrap_or_default(),
        default_exec_read_only(),
    )
    .unwrap()
}

/// What the test agent may change.
const GRANTED: &[&str] = &["calc.py", "tests/", "unittest.py"];

/// Submit, as agent "alpha", the workspace as
/// `edit` leaves it, with `claims`.
fn submit_change(
    q: &mut MergeQueue<'_>,
    fx: &Fx,
    edit: impl FnOnce(&std::path::Path),
    claims: &[Claim],
) -> Outcome {
    let (g, _) = q
        .hub()
        .register_agent(
            Caller::User,
            NewAgent {
                display_name: "alpha".into(),
                model_id: "m".into(),
                role: "executor".into(),
                family: "f".into(),
                capabilities: json!({}),
                permissions: Permissions::collaborator(GRANTED),
            },
        )
        .unwrap();
    for p in GRANTED {
        q.hub()
            .request_claim(Caller::Agent(g.id), p, ClaimType::Edit, false, None)
            .unwrap();
    }
    let main = q.main();
    let store_arc = main.shared_store();
    let i = {
        let store = lock_store(&store_arc);
        let base =
            anchor_snapshot::snapshot(&store, main.workspace(), main.snapshot_options(), "b")
                .unwrap()
                .id();
        iso::create(
            &store,
            main.snapshot_options(),
            main.engine(),
            &iso::iso_base(&fx.state_base(), main.workspace().root()),
            iso::root_for(g.id),
            &base,
        )
        .unwrap()
    };
    edit(i.ws.root());
    let sub = {
        let store = lock_store(&store_arc);
        iso::submit(&store, &i).unwrap()
    };
    let e = q.submit(g.id, &i.base, &sub, claims).unwrap();
    q.process(e.seq).unwrap()
}

fn clamp_claim() -> Claim {
    Claim {
        id: "c1".into(),
        text: "clamp() limits x to [lo, hi]".into(),
        files: vec!["calc.py".into()],
        tests: vec![
            "tests.test_clamp.C.test_low".into(),
            "tests.test_clamp.C.test_high".into(),
        ],
    }
}

fn write(root: &std::path::Path, rel: &str, text: &str) {
    std::fs::write(root.join(rel), text).unwrap();
}

/// The finding codes of a gate rejection.
fn codes(o: &Outcome) -> Vec<String> {
    let Outcome::Rejected {
        reason, feedback, ..
    } = o
    else {
        panic!("expected a rejection: {o:?}")
    };
    assert_eq!(*reason, RejectKind::Gate, "{feedback}");
    feedback["detail"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap().to_string())
        .collect()
}

macro_rules! need_python {
    () => {
        if !python() {
            eprintln!("python3 not found: skipped");
            return;
        }
    };
}

#[cfg(unix)]
#[test]
fn verified_change_merges_with_evidence() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
        },
        &[clamp_claim()],
    );
    assert!(o.is_merged(), "{o:?}");
    drop(q);
    assert_eq!(fx.read("calc.py"), GOOD_CALC);
    // Every command's output, the review input and the decision are
    // durable, hashed Hub evidence.
    let arts = hub
        .list_artifacts(Caller::User, Some("evidence:verify-"))
        .unwrap();
    let names: Vec<&str> = arts
        .iter()
        .map(|a| a.path.rsplit('/').next().unwrap())
        .collect();
    for n in [
        "build.log",
        "test.log",
        "coverage.log",
        "review-input.json",
        "decision.json",
    ] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    let decision = arts
        .iter()
        .find(|a| a.path.ends_with("decision.json"))
        .unwrap();
    let d: Value =
        serde_json::from_slice(&hub.read_evidence(Caller::User, decision).unwrap()).unwrap();
    // The tests ran in the sandbox.
    let log = arts.iter().find(|a| a.path.ends_with("test.log")).unwrap();
    let log = String::from_utf8(hub.read_evidence(Caller::User, log).unwrap()).unwrap();
    assert!(log.contains("fs_confined true"), "{log}");
    assert!(!log.contains("network None"), "{log}");
    assert_eq!(d["approved"], true);
    assert_eq!(d["verdicts"][0]["verdict"], "supported");
    assert!(hub.verify().unwrap().ok());
}

#[cfg(unix)]
#[test]
fn failing_tests_do_not_merge() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let buggy = GOOD_CALC.replace("return hi", "return lo");
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", &buggy);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
        },
        &[clamp_claim()],
    );
    assert!(codes(&o).contains(&"test_failed".to_string()), "{o:?}");
    let Outcome::Rejected { feedback, .. } = &o else {
        unreachable!()
    };
    // Structured feedback for the executor (§8.2(c)).
    assert_eq!(
        feedback["detail"]["failing_tests"],
        json!(["tests.test_clamp.C.test_high"])
    );
    assert!(
        feedback["detail"]["hunks"]
            .as_str()
            .unwrap()
            .contains("+++ b/calc.py")
    );
    drop(q);
    assert_eq!(fx.read("calc.py"), CALC);
}

#[cfg(unix)]
#[test]
fn failing_build_does_not_merge() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| write(r, "calc.py", "def clamp(x, lo, hi:\n    return x\n"),
        &[clamp_claim()],
    );
    assert!(codes(&o).contains(&"build_failed".to_string()), "{o:?}");
    drop(q);
    assert_eq!(fx.read("calc.py"), CALC);
}

#[cfg(unix)]
#[test]
fn claim_without_evidence_rejected_automatically() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let mut claim = clamp_claim();
    claim.tests = vec!["tests.test_clamp.C.test_missing".into()];
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
        },
        &[claim],
    );
    assert_eq!(codes(&o), ["no_test_for_claim"]);
}

#[cfg(unix)]
#[test]
fn uncovered_change_is_not_supported() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    // Only the low bound is tested; the high-bound branch never runs.
    let tests = TEST_CLAMP.replace(
        "\n    def test_high(self):\n        self.assertEqual(clamp(11, 0, 10), 10)\n",
        "",
    );
    let mut claim = clamp_claim();
    claim.tests.pop();
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", &tests);
        },
        &[claim],
    );
    assert_eq!(codes(&o), ["uncovered_change"]);
}

#[cfg(unix)]
#[test]
fn tampered_tests_rejected() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
            // Weaken an existing test.
            write(
                r,
                "tests/test_calc.py",
                &TEST_CALC.replace("clamp(5, 0, 10), 5", "5, 5"),
            );
        },
        &[clamp_claim()],
    );
    assert!(codes(&o).contains(&"tests_tampered".to_string()), "{o:?}");
}

#[cfg(unix)]
#[test]
fn regressed_test_rejected() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    // Without protected paths, deleting a passing test is still caught.
    let mut s = spec();
    s.protected.clear();
    let g = gate(&fx, &opened, s);
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
            std::fs::remove_file(r.join("tests/test_calc.py")).unwrap();
        },
        &[clamp_claim()],
    );
    assert_eq!(codes(&o), ["test_regressed"]);
}

#[cfg(unix)]
#[test]
fn spoofed_exit_rejected() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    // Exit 0 at import time, before any test runs.
    let calc = format!("import os\nos._exit(0)\n{GOOD_CALC}");
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", &calc);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
        },
        &[clamp_claim()],
    );
    let c = codes(&o);
    assert!(c.contains(&"tests_inconclusive".to_string()), "{c:?}");
}

#[cfg(unix)]
#[test]
fn stdlib_shadowing_rejected() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
            write(r, "unittest.py", "raise SystemExit(0)\n");
        },
        &[clamp_claim()],
    );
    assert!(codes(&o).contains(&"shadows_stdlib".to_string()), "{o:?}");
}

#[cfg(unix)]
#[test]
fn verification_commands_cannot_write_outside_scratch() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let target = fx.ws.join("escaped.txt");
    let mut s = spec();
    // The build step tries to write into the main workspace.
    s.build = Some(vec![
        "python3".into(),
        "-I".into(),
        "-c".into(),
        format!("open({:?}, 'w').write('x')", target.display().to_string()),
    ]);
    let g = gate(&fx, &opened, s);
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
        },
        &[clamp_claim()],
    );
    assert!(codes(&o).contains(&"build_failed".to_string()), "{o:?}");
    assert!(!target.exists());
}

fn team_with(reviewer: String) -> Team {
    let mut s = spec();
    s.reviewer = ReviewerKind::Model;
    s.reviewer_roles = Some(reviewer);
    Team {
        schedule: Schedule::Sequential,
        verify: Some(s),
        agents: vec![TeamAgent {
            name: "alpha".into(),
            roles_toml: roles_a(),
            instruction: "x".into(),
            claim_within: vec!["calc.py".into()],
            mock_script: None,
            messages_need_approval: false,
        }],
    }
}

#[test]
fn pipeline_refuses_same_family() {
    // A reviewer of another family is accepted...
    team_with(reviewer_roles("mock_b", None))
        .validate()
        .unwrap();
    // ...one of the executor's family is not...
    let e = team_with(reviewer_roles("mock_a", None))
        .validate()
        .unwrap_err();
    assert!(e.contains("family"), "{e}");
    // ...nor one that could fall back into it.
    let e = team_with(reviewer_roles("mock_b", Some("mock_a")))
        .validate()
        .unwrap_err();
    assert!(e.contains("family"), "{e}");
}

#[cfg(windows)]
#[test]
fn verify_gate_fails_closed_on_windows() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let o = submit_change(
        &mut q,
        &fx,
        |r| write(r, "calc.py", GOOD_CALC),
        &[clamp_claim()],
    );
    assert_eq!(codes(&o), ["unsupported_platform"]);
    drop(q);
    assert_eq!(fx.read("calc.py"), CALC);
}
