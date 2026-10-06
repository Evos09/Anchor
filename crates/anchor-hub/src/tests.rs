use std::sync::{Arc, Barrier};

use serde_json::json;

use super::*;

struct Fx {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    ws: std::path::PathBuf,
    audit: Arc<anchor_policy::AuditLog>,
    clock: Arc<ManualClock>,
    hub: Hub,
}

fn fx_with(cfg: HubConfig) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let db = dir.path().join("hub.sqlite");
    let audit = Arc::new(anchor_policy::AuditLog::open(&dir.path().join("audit.jsonl")).unwrap());
    let clock = Arc::new(ManualClock::new(1_000_000));
    let hub = Hub::open(&db, &ws, audit.clone(), clock.clone(), cfg).unwrap();
    Fx {
        _dir: dir,
        db,
        ws,
        audit,
        clock,
        hub,
    }
}

fn fx() -> Fx {
    fx_with(HubConfig::default())
}

fn agent(h: &Hub, name: &str, within: &[&str]) -> (Caller, String) {
    let (a, token) = h
        .register_agent(
            Caller::User,
            NewAgent {
                display_name: name.into(),
                model_id: format!("{name}-model"),
                role: "executor".into(),
                family: format!("{name}-family"),
                capabilities: json!({}),
                permissions: Permissions::collaborator(within),
            },
        )
        .unwrap();
    (Caller::Agent(a.id), token)
}

fn denied_entries(f: &Fx) -> Vec<anchor_policy::AuditEntry> {
    anchor_policy::AuditLog::read_entries(f.audit.path())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "hub_denied")
        .collect()
}

#[test]
fn only_the_user_registers_agents_and_tokens_authenticate() {
    let f = fx();
    let (a, token) = agent(&f.hub, "alpha", &["."]);
    assert_eq!(f.hub.authenticate(&token).unwrap(), a);
    assert!(matches!(
        f.hub
            .authenticate("anc_0000000000000000000000000000000000000000000000000000000000000000"),
        Err(HubError::Unauthenticated)
    ));
    assert!(f.hub.authenticate("garbage").is_err());
    // An agent cannot register another agent (no self-escalation).
    let r = f.hub.register_agent(
        a,
        NewAgent {
            display_name: "evil".into(),
            model_id: "m".into(),
            role: "r".into(),
            family: "f".into(),
            capabilities: json!({}),
            permissions: Permissions::collaborator(&["."]),
        },
    );
    assert!(matches!(r, Err(HubError::Forbidden(_))));
    assert_eq!(f.hub.agents().unwrap().len(), 1);
    // Retired agents' tokens stop working and their calls are refused.
    let Caller::Agent(id) = a else { unreachable!() };
    f.hub.retire_agent(Caller::User, id).unwrap();
    assert!(matches!(
        f.hub.authenticate(&token),
        Err(HubError::Unauthenticated)
    ));
    assert!(matches!(
        f.hub.request_claim(a, "x", ClaimType::Edit, false, None),
        Err(HubError::Unauthenticated)
    ));
}

#[test]
fn claim_conflict_rules() {
    let f = fx();
    let (a, _) = agent(&f.hub, "alpha", &["."]);
    let (b, _) = agent(&f.hub, "beta", &["."]);
    use ClaimType::*;
    // (a's claim, b's request, conflict?)
    let cases = [
        ((Edit, false, "src/"), (Edit, false, "src/a.rs"), true),
        ((Edit, false, "src/a.rs"), (Edit, false, "src/b.rs"), false),
        ((Edit, false, "a.txt"), (Read, false, "a.txt"), false),
        ((Edit, false, "a.txt"), (Read, true, "a.txt"), true),
        ((Read, true, "a.txt"), (Edit, false, "a.txt"), true),
        ((Read, false, "a.txt"), (Edit, false, "a.txt"), false),
        ((Edit, false, "a.txt"), (Review, false, "a.txt"), true),
        ((Review, false, "lib/"), (Edit, false, "lib/x"), true),
        ((Review, false, "a.txt"), (Review, false, "a.txt"), false),
        ((Read, false, "."), (Read, true, "x"), false),
    ];
    for ((t1, i1, p1), (t2, i2, p2), conflict) in cases {
        let c1 = f.hub.request_claim(a, p1, t1, i1, None).unwrap();
        let r = f.hub.request_claim(b, p2, t2, i2, None);
        match (conflict, r) {
            (true, Err(HubError::Conflict(c))) => assert_eq!(c.id, c1.id),
            (false, Ok(c2)) => f.hub.release_claim(b, c2.id).unwrap(),
            (want, got) => panic!("{p1} {t1} vs {p2} {t2}: want conflict={want}, got {got:?}"),
        }
        f.hub.release_claim(a, c1.id).unwrap();
    }
    // An agent's own claims never conflict with each other.
    f.hub.request_claim(a, "z", Edit, false, None).unwrap();
    f.hub.request_claim(a, "z", Edit, false, None).unwrap();
}

#[test]
fn concurrent_claims_on_one_file_have_exactly_one_winner() {
    let f = fx();
    let hub = Arc::new(f.hub);
    let (a, _) = agent(&hub, "alpha", &["."]);
    let (b, _) = agent(&hub, "beta", &["."]);
    for i in 0..100 {
        let path = format!("shared-{i}.txt");
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = [a, b]
            .into_iter()
            .map(|who| {
                let (hub, barrier, path) = (hub.clone(), barrier.clone(), path.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    hub.request_claim(who, &path, ClaimType::Edit, false, None)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results
            .iter()
            .filter(|r| matches!(r, Err(HubError::Conflict(_))))
            .count();
        assert_eq!((wins, conflicts), (1, 1), "{results:?}");
    }
}

#[test]
fn claims_outside_grants_are_refused_and_audited() {
    let f = fx();
    let (a, _) = agent(&f.hub, "alpha", &["src/"]);
    let (b, _) = agent(&f.hub, "beta", &["."]);
    let before = denied_entries(&f).len();
    for (path, ty) in [
        ("tests/x.rs", ClaimType::Edit),
        (".", ClaimType::Edit),
        ("srcx/a", ClaimType::Read),
        ("../src/a", ClaimType::Edit),
        ("/etc/passwd", ClaimType::Read),
    ] {
        assert!(
            f.hub.request_claim(a, path, ty, false, None).is_err(),
            "{path}"
        );
    }
    assert!(
        f.hub
            .request_claim(a, "src/a.rs", ClaimType::Edit, false, None)
            .is_ok()
    );
    // Releasing or renewing another agent's claim is refused.
    let c = f
        .hub
        .request_claim(b, "doc.md", ClaimType::Edit, false, None)
        .unwrap();
    assert!(matches!(
        f.hub.release_claim(a, c.id),
        Err(HubError::Forbidden(_))
    ));
    assert!(matches!(
        f.hub.renew_claim(a, c.id, None),
        Err(HubError::Forbidden(_))
    ));
    // Neither the user nor the system holds claims.
    assert!(
        f.hub
            .request_claim(Caller::User, "x", ClaimType::Edit, false, None)
            .is_err()
    );
    assert_eq!(denied_entries(&f).len(), before + 8);
    // A narrower grant of claim types.
    let (c_agent, _) = f
        .hub
        .register_agent(
            Caller::User,
            NewAgent {
                display_name: "reader".into(),
                model_id: "m".into(),
                role: "reviewer".into(),
                family: "f".into(),
                capabilities: json!({}),
                permissions: Permissions {
                    claim_within: vec![".".into()],
                    claim_types: vec![ClaimType::Read],
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let r = f
        .hub
        .request_claim(Caller::Agent(c_agent.id), "x", ClaimType::Edit, false, None);
    assert!(matches!(r, Err(HubError::Forbidden(_))));
    assert!(f.hub.verify().unwrap().ok());
}

#[test]
fn leases_expire_and_silent_agents_are_lost() {
    let f = fx();
    let (a, _) = agent(&f.hub, "alpha", &["."]);
    let (b, _) = agent(&f.hub, "beta", &["."]);
    let c = f
        .hub
        .request_claim(a, "f", ClaimType::Edit, false, Some(10_000))
        .unwrap();
    assert_eq!(c.lease_expires_at, 1_000_000 + 10_000);
    f.clock.advance(9_000);
    assert!(
        f.hub
            .request_claim(b, "f", ClaimType::Edit, false, None)
            .is_err()
    );
    let c = f.hub.renew_claim(a, c.id, Some(10_000)).unwrap();
    f.clock.advance(9_000);
    assert!(
        f.hub
            .request_claim(b, "f", ClaimType::Edit, false, None)
            .is_err()
    );
    f.clock.advance(2_000);
    // Expired: now free.
    assert!(
        f.hub
            .request_claim(b, "f", ClaimType::Edit, false, None)
            .is_ok()
    );
    let all = f.hub.list_claims(Caller::User, false).unwrap();
    assert_eq!(
        all.iter().find(|x| x.id == c.id).unwrap().status,
        ClaimStatus::Expired
    );

    // alpha stays silent past two heartbeat intervals while beta heartbeats.
    let task = f
        .hub
        .create_task(Caller::User, "work", &[], Some(1))
        .unwrap();
    f.hub
        .update_task_status(a, task.id, TaskStatus::InProgress)
        .unwrap();
    let held = f
        .hub
        .request_claim(a, "g", ClaimType::Edit, false, None)
        .unwrap();
    for _ in 0..3 {
        f.clock.advance(50_000);
        f.hub.heartbeat(b).unwrap();
    }
    let alpha = f.hub.agent(1).unwrap();
    assert_eq!(alpha.status, AgentStatus::Lost);
    assert_eq!(f.hub.agent(2).unwrap().status, AgentStatus::Active);
    let claims = f.hub.list_claims(Caller::User, false).unwrap();
    assert_eq!(
        claims.iter().find(|x| x.id == held.id).unwrap().status,
        ClaimStatus::Expired
    );
    assert_eq!(
        f.hub.task(Caller::User, task.id).unwrap().status,
        TaskStatus::Pending
    );
    // A lost agent that calls again is active again.
    f.hub.heartbeat(a).unwrap();
    assert_eq!(f.hub.agent(1).unwrap().status, AgentStatus::Active);
    assert!(f.hub.verify().unwrap().ok());
}

#[test]
fn messages_are_persisted_visible_only_to_their_parties_and_holdable() {
    let cfg = HubConfig {
        approve_message_types: vec![MessageType::Decision],
        ..Default::default()
    };
    let f = fx_with(cfg);
    let (a, _) = agent(&f.hub, "alpha", &["."]);
    let (b, _) = agent(&f.hub, "beta", &["."]);
    let (c, _) = agent(&f.hub, "gamma", &["."]);
    let Caller::Agent(bid) = b else {
        unreachable!()
    };
    let m1 = f
        .hub
        .send_message(
            a,
            Recipient::Agent(bid),
            MessageType::Task,
            json!({"text": "write tests"}),
        )
        .unwrap();
    assert_eq!(m1.from_agent_id, Some(1));
    assert_eq!(m1.state, MessageState::Delivered);
    let held = f
        .hub
        .send_message(
            a,
            Recipient::Agent(bid),
            MessageType::Decision,
            json!({"text": "use sqlite"}),
        )
        .unwrap();
    assert_eq!(held.state, MessageState::Held);
    f.hub
        .send_message(
            b,
            Recipient::Room("general".into()),
            MessageType::Status,
            json!("hello"),
        )
        .unwrap();

    let inbox = |who| f.hub.query_messages(who, MessageQuery::default()).unwrap();
    let ids = |v: Vec<Message>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(
        ids(inbox(b)),
        vec![1, 3],
        "held message invisible to its recipient"
    );
    assert_eq!(ids(inbox(c)), vec![3], "gamma sees only the room");
    assert_eq!(ids(inbox(a)), vec![1, 2, 3], "the sender sees what it sent");
    // Only the user releases a held message.
    assert!(matches!(
        f.hub.approve_message(b, held.id),
        Err(HubError::Forbidden(_))
    ));
    assert!(matches!(
        f.hub.approve_message(a, held.id),
        Err(HubError::Forbidden(_))
    ));
    f.hub.approve_message(Caller::User, held.id).unwrap();
    assert_eq!(ids(inbox(b)), vec![1, 2, 3]);
    assert_eq!(inbox(b)[1].approved_by.as_deref(), Some("user"));
    // since_id paging.
    let newer = f
        .hub
        .query_messages(
            b,
            MessageQuery {
                since_id: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(ids(newer), vec![3]);

    // Persistence: reopen the database.
    drop(f.hub);
    let hub = Hub::open(
        &f.db,
        &f.ws,
        f.audit.clone(),
        f.clock.clone(),
        HubConfig::default(),
    )
    .unwrap();
    let all = hub
        .query_messages(Caller::User, MessageQuery::default())
        .unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].content, json!({"text": "write tests"}));
    assert!(hub.verify().unwrap().ok());
}

#[test]
fn message_permissions_and_limits() {
    let f = fx();
    let (a, _) = f
        .hub
        .register_agent(
            Caller::User,
            NewAgent {
                display_name: "quiet".into(),
                model_id: "m".into(),
                role: "r".into(),
                family: "f".into(),
                capabilities: json!({}),
                permissions: Permissions {
                    message_types: vec![MessageType::Status],
                    messages_need_approval: true,
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let (b, _) = agent(&f.hub, "beta", &["."]);
    let a = Caller::Agent(a.id);
    let Caller::Agent(bid) = b else {
        unreachable!()
    };
    assert!(matches!(
        f.hub
            .send_message(a, Recipient::Agent(bid), MessageType::Task, json!(1)),
        Err(HubError::Forbidden(_))
    ));
    let m = f
        .hub
        .send_message(a, Recipient::Agent(bid), MessageType::Status, json!(1))
        .unwrap();
    assert_eq!(m.state, MessageState::Held);
    assert!(
        f.hub
            .send_message(b, Recipient::Agent(bid), MessageType::Status, json!(1))
            .is_err()
    );
    assert!(
        f.hub
            .send_message(b, Recipient::Agent(99), MessageType::Status, json!(1))
            .is_err()
    );
    assert!(
        f.hub
            .send_message(
                b,
                Recipient::Room("bad room".into()),
                MessageType::Status,
                json!(1)
            )
            .is_err()
    );
    let big = "x".repeat(70_000);
    assert!(
        f.hub
            .send_message(
                b,
                Recipient::Room("r".into()),
                MessageType::Status,
                json!(big)
            )
            .is_err()
    );
}

#[test]
fn decisions_tasks_and_artifacts_follow_permissions() {
    let f = fx();
    let (a, _) = agent(&f.hub, "alpha", &["."]);
    let (b, _) = agent(&f.hub, "beta", &["."]);
    std::fs::write(f.ws.join("out.log"), "test output").unwrap();
    let ws = anchor_fs::Workspace::open(&f.ws).unwrap();
    let file = VerifiedFile::read(&ws, "out.log").unwrap();
    assert_eq!(
        file.sha256(),
        anchor_policy::audit::sha256_hex(b"test output")
    );
    let art = f
        .hub
        .register_artifact(a, ArtifactKind::TestOutput, &file, json!({"exit": 0}))
        .unwrap();
    assert!(VerifiedFile::read(&ws, "../escape").is_err());

    let d = f
        .hub
        .propose_decision(a, "Use SQLite", "spec §6", Some(art.id))
        .unwrap();
    assert!(f.hub.propose_decision(a, "x", "y", Some(999)).is_err());
    assert!(matches!(
        f.hub.decide(a, d.id, true, "self-approval"),
        Err(HubError::Forbidden(_))
    ));
    assert!(matches!(
        f.hub.decide(b, d.id, true, ""),
        Err(HubError::Forbidden(_))
    ));
    assert_eq!(
        f.hub.decide(Caller::User, d.id, true, "ok").unwrap().status,
        DecisionStatus::Accepted
    );
    assert!(f.hub.decide(Caller::User, d.id, false, "again").is_err());

    let t1 = f.hub.create_task(a, "build", &[], Some(1)).unwrap();
    let t2 = f.hub.create_task(a, "test", &[t1.id], Some(2)).unwrap();
    assert!(matches!(
        f.hub.update_task_status(a, t2.id, TaskStatus::Done),
        Err(HubError::Forbidden(_))
    ));
    assert!(matches!(
        f.hub.update_task_status(b, t2.id, TaskStatus::InProgress),
        Err(HubError::Forbidden(_))
    ));
    f.hub
        .update_task_status(a, t1.id, TaskStatus::Done)
        .unwrap();
    f.hub
        .update_task_status(b, t2.id, TaskStatus::InProgress)
        .unwrap();
    assert!(f.hub.create_task(a, "x", &[42], None).is_err());
    assert!(f.hub.verify().unwrap().ok());
}

#[test]
fn evidence_is_backend_only_hashed_and_immutable() {
    let f = fx();
    let (a, _) = agent(&f.hub, "alpha", &["."]);
    let log = b"Ran 1 test\n\nOK\n";
    // Agents and the user cannot record evidence; refusals are audited.
    for caller in [a, Caller::User] {
        assert!(matches!(
            f.hub.register_evidence(
                caller,
                ArtifactKind::TestOutput,
                "1-1/test.log",
                log,
                json!({})
            ),
            Err(HubError::Forbidden(_))
        ));
    }
    let art = f
        .hub
        .register_evidence(
            Caller::System,
            ArtifactKind::TestOutput,
            "1-1/test.log",
            log,
            json!({"exit": 0}),
        )
        .unwrap();
    assert_eq!(art.path, "evidence:1-1/test.log");
    assert_eq!(art.sha256, anchor_policy::audit::sha256_hex(log));
    assert_eq!(art.created_by_agent_id, None);
    let file = f.db.parent().unwrap().join("evidence/1-1/test.log");
    assert_eq!(std::fs::read(&file).unwrap(), log);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }
    assert_eq!(f.hub.read_evidence(Caller::System, &art).unwrap(), log);
    // Written once: the same bytes again are fine, different bytes are not.
    f.hub
        .register_evidence(
            Caller::System,
            ArtifactKind::TestOutput,
            "1-1/test.log",
            log,
            json!({}),
        )
        .unwrap();
    assert!(
        f.hub
            .register_evidence(
                Caller::System,
                ArtifactKind::TestOutput,
                "1-1/test.log",
                b"forged",
                json!({})
            )
            .is_err()
    );
    // Names cannot escape the evidence directory.
    for bad in ["../x/y", "a/../b", "a", "a/b/c", "./x", "../x", "a/.."] {
        assert!(
            f.hub
                .register_evidence(Caller::System, ArtifactKind::Log, bad, b"x", json!({}))
                .is_err(),
            "{bad}"
        );
    }
    // A tampered file is detected on read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(&file, b"tampered").unwrap();
    assert!(f.hub.read_evidence(Caller::System, &art).is_err());
    let listed = f
        .hub
        .list_artifacts(Caller::System, Some("evidence:1-"))
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert!(f.hub.list_artifacts(a, None).is_err());
    assert!(f.hub.verify().unwrap().ok());
}

#[test]
fn verify_detects_database_tampering() {
    for attack in [
        "UPDATE message SET content_json = '\"forged\"' WHERE id = 1",
        "DELETE FROM message WHERE id = 1",
        "UPDATE claim SET agent_id = 2 WHERE id = 1",
        "UPDATE agent_token SET sha256 = 'aa' WHERE id = 1",
        "UPDATE agent SET permissions_json = '{}' WHERE id = 1",
        "INSERT INTO message (project_id, from_agent_id, to_agent_id, message_type, content_json, requires_approval, state, created_at) VALUES (1, 1, 2, 'task', '1', 0, 'delivered', 0)",
    ] {
        let f = fx();
        let (a, _) = agent(&f.hub, "alpha", &["."]);
        agent(&f.hub, "beta", &["."]);
        f.hub
            .request_claim(a, "x", ClaimType::Edit, false, None)
            .unwrap();
        f.hub
            .send_message(a, Recipient::Agent(2), MessageType::Task, json!("do it"))
            .unwrap();
        assert!(f.hub.verify().unwrap().ok());
        let conn = rusqlite::Connection::open(&f.db).unwrap();
        conn.execute(attack, []).unwrap();
        drop(conn);
        let r = f.hub.verify().unwrap();
        assert!(!r.ok(), "undetected: {attack}");
    }
}

#[test]
fn verify_detects_audit_truncation() {
    let f = fx();
    let (a, _) = agent(&f.hub, "alpha", &["."]);
    f.hub
        .request_claim(a, "x", ClaimType::Edit, false, None)
        .unwrap();
    assert!(f.hub.verify().unwrap().ok());
    let path = f.audit.path().to_path_buf();
    let text = std::fs::read_to_string(&path).unwrap();
    let keep: Vec<&str> = text.lines().collect();
    let cut = keep[..keep.len() - 1].join("\n") + "\n";
    std::fs::write(&path, cut).unwrap();
    let r = f.hub.verify().unwrap();
    assert!(!r.ok());
    assert!(
        r.errors
            .iter()
            .any(|e| e.contains("truncated") || e.contains("deleted") || e.contains("no audit")),
        "{:?}",
        r.errors
    );
}
