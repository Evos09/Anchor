use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anchor_protocol::*;

use crate::agent::{AgentError, Transport, compact, default_factory, run_session};
use crate::config::{ConfigError, RolesConfig};
use crate::prompt::{PrefixMonitor, SYSTEM_PROMPT, TOOLS_JSON, prefix_hash};
use crate::provider::parse_openai_response;
use crate::router::{Router, TaskKind};

const ROLES: &str = r#"
[providers.local]
kind = "mock"
family = "family-a"

[providers.backup]
kind = "mock"
family = "family-a"

[providers.other]
kind = "mock"
family = "family-b"

[roles.planner]
provider = "local"
model_id = "planner-model"
context_window = 1000

[roles.executor]
provider = "local"
model_id = "exec-model"
context_window = 1000
price_in_per_mtok = 1.0
price_out_per_mtok = 2.0

[roles.tester]
provider = "local"
model_id = "test-model"
context_window = 1000

[roles.reviewer]
provider = "other"
model_id = "review-model"
context_window = 1000
"#;

#[test]
fn roles_config_valid() {
    let c = RolesConfig::parse(ROLES).unwrap();
    assert_eq!(c.family_of("executor").as_deref(), Some("family-a"));
    assert_eq!(c.family_of("reviewer").as_deref(), Some("family-b"));
}

/// Spec §8.2(a)/§9.1: refuse to start if executor and reviewer resolve to
/// the same model family.
#[test]
fn same_family_review_refused() {
    let bad = ROLES.replace(
        "[roles.reviewer]\nprovider = \"other\"",
        "[roles.reviewer]\nprovider = \"backup\"",
    );
    assert!(matches!(
        RolesConfig::parse(&bad),
        Err(ConfigError::SameFamilyReview(f)) if f == "family-a"
    ));
    // Family comparison is case/whitespace-insensitive.
    let bad = ROLES.replace("family = \"family-b\"", "family = \" FAMILY-A \"");
    assert!(matches!(
        RolesConfig::parse(&bad),
        Err(ConfigError::SameFamilyReview(_))
    ));
    // A per-role override that collides is caught too.
    let bad = ROLES.replace(
        "model_id = \"review-model\"",
        "model_id = \"review-model\"\nfamily = \"family-a\"",
    );
    assert!(matches!(
        RolesConfig::parse(&bad),
        Err(ConfigError::SameFamilyReview(_))
    ));
}

/// A fallback provider in the reviewer's family would silently turn review
/// into same-family review; refused at config time.
#[test]
fn fallback_into_reviewer_family_refused() {
    let bad = ROLES.replace(
        "price_out_per_mtok = 2.0",
        "price_out_per_mtok = 2.0\nfallback = [\"other\"]",
    );
    assert!(matches!(
        RolesConfig::parse(&bad),
        Err(ConfigError::SameFamilyReview(_))
    ));
}

#[test]
fn config_rejects_missing_roles_and_unknown_keys() {
    let missing = ROLES.replace("[roles.tester]", "[roles.unused_tester_name]");
    assert!(RolesConfig::parse(&missing).is_err());
    let unknown = format!(
        "{ROLES}\n[providers.x]\nkind = \"mock\"\nfamily = \"f\"\napi_key = \"sk-literal\"\n"
    );
    assert!(
        RolesConfig::parse(&unknown).is_err(),
        "inline keys are not a config field"
    );
}

#[test]
fn router_is_deterministic_and_fast() {
    let c = RolesConfig::parse(ROLES).unwrap();
    let r = Router::new(&c);
    assert_eq!(r.route(TaskKind::Review).role, "reviewer");
    assert_eq!(r.route(TaskKind::Architecture).role, "planner");
    assert_eq!(r.route(TaskKind::Agentic).role, "executor");
    let first = r.route(TaskKind::Mechanical).clone();
    let n = 100_000;
    let t = Instant::now();
    for _ in 0..n {
        assert_eq!(r.route(TaskKind::Mechanical), &first);
    }
    let per = t.elapsed() / n;
    eprintln!("router: {per:?} per decision");
    assert!(per < Duration::from_micros(100), "routing took {per:?}");
}

#[test]
fn openai_response_parsing_rejects_hostile_tool_calls() {
    let body = serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": [
            {"id": "a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"src/x.rs\"}"}},
            {"id": "b", "type": "function", "function": {"name": "exec_shell", "arguments": "{\"cmd\":\"rm -rf /\"}"}},
            {"id": "c", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"x\",\"follow\":true}"}},
            {"id": "d", "type": "function", "function": {"name": "read_file", "arguments": "not json"}},
            {"id": "e", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\\u0000b\"}"}}
        ]}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    });
    let r = parse_openai_response(&body).unwrap();
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].call_id, "a");
    let bad: Vec<_> = r.invalid_calls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(bad, ["b", "c", "d", "e"]);
    assert_eq!(r.usage.unwrap().prompt_tokens, 10);
}

/// Spec §9.4: the prompt prefix hash is identical across turns.
#[test]
fn prompt_prefix_is_stable_across_turns() {
    let sys = ChatMessage {
        role: ChatRole::System,
        content: SYSTEM_PROMPT.into(),
        tool_calls: vec![],
        tool_call_id: None,
    };
    let mut msgs = vec![sys.clone()];
    let mut mon = PrefixMonitor::default();
    let h0 = prefix_hash(&msgs, &TOOLS_JSON);
    for i in 0..20 {
        mon.check(&msgs, &TOOLS_JSON).unwrap();
        assert_eq!(prefix_hash(&msgs, &TOOLS_JSON), h0);
        msgs.push(ChatMessage {
            role: if i % 2 == 0 {
                ChatRole::User
            } else {
                ChatRole::Assistant
            },
            content: format!("turn {i}"),
            tool_calls: vec![],
            tool_call_id: None,
        });
    }
    // Tool definitions are byte-identical however often they are serialised.
    assert_eq!(TOOLS_JSON.to_string(), TOOLS_JSON.to_string());
    // A changed system prompt is detected loudly.
    msgs[0].content.push('!');
    assert!(mon.check(&msgs, &TOOLS_JSON).is_err());
}

#[test]
fn compaction_keeps_head_tail_and_never_orphans_tool_replies() {
    let mk = |role, c: &str| ChatMessage {
        role,
        content: c.into(),
        tool_calls: vec![],
        tool_call_id: if role == ChatRole::Tool {
            Some("x".into())
        } else {
            None
        },
    };
    let mut m = vec![mk(ChatRole::System, "sys"), mk(ChatRole::User, "task")];
    for i in 0..20 {
        m.push(mk(ChatRole::Assistant, &format!("a{i}")));
        m.push(mk(ChatRole::Tool, &format!("t{i}")));
    }
    compact(&mut m, "cp-1");
    assert_eq!(m[0].content, "sys");
    assert_eq!(m[1].content, "task");
    assert!(m[2].content.contains("cp-1"));
    assert_ne!(m[3].role, ChatRole::Tool);
    assert_eq!(m.last().unwrap().content, "t19");
    assert!(m.len() <= 2 + 1 + 6);
}

// ---------------------------------------------------------------------------
// In-process agent loop against a fake backend
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeBackend {
    inbox: VecDeque<Message>,
    sent: Vec<Message>,
    resume_choice: ResumeChoice,
}

impl FakeBackend {
    fn new(task: Task, resume_choice: ResumeChoice) -> Self {
        FakeBackend {
            inbox: VecDeque::from([
                Message::Welcome(Welcome {
                    session_id: task.session_id.clone(),
                    protocol_version: PROTOCOL_VERSION,
                }),
                Message::Task(Box::new(task)),
            ]),
            sent: vec![],
            resume_choice,
        }
    }
}

impl Transport for FakeBackend {
    fn send(&mut self, msg: Message) -> Result<(), AgentError> {
        // Every message must survive the real wire encoding.
        let line =
            encode(&Frame::new(1, msg.clone())).map_err(|e| AgentError::Protocol(e.to_string()))?;
        decode(&line).map_err(|e| AgentError::Protocol(e.to_string()))?;
        match &msg {
            Message::ToolCall(c) => self.inbox.push_back(Message::ToolResult(ToolResult {
                call_id: c.call_id.clone(),
                outcome: ToolOutcome::Ok {
                    output: format!("result of {}", c.request.name()),
                },
            })),
            Message::Checkpoint(cp) => {
                self.inbox
                    .push_back(Message::CheckpointSaved(CheckpointSaved {
                        checkpoint_id: cp.checkpoint_id.clone(),
                    }))
            }
            Message::Paused(_) => self.inbox.push_back(Message::Resume(Resume {
                choice: self.resume_choice,
            })),
            Message::Done(_) | Message::Failed(_) => self.inbox.push_back(Message::Shutdown),
            _ => {}
        }
        self.sent.push(msg);
        Ok(())
    }
    fn recv(&mut self) -> Result<Message, AgentError> {
        self.inbox.pop_front().ok_or(AgentError::Closed)
    }
}

fn task(script: &str) -> Task {
    Task {
        session_id: "sess1".into(),
        instruction: "do the thing".into(),
        roles_toml: ROLES.into(),
        checkpoint_threshold: 0.8,
        resume_from: None,
        mock_script: Some(script.into()),
    }
}

const SCRIPT: &str = r#"{"steps":[
  {"content":"reading","tool_calls":[{"tool":"read_file","args":{"path":"a.txt"}}],"prompt_tokens":100,"completion_tokens":10},
  {"content":"writing","tool_calls":[{"tool":"write_file","args":{"path":"a.txt","content":"x"}},{"tool":"bogus_tool","args":{}}],"prompt_tokens":200,"completion_tokens":10},
  {"content":"all done","prompt_tokens":300,"completion_tokens":10}
]}"#;

#[test]
fn agent_loop_end_to_end_with_mock_provider() {
    let mut b = FakeBackend::new(task(SCRIPT), ResumeChoice::ContinueWithCompaction);
    run_session(&mut b, &*default_factory()).unwrap();
    let kinds: Vec<&str> = b
        .sent
        .iter()
        .map(|m| match m {
            Message::Hello(_) => "hello",
            Message::Notice(_) => "notice",
            Message::TurnStarted { .. } => "turn_started",
            Message::Usage(_) => "usage",
            Message::ToolCall(_) => "tool_call",
            Message::ContextUsage(_) => "context",
            Message::Checkpoint(_) => "checkpoint",
            Message::TurnFinished { .. } => "turn_finished",
            Message::Done(_) => "done",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "tool_call").count(),
        2,
        "{kinds:?}"
    );
    assert_eq!(kinds.iter().filter(|k| **k == "checkpoint").count(), 3);
    assert_eq!(*kinds.last().unwrap(), "done");
    // Cost accounting uses configured prices: (100*1 + 10*2) / 1e6.
    let Message::Usage(u) = b
        .sent
        .iter()
        .find(|m| matches!(m, Message::Usage(_)))
        .unwrap()
    else {
        unreachable!()
    };
    assert!((u.cost_usd - 120.0 / 1e6).abs() < 1e-12);
    // The bogus tool call never reached the backend.
    assert!(
        !b.sent
            .iter()
            .any(|m| matches!(m, Message::ToolCall(c) if c.request.name() == "bogus_tool"))
    );
}

/// Spec §5.4: a session that fills the context pauses at the threshold and
/// writes a checkpoint before doing anything else.
#[test]
fn agent_pauses_at_context_threshold() {
    let script = r#"{"steps":[
      {"content":"","tool_calls":[{"tool":"list_dir","args":{"path":"."}}],"prompt_tokens":850,"completion_tokens":10},
      {"content":"never reached"}
    ]}"#;
    let mut b = FakeBackend::new(task(script), ResumeChoice::FreshSession);
    run_session(&mut b, &*default_factory()).unwrap();
    let paused = b
        .sent
        .iter()
        .position(|m| matches!(m, Message::Paused(_)))
        .expect("paused");
    let Message::Checkpoint(cp) = &b.sent[paused - 1] else {
        panic!("pause must be preceded by a checkpoint")
    };
    assert_eq!(cp.reason, CheckpointReason::ContextThreshold);
    assert_eq!(cp.context_used_tokens, 860);
    let Message::Done(d) = b.sent.last().unwrap() else {
        panic!()
    };
    assert!(d.summary.contains(&cp.checkpoint_id));
    // No further model turns after the user chose a fresh session.
    assert_eq!(
        b.sent
            .iter()
            .filter(|m| matches!(m, Message::TurnStarted { .. }))
            .count(),
        1
    );
}

/// Spec §5.4: a fresh session seeded from a checkpoint resumes from it.
#[test]
fn fresh_session_seeded_from_checkpoint_resumes() {
    let mut first = FakeBackend::new(task(SCRIPT), ResumeChoice::ContinueWithCompaction);
    run_session(&mut first, &*default_factory()).unwrap();
    let cp = first
        .sent
        .iter()
        .filter_map(|m| match m {
            Message::Checkpoint(c) => Some(c.clone()),
            _ => None,
        })
        .nth(1)
        .unwrap();
    assert_eq!(cp.turn, 2);
    let mut t = task(r#"{"steps":[{"content":"finished after resume"}]}"#);
    t.resume_from = Some(cp.clone());
    let mut second = FakeBackend::new(t, ResumeChoice::ContinueWithCompaction);
    run_session(&mut second, &*default_factory()).unwrap();
    let Message::TurnStarted { turn } = second
        .sent
        .iter()
        .find(|m| matches!(m, Message::TurnStarted { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    assert_eq!(*turn, 3, "turn numbering continues from the checkpoint");
    let Message::Checkpoint(c2) = second
        .sent
        .iter()
        .find(|m| matches!(m, Message::Checkpoint(_)))
        .unwrap()
    else {
        unreachable!()
    };
    // History is carried over in full, plus the resume marker and new turn.
    assert!(c2.messages.len() > cp.messages.len());
    assert_eq!(&c2.messages[..cp.messages.len()], &cp.messages[..]);
}

#[test]
fn provider_fallback_is_announced() {
    let roles = ROLES.replace(
        "price_out_per_mtok = 2.0",
        "price_out_per_mtok = 2.0\nfallback = [\"backup\"]",
    );
    // Both mock providers replay the same script, so both fail. The fallback
    // attempt is announced and the final failure is reported, not hidden.
    let mut t = task(r#"{"steps":[{"fail":true},{"content":"unused"}]}"#);
    t.roles_toml = roles;
    let mut b = FakeBackend::new(t, ResumeChoice::ContinueWithCompaction);
    run_session(&mut b, &*default_factory()).unwrap();
    assert!(
        b.sent
            .iter()
            .any(|m| matches!(m, Message::Notice(n) if n.kind == NoticeKind::ProviderFallback))
    );
    assert!(
        matches!(b.sent.last(), Some(Message::Failed(f)) if f.error.contains("all providers failed"))
    );
}

#[test]
fn invalid_roles_config_fails_cleanly() {
    let mut t = task(SCRIPT);
    t.roles_toml = "not = [valid".into();
    let mut b = FakeBackend::new(t, ResumeChoice::ContinueWithCompaction);
    run_session(&mut b, &*default_factory()).unwrap();
    assert!(matches!(b.sent.last(), Some(Message::Failed(_))));
}
