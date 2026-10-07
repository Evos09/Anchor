//! Verifier plugins outside the gate (Phase 8, ADR-0009): the example
//! plugin conforms to the published contract, and a plugin that hangs is
//! stopped at its time limit and fails closed.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use anchor_backend::default_exec_read_only;
use anchor_backend::verifier::{Plugin, PluginVerifier, Runner, conformance};
use anchor_protocol::verify::{ReviewInput, Verdict};
use anchor_verify::Reviewer;

const PY: &str = if cfg!(windows) { "python" } else { "python3" };

fn python() -> bool {
    std::process::Command::new(PY)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn runner() -> Runner {
    Runner::new(
        std::env::var("PATH").unwrap_or_default(),
        default_exec_read_only(),
    )
}

/// Done-when (spec §14 Phase 8): a third party can build a verifier plugin
/// from the docs alone. The example, written that way, passes every
/// conformance check in the real sandbox.
#[test]
fn the_example_plugin_conforms() {
    if !python() {
        eprintln!("{PY} not found: skipped");
        return;
    }
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/verifier-python/verifier.toml");
    let plugin = Plugin::load(&manifest, &std::env::var("PATH").unwrap_or_default()).unwrap();
    let checks = conformance(&plugin, &runner());
    assert!(checks.len() >= 6);
    for c in &checks {
        assert!(c.ok, "{}: {}", c.name, c.detail);
    }
    let by_name = |n: &str| checks.iter().find(|c| c.name == n).unwrap().detail.clone();
    assert!(by_name("verify debug-leftover").contains("refuted"));
    assert!(by_name("verify passing").contains("c1=supported"));
}

/// A plugin that never answers is killed at its time limit, and every
/// claim is unverifiable.
#[test]
fn a_hanging_plugin_times_out_and_fails_closed() {
    if !python() {
        eprintln!("{PY} not found: skipped");
        return;
    }
    let d = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Answers describe at once; never answers verify.
    std::fs::write(
        d.path().join("verifier.py"),
        "import json, sys, time\n\
         req = json.loads(sys.stdin.readline())\n\
         if req['msg']['kind'] == 'describe':\n    \
             print(json.dumps({'v': 1, 'id': req['id'], 'msg': {'kind': 'info', \
             'payload': {'name': 'sleeper', 'version': '1', 'protocol': [1]}}}))\n    \
             sys.exit(0)\n\
         time.sleep(120)\n",
    )
    .unwrap();
    std::fs::write(
        d.path().join("verifier.toml"),
        format!("name = \"sleeper\"\ncommand = [\"{PY}\", \"verifier.py\"]\ntimeout_secs = 2\n"),
    )
    .unwrap();
    let plugin = Plugin::load(d.path(), &std::env::var("PATH").unwrap_or_default()).unwrap();
    let input: ReviewInput =
        serde_json::from_str(include_str!("../../../docs/verifier/fixtures/passing.json")).unwrap();
    // Untimed warm-up: the first sandboxed run on a fresh Windows runner
    // pays a one-time ACL grant over the PATH trees (CI run 37694430534),
    // which the time bound below is not about.
    runner().describe(&plugin, 1).unwrap();
    let mut r = PluginVerifier::new(plugin, runner());
    let t = Instant::now();
    let vs = r.review(&input).unwrap();
    let elapsed = t.elapsed();
    // The verdicts first, so a timeout that did not fire shows its note.
    assert_eq!(vs.len(), input.claims.len());
    for v in &vs {
        assert_eq!(v.verdict, Verdict::Unverifiable);
        assert_eq!(v.reason_code, "verifier_failed");
        assert!(v.note.contains("timed out"), "{}", v.note);
    }
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
}
