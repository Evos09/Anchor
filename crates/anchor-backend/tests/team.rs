//! Phase 4 done-when tests: agents of different model families working on
//! one workspace through the Coordination Hub, in isolated workspaces,
//! integrated by the merge queue. Real `anchor-agent` runtime processes,
//! scripted models.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anchor_backend::interact::{Interaction, PromptView, Scripted};
use anchor_backend::iso;
use anchor_backend::mediator::lock_store;
use anchor_backend::queue::{MergeGate, MergeQueue, NoGate, Outcome};
use anchor_backend::session;
use anchor_backend::team::{self, Schedule, Team, TeamAgent, TeamOptions, TeamReport};
use anchor_backend::{Opened, open_workspace_in};
use anchor_hub::{
    Caller, ClaimType, MergeStatus, MessageQuery, MessageType, NewAgent, Permissions,
};
use anchor_policy::{AuditLog, Responder, UserResponse};
use anchor_protocol::ResumeChoice;
use serde_json::{Value, json};

/// Tests share the agent binary build and process-global counters.
static SERIAL: Mutex<()> = Mutex::new(());

const ROLES: &str = include_str!("../../../examples/demo/roles.mock.toml");

/// Roles whose agentic route (executor) is family A.
fn roles_a() -> String {
    // Normalized: Windows checkouts may have CRLF line endings.
    ROLES.replace("\r\n", "\n")
}

/// Roles whose agentic route is family B (the reviewer moves to A, so
/// review stays cross-family).
fn roles_b() -> String {
    let b = roles_a()
        .replace(
            "[roles.executor]\nprovider = \"mock_a\"",
            "[roles.executor]\nprovider = \"mock_b\"",
        )
        .replace(
            "[roles.reviewer]\nprovider = \"mock_b\"",
            "[roles.reviewer]\nprovider = \"mock_a\"",
        );
    assert_ne!(b, roles_a(), "roles_b must change the executor's provider");
    b
}

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
    p
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
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(ws.join("tests")).unwrap();
    std::fs::write(ws.join("README.md"), "# project\n").unwrap();
    std::fs::write(ws.join("src/a.txt"), "a1\n").unwrap();
    std::fs::write(ws.join("src/b.txt"), "b1\n").unwrap();
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

fn agent(name: &str, roles: String, within: &[&str], script: Value) -> TeamAgent {
    TeamAgent {
        name: name.into(),
        roles_toml: roles,
        instruction: format!("you are {name}"),
        claim_within: within.iter().map(|s| s.to_string()).collect(),
        mock_script: Some(script.to_string()),
        messages_need_approval: false,
    }
}

fn step(content: &str, calls: Value) -> Value {
    json!({ "content": content, "tool_calls": calls, "prompt_tokens": 100, "completion_tokens": 10 })
}

fn done(content: &str) -> Value {
    json!({ "content": content, "prompt_tokens": 100, "completion_tokens": 10 })
}

fn call(tool: &str, args: Value) -> Value {
    json!({ "tool": tool, "args": args })
}

fn run_team(fx: &Fx, opened: &mut Opened, team: &Team) -> TeamReport {
    let opts = TeamOptions {
        agent_bin: agent_bin(),
        state_base: fx.state_base(),
        auto_merge: true,
        checkpoint_threshold: 0.8,
        gate: Box::new(NoGate),
    };
    let interact = team::Shared::wrap(Box::new(Scripted::new(true, ResumeChoice::FreshSession)));
    team::run(opened, team, opts, interact).unwrap()
}

fn hub_of(opened: &Opened) -> Arc<anchor_hub::Hub> {
    let root = opened.mediator.workspace().root().to_path_buf();
    team::open_hub(&opened.paths, &root, opened.mediator.engine().audit_arc()).unwrap()
}

/// Every tool result the agent saw, from its last checkpoint.
fn tool_results(opened: &Opened, name: &str) -> Vec<String> {
    let store =
        anchor_checkpoint::CheckpointStore::open(&opened.paths.checkpoints.join(name)).unwrap();
    let (ok, _) = store.list().unwrap();
    let last = ok.iter().max_by_key(|c| c.turn).unwrap();
    let cp = store.load(&last.checkpoint_id).unwrap();
    cp.messages
        .iter()
        .filter(|m| m.role == anchor_protocol::ChatRole::Tool)
        .map(|m| m.content.clone())
        .collect()
}

#[test]
fn two_agents_of_different_families_collaborate() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    // Fresh Hub: alpha is agent 1, beta agent 2.
    let alpha = json!({ "steps": [
        step("Claim and write the library.", json!([
            call("hub_request_claim", json!({ "path": "src/lib.txt", "claim_type": "edit" })),
            call("write_file", json!({ "path": "src/lib.txt", "content": "fn add(a, b) = a + b\n" })),
        ])),
        step("Ask beta for tests.", json!([
            call("hub_send_message", json!({ "to_agent": 2, "message_type": "task",
                "content": { "text": "please test add() in src/lib.txt" } })),
        ])),
        done("Library written; asked beta for tests."),
    ]});
    let beta = json!({ "steps": [
        step("Read my inbox.", json!([ call("hub_query_messages", json!({})) ])),
        step("Write the tests.", json!([
            call("read_file", json!({ "path": "src/lib.txt" })),
            call("hub_request_claim", json!({ "path": "tests/", "claim_type": "edit" })),
            call("write_file", json!({ "path": "tests/lib_test.txt", "content": "assert add(2, 2) == 4\n" })),
        ])),
        step("Report back.", json!([
            call("hub_send_message", json!({ "to_agent": 1, "message_type": "review",
                "content": { "text": "tests added in tests/lib_test.txt" } })),
        ])),
        done("Tests written."),
    ]});
    let team = Team {
        schedule: Schedule::Sequential,
        agents: vec![
            agent("alpha", roles_a(), &["src/"], alpha),
            agent("beta", roles_b(), &["tests/"], beta),
        ],
    };
    let report = run_team(&fx, &mut opened, &team);

    // Two different model families, both merged.
    assert_eq!(report.agents.len(), 2);
    assert_eq!(report.agents[0].family, "demo-family-a");
    assert_eq!(report.agents[1].family, "demo-family-b");
    for r in &report.agents {
        assert!(r.session.as_ref().unwrap().done.is_some(), "{r:?}");
        assert!(r.merge.as_ref().unwrap().is_merged(), "{r:?}");
    }
    // The exact final tree.
    assert_eq!(fx.read("src/lib.txt"), "fn add(a, b) = a + b\n");
    assert_eq!(fx.read("tests/lib_test.txt"), "assert add(2, 2) == 4\n");
    assert_eq!(fx.read("src/a.txt"), "a1\n");
    assert_eq!(fx.read("README.md"), "# project\n");
    // Beta started from alpha's merged work and saw alpha's message.
    let seen = tool_results(&opened, "beta");
    assert!(
        seen.iter().any(|s| s.contains("please test add()")),
        "{seen:?}"
    );
    assert!(seen.iter().any(|s| s.contains("fn add(a, b)")), "{seen:?}");

    // Every message persisted with sender, recipient, type and time.
    let hub = hub_of(&opened);
    let msgs = hub
        .query_messages(Caller::User, MessageQuery::default())
        .unwrap();
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert_eq!(
        (
            msgs[0].from_agent_id,
            msgs[0].to_agent_id,
            msgs[0].message_type
        ),
        (Some(1), Some(2), MessageType::Task)
    );
    assert_eq!(
        (
            msgs[1].from_agent_id,
            msgs[1].to_agent_id,
            msgs[1].message_type
        ),
        (Some(2), Some(1), MessageType::Review)
    );
    assert!(msgs.iter().all(|m| m.created_at > 0));
    // ...and is auditable: the Hub matches the hash-chained log.
    let v = hub.verify().unwrap();
    assert!(v.ok(), "{:?}", v.errors);
    AuditLog::verify(&opened.paths.audit).unwrap();
    let entries = AuditLog::read_entries(&opened.paths.audit).unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.kind == "hub_send_message")
            .count(),
        2
    );
    assert_eq!(
        entries.iter().filter(|e| e.kind == "merge_applied").count(),
        2
    );
    // Agent decisions are tagged with the agent.
    assert!(
        entries
            .iter()
            .any(|e| e.kind == "decision" && e.data["agent"] == "beta")
    );
    // Isolated workspaces are gone once merged.
    let base = iso::iso_base(&fx.state_base(), &fx.ws);
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
}

#[test]
fn claims_prevent_conflicts() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let script = |text: &str| {
        json!({ "steps": [
            step("Claim and write the shared file.", json!([
                call("hub_request_claim", json!({ "path": "shared.txt", "claim_type": "edit" })),
                call("write_file", json!({ "path": "shared.txt", "content": text })),
            ])),
            done("done"),
        ]})
    };
    let team = Team {
        schedule: Schedule::Parallel,
        agents: vec![
            agent("alpha", roles_a(), &["."], script("from alpha\n")),
            agent("beta", roles_b(), &["."], script("from beta\n")),
        ],
    };
    let report = run_team(&fx, &mut opened, &team);
    let wrote: Vec<&str> = report
        .agents
        .iter()
        .filter(|r| r.session.as_ref().unwrap().stats.mutations == 1)
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(wrote.len(), 1, "exactly one agent may write: {report:?}");
    let winner = wrote[0];
    let loser = if winner == "alpha" { "beta" } else { "alpha" };
    // The loser's claim was refused with a conflict, and its write with
    // DeniedBy::Claim.
    let seen = tool_results(&opened, loser);
    assert!(seen[0].contains("conflict"), "{seen:?}");
    assert!(seen[1].starts_with("DENIED by Claim"), "{seen:?}");
    assert_eq!(fx.read("shared.txt"), format!("from {winner}\n"));
    let l = report.agents.iter().find(|r| r.name == loser).unwrap();
    assert!(l.session.as_ref().unwrap().stats.denied >= 1);
}

#[test]
fn concurrent_isolated_agents_merge_adjacent_files() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let script = |path: &str, text: &str| {
        json!({ "steps": [
            step("edit", json!([
                call("read_file", json!({ "path": path })),
                call("write_file", json!({ "path": path, "content": text })),
            ])),
            done("done"),
        ]})
    };
    let team = Team {
        schedule: Schedule::Parallel,
        agents: vec![
            agent(
                "alpha",
                roles_a(),
                &["src/a.txt"],
                script("src/a.txt", "a2\n"),
            ),
            agent(
                "beta",
                roles_b(),
                &["src/b.txt"],
                script("src/b.txt", "b2\n"),
            ),
        ],
    };
    let report = run_team(&fx, &mut opened, &team);
    for r in &report.agents {
        assert!(r.merge.as_ref().unwrap().is_merged(), "{r:?}");
    }
    assert_eq!(fx.read("src/a.txt"), "a2\n");
    assert_eq!(fx.read("src/b.txt"), "b2\n");
    assert_eq!(fx.read("README.md"), "# project\n");
    {
        let store = opened.mediator.snapshots();
        let v = anchor_snapshot::verify(&store).unwrap();
        assert!(v.errors.is_empty(), "{:?}", v.errors);
    }

    // Revert alpha's merge (found by agent, not by sequence number): only
    // alpha's file goes back.
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(hub, &mut opened.mediator, Box::new(NoGate), true);
    let seq = q.latest_merge_of(1).unwrap().unwrap();
    q.revert(seq).unwrap();
    assert_eq!(fx.read("src/a.txt"), "a1\n");
    assert_eq!(fx.read("src/b.txt"), "b2\n");
}

/// Register an agent and give it an isolated workspace whose files the
/// test then changes directly (as a command run inside it would).
fn iso_agent(q: &mut MergeQueue<'_>, fx: &Fx, name: &str, within: &[&str]) -> (i64, iso::Isolated) {
    let (g, _) = q
        .hub()
        .register_agent(
            Caller::User,
            NewAgent {
                display_name: name.into(),
                model_id: "m".into(),
                role: "executor".into(),
                family: "f".into(),
                capabilities: json!({}),
                permissions: Permissions::collaborator(within),
            },
        )
        .unwrap();
    let main = q.main();
    let store_arc = main.shared_store();
    let store = lock_store(&store_arc);
    let base = anchor_snapshot::snapshot(&store, main.workspace(), main.snapshot_options(), "b")
        .unwrap()
        .id();
    let i = iso::create(
        &store,
        main.snapshot_options(),
        main.engine(),
        &iso::iso_base(&fx.state_base(), &fx.ws),
        iso::root_for(g.id),
        &base,
    )
    .unwrap();
    (g.id, i)
}

fn submit(q: &mut MergeQueue<'_>, agent: i64, i: &iso::Isolated) -> Outcome {
    let sub = {
        let store_arc = q.main().shared_store();
        let store = lock_store(&store_arc);
        iso::submit(&store, i).unwrap()
    };
    let e = q.submit(agent, &i.base, &sub).unwrap();
    q.process(e.seq).unwrap()
}

#[test]
fn unclaimed_changes_are_rejected_at_merge() {
    // Threat 8: a change made without the Hub (e.g. by a command in the
    // agent's own workspace) never reaches the main workspace.
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(NoGate), true);
    let (id, i) = iso_agent(&mut q, &fx, "rogue", &["src/"]);
    hub.request_claim(Caller::Agent(id), "src/a.txt", ClaimType::Edit, false, None)
        .unwrap();
    std::fs::write(i.ws.root().join("src/a.txt"), "claimed edit\n").unwrap();
    std::fs::write(i.ws.root().join("src/b.txt"), "unclaimed edit\n").unwrap();
    std::fs::write(i.ws.root().join("README.md"), "outside its grant\n").unwrap();
    let o = submit(&mut q, id, &i);
    let Outcome::Rejected { feedback, .. } = &o else {
        panic!("{o:?}")
    };
    assert_eq!(feedback["reason"], "claims");
    let paths: Vec<&str> = feedback["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["README.md", "src/b.txt"]);
    drop(q);
    assert_eq!(fx.read("src/a.txt"), "a1\n");
    assert_eq!(fx.read("src/b.txt"), "b1\n");
    assert_eq!(fx.read("README.md"), "# project\n");
    // The agent was told why, through the bus.
    let inbox = hub
        .query_messages(Caller::Agent(id), MessageQuery::default())
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message_type, MessageType::Review);
    assert_eq!(inbox[0].content["feedback"]["reason"], "claims");
}

/// A new directory is checked file by file: claiming one file in it
/// entitles exactly that file.
#[test]
fn new_directories_are_checked_file_by_file() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(NoGate), true);
    let (id, i) = iso_agent(&mut q, &fx, "alpha", &["src/"]);
    hub.request_claim(
        Caller::Agent(id),
        "src/new/a.txt",
        ClaimType::Edit,
        false,
        None,
    )
    .unwrap();
    std::fs::create_dir_all(i.ws.root().join("src/new/deep")).unwrap();
    std::fs::write(i.ws.root().join("src/new/a.txt"), "a\n").unwrap();
    std::fs::write(i.ws.root().join("src/new/deep/b.txt"), "b\n").unwrap();
    let o = submit(&mut q, id, &i);
    let Outcome::Rejected { feedback, .. } = &o else {
        panic!("{o:?}")
    };
    assert_eq!(feedback["paths"][0]["path"], "src/new/deep/b.txt");
    assert_eq!(feedback["paths"].as_array().unwrap().len(), 1);

    // With both files claimed, the same tree merges.
    hub.retire_agent(Caller::User, id).unwrap();
    let (id2, i2) = iso_agent(&mut q, &fx, "beta", &["src/"]);
    hub.request_claim(Caller::Agent(id2), "src/new/", ClaimType::Edit, false, None)
        .unwrap();
    std::fs::create_dir_all(i2.ws.root().join("src/new/deep")).unwrap();
    std::fs::write(i2.ws.root().join("src/new/a.txt"), "a\n").unwrap();
    std::fs::write(i2.ws.root().join("src/new/deep/b.txt"), "b\n").unwrap();
    assert!(submit(&mut q, id2, &i2).is_merged());
    drop(q);
    assert_eq!(fx.read("src/new/deep/b.txt"), "b\n");
}

#[test]
fn merge_conflict_returns_structured_feedback() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(NoGate), true);
    let (a, ia) = iso_agent(&mut q, &fx, "alpha", &["src/"]);
    let (b, ib) = iso_agent(&mut q, &fx, "beta", &["src/"]);
    // Alpha edits a.txt under its claim and merges, then releases.
    let ca = hub
        .request_claim(Caller::Agent(a), "src/a.txt", ClaimType::Edit, false, None)
        .unwrap();
    std::fs::write(ia.ws.root().join("src/a.txt"), "alpha\n").unwrap();
    assert!(submit(&mut q, a, &ia).is_merged());
    hub.release_claim(Caller::Agent(a), ca.id).unwrap();
    // Beta, from the same base, edits the same file.
    hub.request_claim(Caller::Agent(b), "src/a.txt", ClaimType::Edit, false, None)
        .unwrap();
    std::fs::write(ib.ws.root().join("src/a.txt"), "beta\n").unwrap();
    let o = submit(&mut q, b, &ib);
    let Outcome::Rejected { seq, feedback } = &o else {
        panic!("{o:?}")
    };
    assert_eq!(feedback["reason"], "conflict");
    assert_eq!(feedback["paths"], json!(["src/a.txt"]));
    assert_eq!(feedback["base"], json!(ib.base.hex()));
    assert!(feedback["main"].is_string());
    let row = hub
        .merges()
        .unwrap()
        .into_iter()
        .find(|m| m.seq == *seq)
        .unwrap();
    assert_eq!(row.status, MergeStatus::Rejected);
    drop(q);
    assert_eq!(fx.read("src/a.txt"), "alpha\n");
}

struct FailingGate;
impl MergeGate for FailingGate {
    fn check(
        &mut self,
        _: &anchor_snapshot::Store,
        _: &anchor_snapshot::Id,
        changes: &[anchor_snapshot::Change],
    ) -> Result<(), Value> {
        Err(json!({ "tests": "1 failed", "changed": changes.len() }))
    }
}

#[test]
fn merge_gate_failure_rejects() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = fx.open();
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(
        hub.clone(),
        &mut opened.mediator,
        Box::new(FailingGate),
        true,
    );
    let (a, ia) = iso_agent(&mut q, &fx, "alpha", &["src/"]);
    hub.request_claim(Caller::Agent(a), "src/", ClaimType::Edit, false, None)
        .unwrap();
    std::fs::write(ia.ws.root().join("src/a.txt"), "alpha\n").unwrap();
    let o = submit(&mut q, a, &ia);
    let Outcome::Rejected { feedback, .. } = &o else {
        panic!("{o:?}")
    };
    assert_eq!(feedback["reason"], "gate");
    assert_eq!(feedback["detail"]["tests"], "1 failed");
    drop(q);
    assert_eq!(fx.read("src/a.txt"), "a1\n");
}

/// Approves merges, but edits the main workspace during the first
/// approval prompt (between the merge's computation and its application).
struct MeddlingUser {
    file: PathBuf,
    prompts: usize,
}

impl Interaction for MeddlingUser {
    fn confirm(&mut self, _: &PromptView) -> UserResponse {
        self.prompts += 1;
        if self.prompts == 1 {
            std::fs::write(&self.file, "edited by the user meanwhile\n").unwrap();
        }
        UserResponse::Approve
    }
    fn resume_choice(&mut self, _: &str, _: &str) -> ResumeChoice {
        ResumeChoice::FreshSession
    }
    fn notify(&mut self, _: &str) {}
    fn responder(&self) -> Responder {
        Responder::Automatic
    }
}

#[test]
fn merge_never_reverts_concurrent_main_edits() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let mut opened = open_workspace_in(
        &fx.ws,
        &fx.state_base(),
        None,
        Box::new(MeddlingUser {
            file: fx.ws.join("README.md"),
            prompts: 0,
        }),
        vec![],
    )
    .unwrap();
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(NoGate), false);
    let (a, ia) = iso_agent(&mut q, &fx, "alpha", &["src/"]);
    hub.request_claim(Caller::Agent(a), "src/", ClaimType::Edit, false, None)
        .unwrap();
    std::fs::write(ia.ws.root().join("src/a.txt"), "alpha\n").unwrap();
    let o = submit(&mut q, a, &ia);
    assert!(o.is_merged(), "{o:?}");
    drop(q);
    // Both the agent's change and the user's concurrent edit survive.
    assert_eq!(fx.read("src/a.txt"), "alpha\n");
    assert_eq!(fx.read("README.md"), "edited by the user meanwhile\n");
    let entries = AuditLog::read_entries(&opened.paths.audit).unwrap();
    assert_eq!(
        entries.iter().filter(|e| e.kind == "merge_retry").count(),
        1
    );
}

#[test]
fn messages_persist_across_restart() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    {
        let mut opened = fx.open();
        let script = json!({ "steps": [
            step("post", json!([
                call("hub_send_message", json!({ "room": "general", "message_type": "status",
                    "content": { "text": "alpha started" } })),
            ])),
            done("done"),
        ]});
        let team = Team {
            schedule: Schedule::Sequential,
            agents: vec![agent("alpha", roles_a(), &["src/"], script)],
        };
        let r = run_team(&fx, &mut opened, &team);
        assert!(r.agents[0].session.as_ref().unwrap().done.is_some());
    }
    // A new process: reopen everything from disk.
    let opened = fx.open();
    let hub = hub_of(&opened);
    let msgs = hub
        .query_messages(
            Caller::User,
            MessageQuery {
                room: Some("general".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content["text"], "alpha started");
    assert_eq!(msgs[0].from_agent_id, Some(1));
    assert!(hub.verify().unwrap().ok());
}

/// Run `anchor team run` as a child process that aborts part-way through
/// applying its merge; reopening recovers the main workspace and the queue
/// finishes the merge.
#[test]
fn kill_mid_merge_recovers() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    let script = json!({ "steps": [
        step("edit", json!([
            call("write_file", json!({ "path": "src/a.txt", "content": "a2\n" })),
            call("write_file", json!({ "path": "src/b.txt", "content": "b2\n" })),
            call("write_file", json!({ "path": "src/c.txt", "content": "c2\n" })),
        ])),
        done("done"),
    ]});
    let dir = fx.base.join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("roles.toml"), roles_a()).unwrap();
    std::fs::write(dir.join("alpha.json"), script.to_string()).unwrap();
    std::fs::write(
        dir.join("team.toml"),
        "[[agent]]\nname = \"alpha\"\nroles = \"roles.toml\"\ninstruction = \"edit\"\n\
         claim_within = [\"src/\"]\nmock_script = \"alpha.json\"\n",
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
        .env("ANCHOR_TEST_FAULT", "merge:2")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the child must die mid-merge");
    // Part-way: some of the merge's files are in place.
    let partial = ["src/a.txt", "src/b.txt", "src/c.txt"]
        .iter()
        .filter(|f| fx.read(f).ends_with("2\n"))
        .count();
    assert!(partial > 0 && partial < 3, "partial = {partial}");

    // Reopening undoes the half-applied merge first...
    let mut opened = fx.open();
    assert!(opened.recovered.is_some());
    assert_eq!(fx.read("src/a.txt"), "a1\n");
    assert_eq!(fx.read("src/c.txt"), "<absent>");
    // ...then the queue re-queues it and finishes it.
    let hub = hub_of(&opened);
    let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(NoGate), true);
    let queued = q.recover().unwrap();
    assert_eq!(queued.len(), 1);
    // The agent's claims are long gone (its run ended); the claim history
    // still entitles the change.
    let o = q.process(queued[0]).unwrap();
    assert!(o.is_merged(), "{o:?}");
    drop(q);
    assert_eq!(fx.read("src/a.txt"), "a2\n");
    assert_eq!(fx.read("src/b.txt"), "b2\n");
    assert_eq!(fx.read("src/c.txt"), "c2\n");
    assert!(hub.verify().unwrap().ok());
    AuditLog::verify(&opened.paths.audit).unwrap();
}
