use anchor_protocol::{
    CHECKPOINT_FORMAT_VERSION, ChatMessage, ChatRole, CheckpointReason, FileClaimState,
    PendingDecision, TaskState, TaskStatus,
};

use super::*;

fn sample(session: &str, turn: u64, filler: usize) -> Checkpoint {
    Checkpoint {
        format_version: CHECKPOINT_FORMAT_VERSION,
        checkpoint_id: format!("{session}-t{turn}"),
        session_id: session.into(),
        reason: CheckpointReason::TurnBoundary,
        turn,
        model_id: "local-model".into(),
        context_used_tokens: 800,
        context_window_tokens: 1000,
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: "sys".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            ChatMessage {
                role: ChatRole::User,
                content: "x".repeat(filler),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ],
        tasks: vec![TaskState {
            id: "t1".into(),
            description: "implement".into(),
            status: TaskStatus::InProgress,
        }],
        file_claims: vec![FileClaimState {
            path: "src/".into(),
            claim_type: "edit".into(),
        }],
        pending_decisions: vec![PendingDecision {
            id: "d1".into(),
            summary: "pick an auth scheme".into(),
        }],
        summary: Some("working on auth".into()),
    }
}

#[test]
fn tracker_pauses_at_threshold() {
    let mut t = ContextTracker::new(DEFAULT_THRESHOLD).unwrap();
    assert!(matches!(t.update("m", 500, 1000), ContextStatus::Ok { .. }));
    assert!(matches!(t.update("m", 799, 1000), ContextStatus::Ok { .. }));
    assert!(matches!(
        t.update("m", 800, 1000),
        ContextStatus::PauseRequired { .. }
    ));
    assert!(matches!(
        t.update("m", 999, 1000),
        ContextStatus::PauseRequired { .. }
    ));
    // Per model.
    assert!(matches!(
        t.update("other", 10, 1000),
        ContextStatus::Ok { .. }
    ));
    assert_eq!(t.ratio("m"), Some(0.999));
}

#[test]
fn tracker_rejects_bad_threshold() {
    assert!(ContextTracker::new(0.0).is_err());
    assert!(ContextTracker::new(1.5).is_err());
    assert!(ContextTracker::new(f64::NAN).is_err());
    assert!(ContextTracker::new(1.0).is_ok());
}

/// §5.4 test: the checkpoint contains all required fields and round-trips.
#[test]
fn checkpoint_contains_required_fields() {
    let d = tempfile::tempdir().unwrap();
    let s = CheckpointStore::open(d.path()).unwrap();
    let cp = sample("s1", 3, 10);
    s.save(&cp).unwrap();
    let back = s.load("s1-t3").unwrap();
    assert_eq!(back, cp);
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(d.path().join("s1-t3.json")).unwrap()).unwrap();
    for field in [
        "messages",
        "tasks",
        "file_claims",
        "pending_decisions",
        "summary",
        "turn",
        "session_id",
        "context_used_tokens",
        "context_window_tokens",
    ] {
        assert!(v.get(field).is_some(), "missing {field}");
    }
}

#[test]
fn latest_picks_highest_turn_per_session() {
    let d = tempfile::tempdir().unwrap();
    let s = CheckpointStore::open(d.path()).unwrap();
    for t in [1, 5, 3] {
        s.save(&sample("a", t, 1)).unwrap();
    }
    s.save(&sample("b", 9, 1)).unwrap();
    assert_eq!(s.latest(Some("a")).unwrap().unwrap().turn, 5);
    assert_eq!(s.latest(None).unwrap().unwrap().turn, 9);
    assert!(s.latest(Some("zzz")).unwrap().is_none());
}

#[test]
fn load_rejects_path_like_ids_and_corrupt_files() {
    let d = tempfile::tempdir().unwrap();
    let s = CheckpointStore::open(d.path()).unwrap();
    assert!(s.load("../etc/passwd").is_err());
    std::fs::write(d.path().join("broken.json"), b"{not json").unwrap();
    let (ok, bad) = s.list().unwrap();
    assert!(ok.is_empty());
    assert_eq!(bad.len(), 1, "corrupt checkpoints are reported, not hidden");
}

#[test]
fn invalid_checkpoint_refused_on_save() {
    let d = tempfile::tempdir().unwrap();
    let s = CheckpointStore::open(d.path()).unwrap();
    let mut cp = sample("s", 1, 1);
    cp.checkpoint_id = "../escape".into();
    assert!(s.save(&cp).is_err());
    assert!(std::fs::read_dir(d.path()).unwrap().next().is_none());
}

const CHILD_ENV: &str = "ANCHOR_CHECKPOINT_KILL_TEST_DIR";

/// Helper run in a child process by `kill_mid_write_leaves_valid_checkpoint`.
/// Writes large checkpoints in a tight loop until killed.
#[test]
fn child_checkpoint_writer() {
    let Ok(dir) = std::env::var(CHILD_ENV) else {
        return;
    };
    let s = CheckpointStore::open(Path::new(&dir)).unwrap();
    let mut turn = 0;
    loop {
        turn += 1;
        s.save(&sample("killed", turn, 2 << 20)).unwrap();
    }
}

/// §5.4(e) test: killing the process mid-write leaves only complete, valid
/// checkpoints on disk.
#[test]
fn kill_mid_write_leaves_valid_checkpoint() {
    let d = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "tests::child_checkpoint_writer",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, d.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Wait until a few checkpoints exist so the kill lands mid-write.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let n = std::fs::read_dir(d.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .map(|e| e.file_name().to_string_lossy().ends_with(".json"))
                    .unwrap_or(false)
            })
            .count();
        if n >= 3 || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    child.kill().unwrap(); // SIGKILL on Unix, TerminateProcess on Windows
    child.wait().unwrap();

    // Every visible checkpoint is complete and valid; the store recovers.
    let s = CheckpointStore::open(d.path()).unwrap();
    let (ok, bad) = s.list().unwrap();
    assert!(bad.is_empty(), "partial checkpoints visible: {bad:?}");
    assert!(ok.len() >= 3, "child wrote {} checkpoints", ok.len());
    let latest = s.latest(Some("killed")).unwrap().unwrap();
    assert_eq!(latest.messages[1].content.len(), 2 << 20);
    // Temp files from the killed writer were cleaned up on open.
    assert!(std::fs::read_dir(d.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(TMP_PREFIX)
    }));
}
