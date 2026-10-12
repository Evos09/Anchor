use super::*;

fn roundtrip(msg: Message) -> Frame {
    let f = Frame::new(7, msg);
    let s = encode(&f).expect("encode");
    let back = decode(&s).expect("decode");
    assert_eq!(f, back);
    back
}

#[test]
fn roundtrips_every_direction() {
    roundtrip(Message::Hello(Hello {
        runtime_version: "0.1.0".into(),
        protocol_version: PROTOCOL_VERSION,
    }));
    roundtrip(Message::ToolCall(ToolCall {
        call_id: "c1".into(),
        request: ToolRequest::EditFile {
            path: "DEVLOG.md".into(),
            edit: EditOp::Append { text: "x\n".into() },
        },
    }));
    roundtrip(Message::ToolCall(ToolCall {
        call_id: "c2".into(),
        request: ToolRequest::Hub {
            tool: "hub_request_claim".into(),
            args: serde_json::json!({ "path": "src/", "claim_type": "edit" }),
        },
    }));
    roundtrip(Message::TurnStarted { turn: 3 });
    roundtrip(Message::Shutdown);
    roundtrip(Message::Resume(Resume {
        choice: ResumeChoice::FreshSession,
    }));
}

#[test]
fn rejects_version_mismatch() {
    // A Phase 4 (v2) peer is refused with a precise error.
    let line = r#"{"v":2,"id":1,"msg":{"kind":"shutdown"}}"#;
    match decode(line) {
        Err(ProtocolError::VersionMismatch {
            expected: 3,
            got: 2,
        }) => {}
        other => panic!("expected version mismatch, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_top_level_field() {
    let line = r#"{"v":3,"id":1,"msg":{"kind":"shutdown"},"extra":true}"#;
    assert!(matches!(decode(line), Err(ProtocolError::Malformed(_))));
}

#[test]
fn rejects_unknown_payload_field() {
    let line = r#"{"v":3,"id":1,"msg":{"kind":"tool_call","payload":{"call_id":"a","request":{"tool":"read_file","args":{"path":"x","follow_symlinks":true}}}}}"#;
    assert!(matches!(decode(line), Err(ProtocolError::Malformed(_))));
}

#[test]
fn rejects_unknown_struct_variant_field() {
    let line = r#"{"v":3,"id":1,"msg":{"kind":"turn_started","payload":{"turn":1,"x":2}}}"#;
    assert!(matches!(decode(line), Err(ProtocolError::Malformed(_))));
}

#[test]
fn rejects_unknown_tool() {
    let line = r#"{"v":3,"id":1,"msg":{"kind":"tool_call","payload":{"call_id":"a","request":{"tool":"exec_shell","args":{"cmd":"ls"}}}}}"#;
    assert!(matches!(decode(line), Err(ProtocolError::Malformed(_))));
}

#[test]
fn rejects_nul_in_path() {
    let f = Frame::new(
        1,
        Message::ToolCall(ToolCall {
            call_id: "a".into(),
            request: ToolRequest::ReadFile {
                path: "a\0b".into(),
            },
        }),
    );
    assert!(matches!(encode(&f), Err(ProtocolError::Invalid(_))));
    let s = serde_json::to_string(&f).unwrap();
    assert!(matches!(decode(&s), Err(ProtocolError::Invalid(_))));
}

/// Review finding T-3: escape sequences in a model-chosen name never reach
/// a prompt.
#[test]
fn rejects_control_characters_in_names() {
    for path in ["notes\u{1b}[8m.txt", "a\u{7}b", "line\nbreak", "x\u{9b}y"] {
        let f = Frame::new(
            1,
            Message::ToolCall(ToolCall {
                call_id: "a".into(),
                request: ToolRequest::WriteFile {
                    path: path.into(),
                    content: "x".into(),
                },
            }),
        );
        assert!(
            matches!(encode(&f), Err(ProtocolError::Invalid(_))),
            "{path:?}"
        );
    }
}

#[test]
fn rejects_oversize_frame_without_buffering_it_all() {
    let mut big = vec![b'a'; MAX_FRAME_BYTES + 10];
    big.push(b'\n');
    let mut r = std::io::BufReader::new(&big[..]);
    assert!(matches!(
        read_frame(&mut r),
        Err(ProtocolError::FrameTooLarge)
    ));
}

#[test]
fn read_write_stream() {
    let mut buf = Vec::new();
    write_frame(&mut buf, &Frame::new(1, Message::TurnStarted { turn: 1 })).unwrap();
    write_frame(&mut buf, &Frame::new(2, Message::Shutdown)).unwrap();
    let mut r = std::io::BufReader::new(&buf[..]);
    assert_eq!(read_frame(&mut r).unwrap().unwrap().id, 1);
    assert_eq!(read_frame(&mut r).unwrap().unwrap().id, 2);
    assert!(read_frame(&mut r).unwrap().is_none());
}

#[test]
fn rejects_bad_call_id() {
    let f = Frame::new(
        1,
        Message::ToolResult(ToolResult {
            call_id: "has space".into(),
            outcome: ToolOutcome::Error { detail: "x".into() },
        }),
    );
    assert!(encode(&f).is_err());
}

#[test]
fn mutating_tools_are_classified_by_tool_not_text() {
    assert!(!ToolRequest::ReadFile { path: "a".into() }.can_mutate());
    assert!(
        ToolRequest::RunCommand {
            program: "ls".into(),
            args: vec![],
            network: false
        }
        .can_mutate()
    );
}

#[test]
fn hub_requests_are_bounded() {
    let call = |tool: &str, args: serde_json::Value| {
        Frame::new(
            1,
            Message::ToolCall(ToolCall {
                call_id: "a".into(),
                request: ToolRequest::Hub {
                    tool: tool.into(),
                    args,
                },
            }),
        )
    };
    assert!(encode(&call("hub_list_claims", serde_json::json!({}))).is_ok());
    // Only hub_* names; the args must be an object of bounded size.
    for (tool, args) in [
        ("read_file", serde_json::json!({})),
        ("hub_X", serde_json::json!({})),
        ("hub_", serde_json::json!({})),
        ("hub_list_claims", serde_json::json!([1])),
        (
            "hub_send_message",
            serde_json::json!({ "text": "x".repeat(MAX_HUB_ARGS_BYTES) }),
        ),
    ] {
        let line = serde_json::to_string(&call(tool, args)).unwrap();
        assert!(decode(&line).is_err(), "{tool}");
    }
}

#[test]
fn claims_and_verdicts_on_the_wire() {
    use crate::verify::*;
    let claim = Claim {
        id: "c1".into(),
        text: "handles expired tokens".into(),
        files: vec!["auth.py".into()],
        tests: vec!["test_auth.T.test_expired".into()],
    };
    roundtrip(Message::Done(Done {
        summary: "done".into(),
        claims: vec![claim.clone()],
    }));
    // Claims are optional on the wire (sessions without claims).
    let line = r#"{"v":3,"id":1,"msg":{"kind":"done","payload":{"summary":"ok"}}}"#;
    assert!(matches!(decode(line).unwrap().msg, Message::Done(d) if d.claims.is_empty()));
    let verdict = ClaimVerdict {
        claim_id: "c1".into(),
        verdict: Verdict::Refuted,
        reason_code: "no_test_for_claim".into(),
        evidence_refs: vec!["diff:auth.py".into()],
        note: "advisory".into(),
    };
    roundtrip(Message::Verdicts(Verdicts {
        verdicts: vec![verdict.clone()],
    }));
    roundtrip(Message::ToolCall(ToolCall {
        call_id: "c3".into(),
        request: ToolRequest::SubmitClaims {
            claims: vec![claim.clone()],
        },
    }));
    // Bad claims and verdicts are refused at the wire.
    let mut bad = claim.clone();
    bad.id = "has space".into();
    assert!(
        encode(&Frame::new(
            1,
            Message::Done(Done {
                summary: String::new(),
                claims: vec![bad]
            })
        ))
        .is_err()
    );
    let mut bad = verdict.clone();
    bad.reason_code = "Not Snake".into();
    assert!(
        encode(&Frame::new(
            1,
            Message::Verdicts(Verdicts {
                verdicts: vec![bad]
            })
        ))
        .is_err()
    );
    let too_many: Vec<Claim> = (0..=MAX_CLAIMS)
        .map(|i| Claim {
            id: format!("c{i}"),
            ..claim.clone()
        })
        .collect();
    assert!(validate_claims(&too_many).is_err());
    // An unknown verdict word is malformed, not silently accepted.
    let line = r#"{"v":3,"id":1,"msg":{"kind":"verdicts","payload":{"verdicts":[{"claim_id":"c1","verdict":"probably","reason_code":"x"}]}}}"#;
    assert!(matches!(decode(line), Err(ProtocolError::Malformed(_))));
}

#[test]
fn review_tasks_are_isolated() {
    use crate::verify::*;
    let review = ReviewInput {
        claims: vec![],
        changes: vec![],
        commands: vec![],
        tests: vec![],
        coverage: None,
        claim_refs: Default::default(),
        truncated: false,
    };
    let mut task = Task {
        session_id: "s".into(),
        instruction: "review".into(),
        roles_toml: "x".into(),
        checkpoint_threshold: 0.8,
        resume_from: None,
        mock_script: None,
        hub: None,
        review: Some(Box::new(review)),
        continue_with: None,
    };
    roundtrip(Message::Task(Box::new(task.clone())));
    // A reviewer is never also a Hub agent.
    task.hub = Some(HubInfo {
        agent_id: 1,
        display_name: "r".into(),
        peers: vec![],
        tools: vec![],
    });
    assert!(Message::Task(Box::new(task)).validate().is_err());
}
