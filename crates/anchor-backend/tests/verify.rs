//! Phase 5 done-when tests: the verification gate (spec §8). A change that
//! fails its build, its tests or its review does not merge; claims without
//! evidence are rejected automatically; tests are protected; the pipeline
//! refuses a reviewer of an executor's family. Real commands (Python's
//! unittest and trace) in the sandbox.

#![allow(clippy::unwrap_used)]
// The gate tests run on every platform (Windows: AppContainer, ADR-0026).
// The team tests drive the agent runtime, which is confined only on Unix
// (AR-7), so on Windows their helpers go unused.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

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

/// The interpreter: Windows installs name it `python`.
const PY: &str = if cfg!(windows) { "python" } else { "python3" };

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
    std::process::Command::new(PY)
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
            [PY, "-I", "-m", "compileall", "-q", "."]
                .map(String::from)
                .to_vec(),
        ),
        test: [PY, "-I", "-m", "unittest", "discover", "-v"]
            .map(String::from)
            .to_vec(),
        coverage: Some(
            [
                PY,
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
            eprintln!("{PY} not found: skipped");
            return;
        }
    };
}

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

/// A change of tests only, whose new test fails: the defect is in code
/// the main workspace already has (a reviewer missed it; a test caught it
/// later, spec §12.3). Rejected, with the blame placed.
#[test]
fn test_only_change_failing_is_flagged_as_caught_later() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    // calc.py's clamp() (the stub) ignores its bounds.
    let claim = Claim {
        id: "c1".into(),
        text: "clamp() is tested below its range".into(),
        files: vec!["tests/test_clamp.py".into()],
        tests: vec!["tests.test_clamp.C.test_low".into()],
    };
    let o = submit_change(
        &mut q,
        &fx,
        |r| write(r, "tests/test_clamp.py", TEST_CLAMP),
        &[claim],
    );
    let c = codes(&o);
    assert!(c.contains(&"fails_on_main".to_string()), "{o:?}");
    assert!(!o.is_merged());
}

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

/// Review finding V-1: a test module that patches the runner so failures
/// are not counted is rejected before its verdict is believed.
#[test]
fn runner_tampering_rejected() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let tests = format!("{TEST_CLAMP}\nunittest.TestResult.addFailure = lambda *a, **k: None\n");
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            // Still buggy: the patched runner would report it passing.
            write(r, "tests/test_clamp.py", &tests);
        },
        &[clamp_claim()],
    );
    assert!(codes(&o).contains(&"runner_tampering".to_string()), "{o:?}");
}

/// Review finding V-2: files the interpreter or runner loads on its own
/// are rejected, and commands never see the candidate as `HOME` or a
/// user site directory.
#[test]
fn startup_hooks_rejected_and_home_is_empty() {
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
            write(r, "tests/evil.pth", "import calc\n");
            write(r, "tests/conftest.py", "\n");
        },
        &[clamp_claim()],
    );
    let c = codes(&o);
    assert_eq!(c, ["startup_hook"], "{o:?}");
    drop(q);
    drop(opened);

    let fx = fixture();
    let mut opened = fx.open();

    let mut s = spec();
    s.build = Some(vec![
        PY.into(),
        "-c".into(),
        "import os, sys\n\
         h = os.environ['HOME']\n\
         ok = os.listdir(h) == [] and os.path.realpath(h) != os.getcwd() \\\n\
             and os.environ.get('PYTHONNOUSERSITE') == '1' \\\n\
             and 'PYTHONPATH' not in os.environ\n\
         try:\n    open(os.path.join(h, 'x'), 'w')\n    ok = False\n\
         except OSError:\n    pass\n\
         sys.exit(0 if ok else 1)"
            .into(),
    ]);
    let hub = hub_of(&opened);
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
    assert!(o.is_merged(), "{o:?}");
}

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
        PY.into(),
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
            repair_mock_scripts: Vec::new(),
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

// ---- Team runs: claims from real runtime sessions, self-repair, model
// reviewers. ----

fn agent_bin() -> PathBuf {
    let p = anchor_backend::session::default_agent_bin();
    if !p.exists() {
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "-q",
                "-p",
                "anchor-runtime",
                "--bin",
                "anchor-agent",
            ])
            .status()
            .expect("cargo build anchor-agent");
        assert!(status.success());
    }
    p
}

fn step(calls: Value) -> Value {
    json!({ "content": "", "tool_calls": calls, "prompt_tokens": 100, "completion_tokens": 10 })
}

fn call(tool: &str, args: Value) -> Value {
    json!({ "tool": tool, "args": args })
}

/// An executor that writes `calc` (and the clamp tests) and claims it.
fn executor_script(calc: &str) -> String {
    json!({ "steps": [
        step(json!([
            call("hub_request_claim", json!({ "path": "calc.py", "claim_type": "edit" })),
            call("hub_request_claim", json!({ "path": "tests/", "claim_type": "edit" })),
        ])),
        step(json!([
            call("write_file", json!({ "path": "calc.py", "content": calc })),
            call("write_file", json!({ "path": "tests/test_clamp.py", "content": TEST_CLAMP })),
        ])),
        step(json!([ call("submit_claims", json!({ "claims": [clamp_claim()] })) ])),
        { "content": "clamp implemented", "prompt_tokens": 100, "completion_tokens": 10 },
    ]})
    .to_string()
}

/// A reviewer that gives every claim (here: `c1`) `verdict`.
fn reviewer_script(verdict: &str) -> String {
    json!({ "steps": [
        step(json!([ call("submit_verdicts", json!({ "verdicts": [{
            "claim_id": "c1", "verdict": verdict, "reason_code": "checked",
            "evidence_refs": ["diff:calc.py", "test:tests.test_clamp.C.test_high"],
        }]})) ])),
        { "content": "reviewed", "prompt_tokens": 100, "completion_tokens": 10 },
    ]})
    .to_string()
}

fn verify_team(first: &str, repairs: &[&str], spec: VerifySpec) -> Team {
    Team {
        schedule: Schedule::Sequential,
        verify: Some(spec),
        agents: vec![TeamAgent {
            name: "alpha".into(),
            roles_toml: roles_a(),
            instruction: "Make clamp() limit x to [lo, hi], with tests.".into(),
            claim_within: vec!["calc.py".into(), "tests/".into()],
            mock_script: Some(executor_script(first)),
            repair_mock_scripts: repairs.iter().map(|c| executor_script(c)).collect(),
            messages_need_approval: false,
        }],
    }
}

fn run_verified(
    fx: &Fx,
    opened: &mut Opened,
    team: &Team,
    interact: Box<dyn anchor_backend::interact::Interaction>,
    auto_merge: bool,
) -> team::TeamReport {
    let opts = team::TeamOptions {
        agent_bin: agent_bin(),
        state_base: fx.state_base(),
        auto_merge,
        checkpoint_threshold: 0.8,
        gate: Box::new(anchor_backend::queue::NoGate),
    };
    team::run(opened, team, opts, team::Shared::wrap(interact)).unwrap()
}

fn buggy() -> String {
    GOOD_CALC.replace("return hi", "return lo")
}

#[cfg(unix)]
#[test]
fn self_repair_merges_on_second_attempt() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let team = verify_team(&buggy(), &[GOOD_CALC], spec());
    let r = run_verified(
        &fx,
        &mut opened,
        &team,
        Box::new(Scripted::new(true, ResumeChoice::FreshSession)),
        true,
    );
    let a = &r.agents[0];
    assert_eq!(a.attempts.len(), 2, "{:?}", a.attempts);
    assert_eq!(a.attempts[0].reject_kind(), Some(RejectKind::Gate));
    assert!(a.attempts[1].is_merged());
    assert!(a.merge.as_ref().unwrap().is_merged());
    assert_eq!(fx.read("calc.py"), GOOD_CALC);
    // The repair session was told what failed.
    let audit = anchor_policy::AuditLog::read_entries(&opened.paths.audit).unwrap();
    assert_eq!(
        audit.iter().filter(|e| e.kind == "repair_started").count(),
        1
    );
    let store =
        anchor_checkpoint::CheckpointStore::open(&opened.paths.checkpoints.join("alpha")).unwrap();
    let (cps, _) = store.list().unwrap();
    let seen_feedback = cps.iter().any(|c| {
        store
            .load(&c.checkpoint_id)
            .unwrap()
            .messages
            .iter()
            .any(|m| {
                m.content.contains("REJECTED by verification")
                    && m.content.contains("tests.test_clamp.C.test_high")
            })
    });
    assert!(seen_feedback, "the repair instruction carries the feedback");
}

#[cfg(unix)]
#[test]
fn retries_are_bounded() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let b = buggy();
    let team = verify_team(&b, &[&b], spec());
    let r = run_verified(
        &fx,
        &mut opened,
        &team,
        Box::new(Scripted::new(true, ResumeChoice::FreshSession)),
        true,
    );
    let a = &r.agents[0];
    assert_eq!(a.attempts.len(), 3, "max_attempts = 3");
    assert!(
        a.attempts
            .iter()
            .all(|o| o.reject_kind() == Some(RejectKind::Gate))
    );
    assert_eq!(fx.read("calc.py"), CALC);
}

/// Approves everything except merges.
struct DeclinesMerges;

impl anchor_backend::interact::Interaction for DeclinesMerges {
    fn confirm(&mut self, v: &anchor_backend::interact::PromptView) -> anchor_policy::UserResponse {
        if v.rule.contains("merge_queue") {
            anchor_policy::UserResponse::Deny
        } else {
            anchor_policy::UserResponse::Approve
        }
    }
    fn resume_choice(&mut self, _: &str, _: &str) -> ResumeChoice {
        ResumeChoice::FreshSession
    }
    fn notify(&mut self, _: &str) {}
    fn responder(&self) -> anchor_policy::Responder {
        anchor_policy::Responder::Automatic
    }
}

#[cfg(unix)]
#[test]
fn user_decline_is_not_retried() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open_workspace_in(
        &fx.ws,
        &fx.state_base(),
        None,
        Box::new(DeclinesMerges),
        vec![],
    )
    .unwrap();
    let team = verify_team(GOOD_CALC, &[], spec());
    let r = run_verified(&fx, &mut opened, &team, Box::new(DeclinesMerges), false);
    let a = &r.agents[0];
    assert_eq!(a.attempts.len(), 1);
    assert_eq!(a.attempts[0].reject_kind(), Some(RejectKind::Declined));
    assert_eq!(fx.read("calc.py"), CALC);
}

fn model_spec(reviewer: &str, verdict: &str) -> VerifySpec {
    let mut s = spec();
    s.reviewer = ReviewerKind::Model;
    s.reviewer_roles = Some(reviewer_roles(reviewer, None));
    s.reviewer_mock_script = Some(reviewer_script(verdict));
    s.max_attempts = 1;
    s
}

#[cfg(unix)]
#[test]
fn model_reviewer_approves_supported_claims() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let team = verify_team(GOOD_CALC, &[], model_spec("mock_b", "supported"));
    let r = run_verified(
        &fx,
        &mut opened,
        &team,
        Box::new(Scripted::new(true, ResumeChoice::FreshSession)),
        true,
    );
    assert!(
        r.agents[0].merge.as_ref().unwrap().is_merged(),
        "{:?}",
        r.agents[0].attempts
    );
    assert_eq!(fx.read("calc.py"), GOOD_CALC);
}

#[cfg(unix)]
#[test]
fn refuted_claim_does_not_merge() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let team = verify_team(GOOD_CALC, &[], model_spec("mock_b", "refuted"));
    let r = run_verified(
        &fx,
        &mut opened,
        &team,
        Box::new(Scripted::new(true, ResumeChoice::FreshSession)),
        true,
    );
    let o = r.agents[0].merge.as_ref().unwrap();
    assert_eq!(codes(o), ["checked"]);
    assert_eq!(fx.read("calc.py"), CALC);
}

/// A reviewer that could fall back into the executor's family is refused
/// before any agent runs (§8.2(a)). (Verdicts from a session that did fall
/// back into it are refused by the gate: `verify::tests`.)
#[cfg(unix)]
#[test]
fn reviewer_falling_back_into_executor_family_is_refused() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let mut s = model_spec("mock_b", "supported");
    s.reviewer_roles = Some(reviewer_roles("mock_b", Some("mock_a")));
    let team = verify_team(GOOD_CALC, &[], s);
    let opts = team::TeamOptions {
        agent_bin: agent_bin(),
        state_base: fx.state_base(),
        auto_merge: true,
        checkpoint_threshold: 0.8,
        gate: Box::new(anchor_backend::queue::NoGate),
    };
    let interact = team::Shared::wrap(Box::new(Scripted::new(true, ResumeChoice::FreshSession)));
    let e = team::run(&mut opened, &team, opts, interact).unwrap_err();
    assert!(e.contains("family"), "{e}");
    assert_eq!(fx.read("calc.py"), CALC);
}

/// A child `anchor team run` with [verify] dies part-way through applying
/// a verified merge. Reopening undoes the half-applied merge; the queue
/// re-queues it and verifies it again (with the claims persisted at
/// submit) before applying it.
#[cfg(unix)]
#[test]
fn crash_after_gate_reverifies_on_recover() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let dir = fx.base.join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("roles.toml"), roles_a()).unwrap();
    std::fs::write(dir.join("alpha.json"), executor_script(GOOD_CALC)).unwrap();
    std::fs::write(
        dir.join("team.toml"),
        format!(
            "[[agent]]\nname = \"alpha\"\nroles = \"roles.toml\"\ninstruction = \"clamp\"\n\
             claim_within = [\"calc.py\", \"tests/\"]\nmock_script = \"alpha.json\"\n\n{}",
            include_str!("../../../examples/eval/corpus/corpus.toml")
        ),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_anchor"))
        .args(["team", "run", "--workspace"])
        .arg(&fx.ws)
        .arg("--team")
        .arg(dir.join("team.toml"))
        .args(["--auto-approve", "--auto-merge", "--agent"])
        .arg(agent_bin())
        .env("ANCHOR_STATE_DIR", fx.state_base())
        .env("ANCHOR_TEST_FAULT", "merge:1")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "the child must die mid-merge: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let mut opened = fx.open();
    assert!(
        opened.recovered.is_some(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fx.read("calc.py"), CALC);
    let hub = hub_of(&opened);
    let g = gate(&fx, &opened, spec());
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(g), true);
    let queued = q.recover().unwrap();
    assert_eq!(queued.len(), 1);
    let o = q.process(queued[0]).unwrap();
    assert!(o.is_merged(), "{o:?}");
    drop(q);
    assert_eq!(fx.read("calc.py"), GOOD_CALC);
    let audit = anchor_policy::AuditLog::read_entries(&opened.paths.audit).unwrap();
    // Verified once before the crash and again before the apply that held.
    assert_eq!(
        audit.iter().filter(|e| e.kind == "merge_verified").count(),
        2
    );
    assert!(hub.verify().unwrap().ok());
    anchor_policy::AuditLog::verify(&opened.paths.audit).unwrap();
}

/// A verification that takes a while never holds the snapshot store: other
/// agents keep snapshotting and writing meanwhile.
#[cfg(unix)]
#[test]
fn slow_gate_does_not_block_other_agents() {
    need_python!();
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let mut s = spec();
    // A test command that takes ~1.5 s.
    s.build = Some(vec![
        PY.into(),
        "-I".into(),
        "-c".into(),
        "import time; time.sleep(1.5)".into(),
    ]);
    let g = gate(&fx, &opened, s);
    let store = opened.mediator.shared_store();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = {
        let (store, stop) = (store.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut worst = std::time::Duration::ZERO;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let t = std::time::Instant::now();
                drop(lock_store(&store));
                worst = worst.max(t.elapsed());
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            worst
        })
    };
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(g), true);
    let t = std::time::Instant::now();
    let o = submit_change(
        &mut q,
        &fx,
        |r| {
            write(r, "calc.py", GOOD_CALC);
            write(r, "tests/test_clamp.py", TEST_CLAMP);
        },
        &[clamp_claim()],
    );
    let took = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let worst = probe.join().unwrap();
    assert!(o.is_merged(), "{o:?}");
    assert!(took >= std::time::Duration::from_millis(1500));
    assert!(
        worst < std::time::Duration::from_millis(1000),
        "the store was held for {worst:?} during a {took:?} merge"
    );
}

/// A team without [verify] needs an explicit --unverified.
#[test]
fn unverified_team_needs_explicit_flag() {
    let fx = fixture();
    let dir = fx.base.join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("roles.toml"), roles_a()).unwrap();
    std::fs::write(
        dir.join("team.toml"),
        "[[agent]]\nname = \"alpha\"\nroles = \"roles.toml\"\ninstruction = \"x\"\n\
         claim_within = [\"calc.py\"]\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_anchor"))
        .args(["team", "run", "--workspace"])
        .arg(&fx.ws)
        .arg("--team")
        .arg(dir.join("team.toml"))
        .arg("--auto-approve")
        .env("ANCHOR_STATE_DIR", fx.state_base())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--unverified"), "{err}");
    assert_eq!(fx.read("calc.py"), CALC);
}
