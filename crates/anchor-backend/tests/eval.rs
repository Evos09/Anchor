//! Phase 5 done-when: the reviewer's catch rate is measured on a
//! seeded-bug harness (spec §8.5, §16.3). These tests check the harness
//! itself with calibration reviewers (accept-all: every bug that reaches
//! review merges; oracle: none does) and lock the rule baseline's exact
//! confusion matrix on a subset of the corpus, as a regression guard.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anchor_backend::eval::{self, Calibration, Corpus, Matrix};
use anchor_backend::verify::{BoxedReviewer, VerifyGate};

/// Tasks measured here (the whole corpus runs in CI via `anchor eval`).
const SUBSET: &[&str] = &["clamp", "fizzbuzz", "leap_year", "roman"];

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/eval/corpus")
}

fn python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn subset() -> Corpus {
    let mut c = Corpus::load(&corpus_dir()).unwrap();
    c.retain(&SUBSET.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .unwrap();
    c
}

fn measure(c: &Corpus, reviewer: BoxedReviewer, oracle: Option<&AtomicBool>) -> eval::Report {
    let state = tempfile::tempdir().unwrap();
    let mut gate = VerifyGate::new(
        c.spec.clone(),
        anchor_backend::verify::scratch_base(state.path(), &c.dir),
        None,
        reviewer,
        std::env::var("PATH").unwrap_or_default(),
        anchor_backend::default_exec_read_only(),
    )
    .unwrap();
    eval::run(c, &mut gate, oracle, None).unwrap()
}

#[test]
fn corpus_ground_truth_holds() {
    if !python() {
        return;
    }
    let c = Corpus::load(&corpus_dir()).unwrap();
    assert_eq!(c.tasks.len(), 12);
    assert_eq!(c.tasks.iter().map(|t| t.bugs.len()).sum::<usize>(), 48);
    assert_eq!(eval::self_check(&c).unwrap(), Vec::<String>::new());
}

#[test]
fn seeded_bug_harness_measures_reviewers() {
    if !python() {
        eprintln!("python3 not found: skipped");
        return;
    }
    let c = subset();

    // Accept-all: every bug the deterministic stages pass on merges.
    let r = measure(&c, Box::new(Calibration::AcceptAll), None);
    let reached = r.reviewer_stage.tp + r.reviewer_stage.fn_;
    assert!(reached > 0);
    assert_eq!(r.reviewer_stage.fn_rate(), Some(1.0));
    assert_eq!(r.gate.fn_, reached);
    assert_eq!(r.completion_rate, Some(1.0));

    // Oracle: nothing buggy merges, nothing correct is refused.
    let flag = Arc::new(AtomicBool::new(false));
    let r = measure(&c, Box::new(Calibration::Oracle(flag.clone())), Some(&flag));
    assert_eq!(r.gate.fn_rate(), Some(0.0));
    assert_eq!(r.gate.fp, 0);

    // The rule baseline, exactly (a regression guard, not a threshold:
    // semantic and vacuous-test bugs pass it by construction).
    let r = measure(&c, anchor_backend::verify::rule_reviewer(), None);
    assert_eq!(
        r.gate,
        Matrix {
            tp: 10,
            fn_: 6,
            fp: 0,
            tn: 4
        },
        "{}",
        r.to_markdown()
    );
    let fn_of = |cat: &str| r.by_category[cat].fn_;
    assert_eq!(fn_of("visible_test_failure"), 0);
    assert_eq!(fn_of("uncovered_branch"), 0);
    assert_eq!(fn_of("test_tampering"), 0);
    assert_eq!(fn_of("spoofed_exit"), 0);
    assert_eq!(fn_of("unclaimed_file"), 0);
    assert_eq!(fn_of("semantic"), 4);
    assert_eq!(fn_of("vacuous_test"), 2);
}

const ROLES: &str = include_str!("../../../examples/demo/roles.mock.toml");

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
            .unwrap();
        assert!(status.success());
    }
    p
}

/// The CLI end to end: a mock model reviewer (it supports every claim),
/// the JSON report, and the §8.5 stop rule as an exit status.
#[test]
fn eval_cli_with_mock_model_reviewer() {
    if !python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let roles = dir.path().join("roles.toml");
    std::fs::write(&roles, ROLES.replace("\r\n", "\n")).unwrap();
    let script = dir.path().join("reviewer.json");
    std::fs::write(
        &script,
        serde_json::json!({ "steps": [
            { "content": "", "prompt_tokens": 1000, "completion_tokens": 50,
              "tool_calls": [{ "tool": "submit_verdicts", "args": { "verdicts": [
                { "claim_id": "c1", "verdict": "supported", "reason_code": "looks_right" }
              ]}}]},
            { "content": "done", "prompt_tokens": 1000, "completion_tokens": 10 },
        ]})
        .to_string(),
    )
    .unwrap();
    let json = dir.path().join("report.json");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_anchor"))
        .args(["eval", "run", "--corpus"])
        .arg(corpus_dir())
        .args(["--only", "clamp", "--reviewer", "model", "--reviewer-roles"])
        .arg(&roles)
        .arg("--executor-roles")
        .arg(&roles)
        .arg("--mock-reviewer")
        .arg(&script)
        .arg("--agent")
        .arg(agent_bin())
        .arg("--json")
        .arg(&json)
        .args(["--max-fn-rate", "0.0"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The mock reviewer approves what reaches it, so bugs merge: the stop
    // rule fails the run.
    assert!(!out.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("STOP"), "{stderr}");
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    let r = &j["report"];
    assert_eq!(r["reviewer"], "model");
    assert_eq!(r["gate"]["tn"], 1, "{stdout}");
    // Each variant that reached review cost one review session.
    let reached = r["variants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["reached_review"] == true)
        .count() as u64;
    assert_eq!(r["usage"]["sessions"], reached);
    assert!(r["usage"]["tokens_in"].as_u64().unwrap() > 0);
}
