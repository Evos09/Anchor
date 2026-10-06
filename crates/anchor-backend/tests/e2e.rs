//! End-to-end tests: the real `anchor-agent` runtime process, the real
//! backend mediator, a scripted model.

// Agent commands require a sandbox that only exists on Unix so far
// (ADR-0003); on Windows they fail closed and these scenarios differ.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Mutex;

use anchor_backend::interact::Scripted;
use anchor_backend::session::{self, SessionConfig, SessionReport};
use anchor_backend::{Opened, default_exec_read_only, open_workspace};
use anchor_policy::AuditLog;
use anchor_protocol::{CheckpointReason, ResumeChoice};

/// Tests in this file share process-global counters; run them one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const ROLES: &str = include_str!("../../../examples/demo/roles.mock.toml");

fn agent_bin() -> PathBuf {
    let p = session::default_agent_bin();
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
    assert!(p.exists(), "anchor-agent not found at {}", p.display());
    p
}

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    ws: PathBuf,
}

/// A workspace with an append-only changelog, a symlink trap pointing at a
/// fake credential directory outside the workspace, and world-traversable
/// permissions (commands run as `nobody` when tests run as root).
fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(base.join("fake_home/.ssh")).unwrap();
    std::fs::write(ws.join("src/main.py"), "print('hello')\n").unwrap();
    std::fs::write(ws.join("src/util.py"), "def helper():\n    return 42\n").unwrap();
    std::fs::write(
        ws.join("CHANGELOG.md"),
        "# Changelog\n\n## 2026-10-01\n- initial\n",
    )
    .unwrap();
    std::fs::write(
        base.join("fake_home/.ssh/authorized_keys"),
        "ssh-ed25519 AAAA user@host\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::os::unix::fs::symlink(base.join("fake_home/.ssh"), ws.join("config_link")).unwrap();
        for p in [&base, &ws, &ws.join("src")] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        for f in ["src/main.py", "src/util.py", "CHANGELOG.md"] {
            std::fs::set_permissions(ws.join(f), std::fs::Permissions::from_mode(0o666)).unwrap();
        }
    }
    Fx {
        _dir: dir,
        base,
        ws,
    }
}

fn open(fx: &Fx, approve: bool, resume: ResumeChoice) -> Opened {
    open_workspace(
        &fx.ws,
        None,
        Box::new(Scripted::new(approve, resume)),
        default_exec_read_only(),
    )
    .unwrap()
}

fn run(fx: &Fx, opened: &mut Opened, script: &str, threshold: f64, roles: &str) -> SessionReport {
    let cfg = SessionConfig {
        agent_bin: agent_bin(),
        session_id: session::new_session_id(),
        instruction: "test task".into(),
        roles_toml: roles.into(),
        checkpoint_threshold: threshold,
        resume_from: None,
        mock_script: Some(script.into()),
    };
    let _ = fx;
    session::run(&cfg, &mut opened.mediator, &opened.checkpoints).unwrap()
}

const SCRIPT: &str = r###"{"steps": [
  {"content": "look", "tool_calls": [
     {"tool": "list_dir", "args": {"path": "."}},
     {"tool": "read_file", "args": {"path": "src/main.py"}}
  ], "prompt_tokens": 100, "completion_tokens": 10},
  {"content": "attacks", "tool_calls": [
     {"tool": "read_file", "args": {"path": "../../../../etc/passwd"}},
     {"tool": "read_file", "args": {"path": "config_link/authorized_keys"}},
     {"tool": "write_file", "args": {"path": "config_link/authorized_keys", "content": "pwned"}},
     {"tool": "write_file", "args": {"path": "CHANGELOG.md", "content": "rewritten"}},
     {"tool": "edit_file", "args": {"path": "CHANGELOG.md", "edit": {"op": "replace", "old": "initial", "new": "tampered"}}},
     {"tool": "write_file", "args": {"path": ".anchor/policy.toml", "content": "version = 1\n[default]\naction = \"allow\"\n"}},
     {"tool": "glob", "args": {"pattern": "../**"}},
     {"tool": "grep", "args": {"pattern": "(a+)+$", "path_glob": "/etc/*"}},
     {"tool": "run_command", "args": {"program": "/bin/sh", "args": ["-c", "id"]}},
     {"tool": "run_command", "args": {"program": "echo", "args": ["; rm -rf / #"]}}
  ], "prompt_tokens": 200, "completion_tokens": 10},
  {"content": "work", "tool_calls": [
     {"tool": "edit_file", "args": {"path": "src/main.py", "edit": {"op": "replace", "old": "print('hello')", "new": "print('hello, anchor')"}}},
     {"tool": "edit_file", "args": {"path": "CHANGELOG.md", "edit": {"op": "append", "text": "## 2026-10-06\n- greet anchor\n"}}},
     {"tool": "write_file", "args": {"path": "src/new_module.py", "content": "X = 1\n"}},
     {"tool": "run_command", "args": {"program": "python3", "args": ["src/main.py"]}}
  ], "prompt_tokens": 300, "completion_tokens": 10},
  {"content": "done", "prompt_tokens": 400, "completion_tokens": 10}
]}"###;

fn entries(opened: &Opened) -> Vec<anchor_policy::AuditEntry> {
    AuditLog::read_entries(&opened.paths.audit).unwrap()
}

/// Phase 1/2 acceptance: a task runs end to end through the separate
/// runtime process; every attack in the script is refused; legitimate work
/// lands; nothing outside the workspace changes; every file mutation was
/// approved by the engine and audited (instrumentation).
#[test]
fn end_to_end_session_enforces_every_primitive() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open(&fx, true, ResumeChoice::FreshSession);
    let mutations_before = anchor_fs::mutations_performed();
    let report = run(&fx, &mut opened, SCRIPT, 0.8, ROLES);

    assert_eq!(report.done.as_deref(), Some("done"), "{report:?}");
    assert_ne!(
        report.runtime_pid,
        std::process::id(),
        "runtime must be a separate process"
    );

    // Outside the workspace: untouched.
    assert_eq!(
        std::fs::read_to_string(fx.base.join("fake_home/.ssh/authorized_keys")).unwrap(),
        "ssh-ed25519 AAAA user@host\n"
    );
    // Append-only file: original prefix intact, append landed.
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("CHANGELOG.md")).unwrap(),
        "# Changelog\n\n## 2026-10-01\n- initial\n## 2026-10-06\n- greet anchor\n"
    );
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("src/main.py")).unwrap(),
        "print('hello, anchor')\n"
    );
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("src/new_module.py")).unwrap(),
        "X = 1\n"
    );
    // Policy file not modified by the agent.
    assert_eq!(
        std::fs::read_to_string(&opened.paths.policy).unwrap(),
        anchor_backend::DEFAULT_POLICY
    );

    // 9 attack calls denied. The tenth, `echo "; rm -rf / #"`, runs: the
    // payload is a single inert argv element (Threat 4).
    let es = entries(&opened);
    let refused: Vec<_> = es
        .iter()
        .filter(|e| e.kind == "command_refused")
        .map(|e| e.data.to_string())
        .collect();
    assert_eq!(
        report.stats.denied, 9,
        "{:?}; sandbox refusals: {refused:?}",
        report.stats
    );
    assert_eq!(report.stats.commands, 2, "sandbox refusals: {refused:?}");

    // Instrumentation: every filesystem mutation in this process went
    // through anchor-fs with an approval, and each has an audit record whose
    // decision was approved.
    let es = entries(&opened);
    let mutation_entries: Vec<_> = es.iter().filter(|e| e.kind == "mutation").collect();
    assert_eq!(report.stats.mutations, 3);
    assert_eq!(mutation_entries.len() as u64, report.stats.mutations);
    assert_eq!(
        anchor_fs::mutations_performed() - mutations_before,
        report.stats.mutations
    );
    for m in &mutation_entries {
        let id = &m.data["decision_id"];
        let approved = es.iter().any(|e| {
            (e.kind == "decision" && e.data["decision_id"] == *id && e.data["action"] == "allow")
                || (e.kind == "prompt_response"
                    && e.data["decision_id"] == *id
                    && e.data["response"] == "approve")
        });
        assert!(approved, "mutation without approval: {m:?}");
    }
    // Every decision was logged with a structured request and a rule.
    for d in es.iter().filter(|e| e.kind == "decision") {
        assert!(d.data["request"]["tool"].is_string());
        assert!(d.data["matched_rule"].is_string());
    }
    assert_eq!(
        AuditLog::verify(&opened.paths.audit).unwrap(),
        es.len() as u64
    );

    let echo = es
        .iter()
        .find(|e| e.kind == "command" && e.data["args"][0] == "; rm -rf / #")
        .expect("echo audited");
    assert_eq!(echo.data["exit"], "Exited(0)");
    assert!(echo.data["program"].as_str().unwrap().ends_with("echo"));

    // The confined python command really ran.
    let cmd = es
        .iter()
        .find(|e| e.kind == "command" && e.data["args"][0] == "src/main.py")
        .expect("command audited");
    assert_eq!(cmd.data["fs_confined"], true);
    assert_eq!(cmd.data["network_isolated"], true);
    assert_eq!(cmd.data["exit"], "Exited(0)");

    // One checkpoint per turn.
    assert_eq!(report.checkpoints.len(), 4);
    let (ok, bad) = opened.checkpoints.list().unwrap();
    assert!(bad.is_empty());
    assert_eq!(ok.len(), 4);

    // Rollback to before turn 3 restores the pre-work state exactly,
    // including removing the file the agent created.
    let log = opened.mediator.rollback_to(3).unwrap();
    assert!(!log.is_empty());
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("src/main.py")).unwrap(),
        "print('hello')\n"
    );
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("CHANGELOG.md")).unwrap(),
        "# Changelog\n\n## 2026-10-01\n- initial\n"
    );
    assert!(!fx.ws.join("src/new_module.py").exists());
    assert!(AuditLog::verify(&opened.paths.audit).is_ok());
}

/// With every prompt denied, nothing in the workspace changes.
#[test]
fn denied_prompts_change_nothing() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open(&fx, false, ResumeChoice::FreshSession);
    let before_main = std::fs::read(fx.ws.join("src/main.py")).unwrap();
    let report = run(&fx, &mut opened, SCRIPT, 0.8, ROLES);
    assert!(report.done.is_some());
    assert_eq!(
        std::fs::read(fx.ws.join("src/main.py")).unwrap(),
        before_main
    );
    assert!(!fx.ws.join("src/new_module.py").exists());
    assert_eq!(
        report.stats.mutations, 1,
        "only the policy-allowed append to the append-only file"
    );
}

/// An unread file cannot be overwritten without the user confirming the
/// deletion diff (spec §5.3(c)); the prompt shows the canonical path.
#[test]
fn overwrite_of_unread_file_requires_diff_confirmation() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open(&fx, false, ResumeChoice::FreshSession);
    let script = r#"{"steps": [
      {"content": "", "tool_calls": [{"tool": "write_file", "args": {"path": "./src/../src/util.py", "content": "clobbered\n"}}]},
      {"content": "done"}
    ]}"#;
    let report = run(&fx, &mut opened, script, 0.8, ROLES);
    assert_eq!(report.stats.denied, 1);
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("src/util.py")).unwrap(),
        "def helper():\n    return 42\n"
    );
}

/// The runtime process has no filesystem authority (Landlock self-sandbox
/// on Linux).
#[cfg(target_os = "linux")]
#[test]
fn runtime_process_cannot_touch_the_filesystem() {
    use std::io::Write;
    let fx = fixture();
    let target = fx.ws.join("src/main.py");
    let mut child = std::process::Command::new(agent_bin())
        .arg("--selftest-fs-denied")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", target.display()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("sandbox not-enforced") {
        eprintln!("SKIPPED: Landlock unavailable on this kernel: {text}");
        return;
    }
    assert!(text.contains("read DENIED"), "{text}");
    assert!(text.contains("write DENIED"), "{text}");
    assert!(!fx.ws.join("src/main.py.agent-write").exists());
}

/// Spec §5.4: a session that fills the context pauses and writes a
/// threshold checkpoint; a fresh session seeded from it resumes.
#[test]
fn context_threshold_pauses_and_fresh_session_resumes() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open(&fx, true, ResumeChoice::FreshSession);
    let script = r#"{"steps": [
      {"content": "", "tool_calls": [{"tool": "read_file", "args": {"path": "src/main.py"}}], "prompt_tokens": 30000, "completion_tokens": 10},
      {"content": "should not run"}
    ]}"#;
    let report = run(&fx, &mut opened, script, 0.8, ROLES);
    let paused = report.paused_at.clone().expect("paused");
    let cp = opened.checkpoints.load(&paused).unwrap();
    assert_eq!(cp.reason, CheckpointReason::ContextThreshold);
    assert!(cp.messages.len() >= 4);

    let cfg = SessionConfig {
        agent_bin: agent_bin(),
        session_id: cp.session_id.clone(),
        instruction: "continue".into(),
        roles_toml: ROLES.into(),
        checkpoint_threshold: 0.8,
        resume_from: Some(cp.clone()),
        mock_script: Some(r#"{"steps": [{"content": "resumed and finished"}]}"#.into()),
    };
    let r2 = session::run(&cfg, &mut opened.mediator, &opened.checkpoints).unwrap();
    assert_eq!(r2.done.as_deref(), Some("resumed and finished"));
    assert!(
        r2.checkpoints
            .iter()
            .any(|c| c.ends_with(&format!("t{:04}", cp.turn + 1)))
    );
}

/// Spec §8.2(a): the runtime refuses to start a session whose executor and
/// reviewer resolve to the same model family.
#[test]
fn same_family_reviewer_refused() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open(&fx, true, ResumeChoice::FreshSession);
    let roles = ROLES.replace("family = \"demo-family-b\"", "family = \"demo-family-a\"");
    let report = run(&fx, &mut opened, SCRIPT, 0.8, &roles);
    assert!(report.failed.unwrap().contains("same model family"));
    assert_eq!(report.stats.tool_calls, 0);
}

/// Threat 9: a crafted pre-image manifest cannot make a restore write
/// outside the workspace, and a tampered blob is rejected.
#[test]
fn crafted_manifest_cannot_escape_on_restore() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open(&fx, true, ResumeChoice::FreshSession);
    let blobs = opened.paths.preimages.join("blobs");
    let payload = b"pwned\n";
    let sha = anchor_policy::audit::hex(&anchor_fs::sha256(payload));
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join(&sha), payload).unwrap();
    for rel in [
        "../fake_home/.ssh/authorized_keys",
        "config_link/authorized_keys",
        "/etc/hostname",
    ] {
        let manifest = serde_json::json!({
            "turn": 99, "kind": "files", "entries": { rel: sha }
        });
        std::fs::write(
            opened.paths.preimages.join("turns/0000000099.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(
            opened.mediator.rollback_to(99).is_err(),
            "{rel} should be refused"
        );
    }
    assert_eq!(
        std::fs::read_to_string(fx.base.join("fake_home/.ssh/authorized_keys")).unwrap(),
        "ssh-ed25519 AAAA user@host\n"
    );
    // Tampered blob content is detected by its hash.
    let manifest =
        serde_json::json!({"turn": 99, "kind": "files", "entries": {"src/util.py": sha}});
    std::fs::write(
        opened.paths.preimages.join("turns/0000000099.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(blobs.join(&sha), b"different").unwrap();
    assert!(opened.mediator.rollback_to(99).is_err());
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("src/util.py")).unwrap(),
        "def helper():\n    return 42\n"
    );
}

/// Spec §16.4 durability: kill the backend mid-turn; a valid checkpoint and
/// an intact audit chain remain, and a new session resumes from it.
#[cfg(unix)]
#[test]
fn kill_mid_turn_leaves_valid_checkpoint_and_resumes() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let dir = fx.base.join("cfg");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("roles.toml"), ROLES).unwrap();
    std::fs::write(
        dir.join("script.json"),
        r#"{"steps": [
          {"content": "", "tool_calls": [{"tool": "read_file", "args": {"path": "src/main.py"}}]},
          {"content": "", "tool_calls": [{"tool": "run_command", "args": {"program": "sleep", "args": ["5"]}}]},
          {"content": "never"}
        ]}"#,
    )
    .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_anchor"))
        .args(["run", "--workspace"])
        .arg(&fx.ws)
        .args(["--task", "t", "--auto-approve", "--agent"])
        .arg(agent_bin())
        .arg("--roles")
        .arg(dir.join("roles.toml"))
        .arg("--mock-script")
        .arg(dir.join("script.json"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let audit = fx.ws.join(".anchor/audit.jsonl");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    // Wait until the command is running (turn 2), then kill -9.
    loop {
        let text = std::fs::read_to_string(&audit).unwrap_or_default();
        if text.contains("\"tool\":\"run_command\"") || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    child.kill().unwrap();
    child.wait().unwrap();

    let store =
        anchor_checkpoint::CheckpointStore::open(&fx.ws.join(".anchor/checkpoints")).unwrap();
    let (ok, bad) = store.list().unwrap();
    assert!(bad.is_empty(), "{bad:?}");
    let latest = store.latest(None).unwrap().expect("a checkpoint survived");
    let refused: Vec<_> = AuditLog::read_entries(&audit)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.kind == "command_refused")
        .map(|e| e.data.to_string())
        .collect();
    assert_eq!(latest.turn, 1, "sandbox refusals: {refused:?}");
    assert!(AuditLog::verify(&audit).is_ok());
    assert!(ok.iter().all(|c| c.turn == 1));

    // Resume in a fresh session.
    let mut opened = open(&fx, true, ResumeChoice::FreshSession);
    let cfg = SessionConfig {
        agent_bin: agent_bin(),
        session_id: latest.session_id.clone(),
        instruction: "resume".into(),
        roles_toml: ROLES.into(),
        checkpoint_threshold: 0.8,
        resume_from: Some(latest),
        mock_script: Some(r#"{"steps": [{"content": "recovered"}]}"#.into()),
    };
    let r = session::run(&cfg, &mut opened.mediator, &opened.checkpoints).unwrap();
    assert_eq!(r.done.as_deref(), Some("recovered"));
}
