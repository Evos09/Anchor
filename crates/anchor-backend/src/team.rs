//! Team runs (spec §6, §7): several agents, possibly of different model
//! families, work on one workspace through the Coordination Hub. Each agent
//! runs in its own runtime process, behind its own mediator, in its own
//! isolated copy of the workspace; the merge queue integrates their work.
//!
//! `team.toml`:
//!
//! ```toml
//! schedule = "sequential"          # or "parallel"
//!
//! [[agent]]
//! name = "alpha"                   # [a-z0-9_-], unique
//! roles = "roles-a.toml"           # relative to team.toml
//! instruction = "Write src/lib.txt, then ask beta for tests."
//! claim_within = ["src/"]          # what the agent may claim (and change)
//! # mock_script = "alpha.json"     # scripted model (tests, demos)
//! # messages_need_approval = true  # hold its messages for the user
//! ```
//!
//! An agent's model and family come from its roles file (the agentic
//! route), never from the team file.
//!
//! Merges are verified when the file has a `[verify]` section (spec §8,
//! [`crate::verify`]):
//!
//! ```toml
//! [verify]
//! build = ["python3", "-I", "-m", "compileall", "-q", "."]
//! test = ["python3", "-I", "-m", "unittest", "discover", "-v"]
//! coverage = ["python3", "-I", "-m", "trace", "--count", "--missing",
//!             "--coverdir", "{coverdir}", "--ignore-dir", "$prefix",
//!             "--ignore-dir", "$exec_prefix", "--module", "unittest", "discover"]
//! protected = ["tests/**"]         # existing files here may not change
//! max_attempts = 3                 # submissions per agent (self-repair)
//! reviewer = "rule"                # or "model" (with reviewer_roles)
//! # reviewer_roles = "reviewer.toml"
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anchor_checkpoint::CheckpointStore;
use anchor_guard::Guard;
use anchor_hub::{AgentId, Caller, Hub, HubConfig, NewAgent, Permissions, SystemClock};
use anchor_policy::{AuditLog, Responder, UserResponse};
use anchor_protocol::roles::{RolesView, Route};
use anchor_protocol::{HubInfo, HubPeer, HubToolDef, ResumeChoice};
use serde::Deserialize;

use crate::interact::{Interaction, PromptView};
use crate::mediator::{HubBinding, Mediator, lock_store};
use crate::queue::{MergeGate, MergeQueue, Outcome, RejectKind};
use crate::session::{self, SessionConfig, SessionReport};
use crate::verify::{ModelReviewer, ReviewerKind, VerifyFile, VerifyGate, VerifySpec};
use crate::{Opened, StatePaths, iso};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// One agent at a time, in file order, each starting from the main
    /// workspace as the previous one left it (deterministic).
    #[default]
    Sequential,
    /// Every agent at once from the same base; merges in completion order.
    Parallel,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamFile {
    #[serde(default)]
    schedule: Schedule,
    #[serde(rename = "agent", default)]
    agents: Vec<AgentSpec>,
    /// The verification pipeline (spec §8, ADR-0024).
    #[serde(default)]
    verify: Option<VerifyFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSpec {
    name: String,
    roles: PathBuf,
    instruction: String,
    #[serde(default)]
    claim_within: Vec<String>,
    #[serde(default)]
    mock_script: Option<PathBuf>,
    /// Scripted models for repair attempts 2, 3, … (the last repeats).
    #[serde(default)]
    repair_mock_scripts: Vec<PathBuf>,
    #[serde(default)]
    messages_need_approval: bool,
}

#[derive(Debug, Clone)]
pub struct TeamAgent {
    pub name: String,
    pub roles_toml: String,
    pub instruction: String,
    pub claim_within: Vec<String>,
    pub mock_script: Option<String>,
    /// Scripted models for repair attempts 2, 3, … (the last repeats).
    pub repair_mock_scripts: Vec<String>,
    pub messages_need_approval: bool,
    /// Secrets for this agent's runtime (the app's keys). Never logged.
    pub extra_env: crate::inference::keys::SecretEnv,
}

#[derive(Debug, Clone)]
pub struct Team {
    pub schedule: Schedule,
    pub agents: Vec<TeamAgent>,
    /// `None`: merges are not verified (the CLI then requires
    /// `--unverified`).
    pub verify: Option<VerifySpec>,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && !n.starts_with('-')
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl Team {
    /// Load `team.toml`; `roles` and `mock_script` paths are relative to it.
    pub fn load(path: &Path) -> Result<Team, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file: TeamFile =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let read = |p: &Path| {
            let p = dir.join(p);
            std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        let mut agents = Vec::new();
        for a in file.agents {
            agents.push(TeamAgent {
                roles_toml: read(&a.roles)?,
                mock_script: a.mock_script.as_deref().map(read).transpose()?,
                repair_mock_scripts: a
                    .repair_mock_scripts
                    .iter()
                    .map(|p| read(p))
                    .collect::<Result<_, _>>()?,
                name: a.name,
                instruction: a.instruction,
                claim_within: a.claim_within,
                messages_need_approval: a.messages_need_approval,
                extra_env: Default::default(),
            });
        }
        let team = Team {
            schedule: file.schedule,
            agents,
            verify: file.verify.map(|v| VerifySpec::load(v, dir)).transpose()?,
        };
        team.validate()?;
        Ok(team)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.agents.is_empty() {
            return Err("a team needs at least one [[agent]]".into());
        }
        if self.agents.len() > 16 {
            return Err("at most 16 agents per team".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for a in &self.agents {
            if !valid_name(&a.name) {
                return Err(format!(
                    "agent name {:?}: use 1-32 of [a-z0-9_-], not starting with -",
                    a.name
                ));
            }
            if !seen.insert(&a.name) {
                return Err(format!("agent name {:?} is used twice", a.name));
            }
            if a.claim_within.is_empty() {
                return Err(format!(
                    "agent {:?}: claim_within is empty, so it could change nothing",
                    a.name
                ));
            }
            for w in &a.claim_within {
                anchor_hub::ClaimPath::parse(w)
                    .map_err(|e| format!("agent {:?}: claim_within {w:?}: {e}", a.name))?;
            }
            roles_identity(&a.roles_toml).map_err(|e| format!("agent {:?}: {e}", a.name))?;
        }
        if let Some(v) = &self.verify {
            v.validate()?;
            v.check_families(&self.executor_families()?)?;
        }
        Ok(())
    }

    /// Every family any agent's executor can run on (primary or fallback).
    pub fn executor_families(&self) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        for a in &self.agents {
            for f in roles_view(&a.roles_toml)?.families(Route::Agentic) {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        Ok(out)
    }
}

/// `(role, model_id, family)` of the agentic route of a roles file.
pub fn roles_identity(roles_toml: &str) -> Result<(String, String, String), String> {
    let id = roles_view(roles_toml)?.identity(Route::Agentic)?;
    Ok((id.role, id.model_id, id.family))
}

/// The family-resolution view of a roles file (shared with the runtime).
pub fn roles_view(roles_toml: &str) -> Result<RolesView, String> {
    toml::from_str(roles_toml).map_err(|e| format!("roles: {e}"))
}

/// Open the workspace's Hub (`<state>/hub.sqlite`), sharing the audit log.
pub fn open_hub(
    paths: &StatePaths,
    ws_root: &Path,
    audit: Arc<AuditLog>,
) -> Result<Arc<Hub>, String> {
    Hub::open(
        &paths.dir.join("hub.sqlite"),
        ws_root,
        audit,
        Arc::new(SystemClock),
        HubConfig::default(),
    )
    .map(Arc::new)
    .map_err(|e| format!("hub: {e}"))
}

/// The Hub tools as offered to a runtime.
pub fn hub_tool_defs() -> Vec<HubToolDef> {
    anchor_hub::tools::definitions()
        .into_iter()
        .map(|t| HubToolDef {
            name: t.name.into(),
            description: t.description.into(),
            input_schema: t.input_schema,
        })
        .collect()
}

/// One interaction shared by every agent of a run: prompts are serialized
/// and prefixed with the agent's name.
pub struct Shared {
    name: String,
    inner: Arc<Mutex<Box<dyn Interaction>>>,
    /// Set when the shared interaction can serve agents independently.
    own: Option<Box<dyn Interaction>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").field("name", &self.name).finish()
    }
}

impl Shared {
    pub fn wrap(inner: Box<dyn Interaction>) -> Arc<Mutex<Box<dyn Interaction>>> {
        Arc::new(Mutex::new(inner))
    }
    pub fn new(name: &str, inner: &Arc<Mutex<Box<dyn Interaction>>>) -> Self {
        let own = inner.lock().unwrap_or_else(|p| p.into_inner()).split(name);
        Shared {
            name: name.to_string(),
            inner: inner.clone(),
            own,
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn Interaction>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Interaction for Shared {
    fn confirm(&mut self, view: &PromptView) -> UserResponse {
        if let Some(own) = &mut self.own {
            return own.confirm(view);
        }
        let mut v = view.clone();
        v.rule = format!("[{}] {}", self.name, v.rule);
        self.lock().confirm(&v)
    }
    fn resume_choice(&mut self, checkpoint_id: &str, ratio_hint: &str) -> ResumeChoice {
        if let Some(own) = &mut self.own {
            return own.resume_choice(checkpoint_id, ratio_hint);
        }
        self.lock().resume_choice(checkpoint_id, ratio_hint)
    }
    fn notify(&mut self, line: &str) {
        let line = format!("[{}] {line}", self.name);
        match &mut self.own {
            Some(own) => own.notify(&line),
            None => self.lock().notify(&line),
        }
    }
    fn responder(&self) -> Responder {
        match &self.own {
            Some(own) => own.responder(),
            None => self.lock().responder(),
        }
    }
}

pub struct TeamOptions {
    pub agent_bin: PathBuf,
    pub state_base: PathBuf,
    pub auto_merge: bool,
    pub checkpoint_threshold: f64,
    pub gate: Box<dyn MergeGate>,
    /// Stops every session of the run (the app's Stop button); agents not
    /// yet started are skipped.
    pub cancel: Option<Arc<session::Cancel>>,
}

impl std::fmt::Debug for TeamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamOptions")
            .field("agent_bin", &self.agent_bin)
            .field("auto_merge", &self.auto_merge)
            .finish()
    }
}

#[derive(Debug)]
pub struct AgentResult {
    pub name: String,
    pub agent_id: AgentId,
    pub model_id: String,
    pub family: String,
    /// The last session (of the last attempt).
    pub session: Result<SessionReport, String>,
    /// Sessions of earlier, rejected attempts.
    pub earlier_sessions: Vec<Result<SessionReport, String>>,
    /// The final merge outcome (the last of `attempts`).
    pub merge: Option<Outcome>,
    /// Every submission's outcome, in order (at most `max_attempts`).
    pub attempts: Vec<Outcome>,
}

/// What a repair session is told about the rejected attempt.
fn repair_note(reason: RejectKind, feedback: &serde_json::Value) -> String {
    match reason {
        RejectKind::Gate => format!(
            "{}\nThe workspace now holds your rejected change merged with the latest \
             main workspace. Fix the problems above, then finish with new claims.",
            crate::verify::render_gate_feedback(feedback)
        ),
        _ => format!(
            "Your previous change conflicted with changes made meanwhile to: {}. \
             The workspace now holds the latest version of those files with the rest \
             of your change applied. Redo your edits to them on top of the current \
             content, then finish with new claims.",
            feedback["paths"]
                .as_array()
                .map(|ps| ps
                    .iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
    }
}

#[derive(Debug, Default)]
pub struct TeamReport {
    /// Merges left queued by an earlier, interrupted run, finished first.
    pub recovered: Vec<Outcome>,
    pub agents: Vec<AgentResult>,
}

struct Prepared {
    idx: usize,
    mediator: Mediator,
    iso: iso::Isolated,
    checkpoints: CheckpointStore,
    cfg: SessionConfig,
}

fn run_one(
    p: &mut Prepared,
    cancel: Option<&session::Cancel>,
) -> Result<SessionReport, session::SessionError> {
    session::run_cancellable(&p.cfg, &mut p.mediator, &p.checkpoints, cancel)
}

fn finished_ok(r: &Result<SessionReport, String>) -> bool {
    matches!(r, Ok(rep) if rep.done.is_some() && rep.failed.is_none())
}

/// The verification gate of a team run.
fn verify_gate(
    opened: &Opened,
    hub: &Arc<Hub>,
    spec: &VerifySpec,
    team: &Team,
    opts: &TeamOptions,
) -> Result<VerifyGate, String> {
    let main = &opened.mediator;
    let ws_root = main.workspace().root().to_path_buf();
    let scratch = crate::verify::scratch_base(&opts.state_base, &ws_root);
    let reviewer: crate::verify::BoxedReviewer = match spec.reviewer {
        ReviewerKind::Rule => crate::verify::rule_reviewer(),
        ReviewerKind::Model => {
            let (mediator, checkpoints) =
                crate::verify::reviewer_mediator(main, &opts.state_base, &opened.paths)?;
            Box::new(ModelReviewer::new(
                mediator,
                checkpoints,
                opts.agent_bin.clone(),
                spec.reviewer_roles.clone().unwrap_or_default(),
                spec.reviewer_mock_script.clone(),
                team.executor_families()?,
            )?)
        }
    };
    // The tester's commands are the user's own (not an agent's): they get
    // the system's read-only view, plus whatever agent commands get.
    let mut read_only = crate::default_exec_read_only();
    for d in main.exec_read_only() {
        if !read_only.contains(d) {
            read_only.push(d.clone());
        }
    }
    let reviewer = crate::verifier::with_plugins(
        reviewer,
        &spec.verifiers,
        &ws_root,
        main.path_var(),
        crate::default_exec_read_only(),
    )?;
    VerifyGate::new(
        spec.clone(),
        scratch,
        Some(hub.clone()),
        reviewer,
        main.path_var().to_string(),
        read_only,
    )
}

/// Run `team` on the opened workspace. `interact` answers every prompt of
/// every agent (and the merge approvals), one at a time.
pub fn run(
    opened: &mut Opened,
    team: &Team,
    mut opts: TeamOptions,
    interact: Arc<Mutex<Box<dyn Interaction>>>,
) -> Result<TeamReport, String> {
    team.validate()?;
    let ws_root = opened.mediator.workspace().root().to_path_buf();
    let hub = open_hub(
        &opened.paths,
        &ws_root,
        opened.mediator.engine().audit_arc(),
    )?;
    let iso_base = iso::iso_base(&opts.state_base, &ws_root);
    crate::create_private_dir(&iso_base).map_err(|e| format!("{}: {e}", iso_base.display()))?;

    // Register every agent first, so each knows its peers.
    let mut ids = Vec::new();
    let mut idents = Vec::new();
    for a in &team.agents {
        let (role, model_id, family) = roles_identity(&a.roles_toml)?;
        let within: Vec<&str> = a.claim_within.iter().map(String::as_str).collect();
        let mut permissions = Permissions::collaborator(&within);
        permissions.messages_need_approval = a.messages_need_approval;
        let (agent, _token) = hub
            .register_agent(
                Caller::User,
                NewAgent {
                    display_name: a.name.clone(),
                    model_id: model_id.clone(),
                    role: role.clone(),
                    family: family.clone(),
                    capabilities: serde_json::json!({ "runtime": "anchor-agent" }),
                    permissions,
                },
            )
            .map_err(|e| format!("registering {}: {e}", a.name))?;
        ids.push(agent.id);
        idents.push((role, model_id, family));
    }
    // The prices each agent's roles file sets: the dashboard recomputes
    // every reported cost from them.
    let prices = |roles: &str| roles_view(roles).map(|v| v.prices()).unwrap_or_default();
    let reviewer = team.verify.as_ref().map(|v| {
        let id = v
            .reviewer_roles
            .as_deref()
            .and_then(|r| roles_view(r).ok()?.identity(Route::Review).ok());
        serde_json::json!({
            "kind": match v.reviewer { ReviewerKind::Rule => "rule", ReviewerKind::Model => "model" },
            "prices": v.reviewer_roles.as_deref().map(prices),
            "role": id.as_ref().map(|i| i.role.clone()),
            "model_id": id.as_ref().map(|i| i.model_id.clone()),
            "family": id.as_ref().map(|i| i.family.clone()),
        })
    });
    let _ = opened.mediator.engine().log(
        "team_started",
        serde_json::json!({
            "schedule": team.schedule,
            "agents": team.agents.iter().zip(&ids).zip(&idents).map(|((a, id), (role, model, family))| {
                serde_json::json!({
                    "name": a.name, "agent_id": id, "role": role, "model_id": model,
                    "family": family, "claim_within": a.claim_within,
                    "instruction": a.instruction, "prices": prices(&a.roles_toml),
                })
            }).collect::<Vec<_>>(),
            "verify": reviewer,
        }),
    );
    let tools = hub_tool_defs();
    let hub_info = |i: usize| HubInfo {
        agent_id: ids[i],
        display_name: team.agents[i].name.clone(),
        peers: (0..team.agents.len())
            .filter(|&j| j != i)
            .map(|j| HubPeer {
                agent_id: ids[j],
                display_name: team.agents[j].name.clone(),
                role: idents[j].0.clone(),
                family: idents[j].2.clone(),
            })
            .collect(),
        tools: tools.clone(),
    };

    let gate: Box<dyn MergeGate> = match &team.verify {
        Some(spec) => Box::new(verify_gate(opened, &hub, spec, team, &opts)?),
        None => std::mem::replace(&mut opts.gate, Box::new(crate::queue::NoGate)),
    };
    let paths = opened.paths.clone();
    let cancel = opts.cancel.as_deref();
    let mut queue = MergeQueue::new(hub.clone(), &mut opened.mediator, gate, opts.auto_merge);
    let mut report = TeamReport::default();
    for seq in queue.recover()? {
        report.recovered.push(queue.process(seq)?);
    }

    // Isolated workspace, mediator and session config for agent `i`'s
    // `attempt` (1-based): filled from `from`, with its changes counted
    // from `base`.
    let prepare = |queue: &mut MergeQueue<'_>,
                   i: usize,
                   attempt: usize,
                   from: &anchor_snapshot::Id,
                   base: &anchor_snapshot::Id,
                   instruction: String| {
        let a = &team.agents[i];
        let main = queue.main();
        let store_arc = main.shared_store();
        let mut iso = {
            let store = lock_store(&store_arc);
            let root = iso::root_for(ids[i]);
            if attempt > 1 {
                // The previous attempt's directory is settled: start over.
                iso::teardown(&store, &root, &iso_base.join(root.name().unwrap_or("?")))?;
            }
            iso::create(
                &store,
                main.snapshot_options(),
                main.engine(),
                &iso_base,
                root,
                from,
            )?
        };
        iso.base = *base;
        let ws = anchor_fs::Workspace::open(iso.ws.root()).map_err(|e| e.to_string())?;
        let guard = Guard::load(&paths.workspace_config).map_err(|e| e.to_string())?;
        let mut mediator = main.isolated(
            ws,
            iso.opts.clone(),
            main.engine().for_agent(&a.name),
            guard,
            Box::new(Shared::new(&a.name, &interact)),
        );
        mediator.bind_hub(HubBinding {
            hub: hub.clone(),
            agent: ids[i],
        });
        let checkpoints =
            CheckpointStore::open(&paths.checkpoints.join(&a.name)).map_err(|e| e.to_string())?;
        let mock_script = match attempt {
            1 => a.mock_script.clone(),
            n => a
                .repair_mock_scripts
                .get(n - 2)
                .or(a.repair_mock_scripts.last())
                .cloned()
                .or_else(|| a.mock_script.clone()),
        };
        let cfg = SessionConfig {
            agent_bin: opts.agent_bin.clone(),
            session_id: format!("{}-{}-{attempt}", session::new_session_id(), a.name),
            instruction,
            roles_toml: a.roles_toml.clone(),
            checkpoint_threshold: opts.checkpoint_threshold,
            resume_from: None,
            mock_script,
            hub: Some(hub_info(i)),
            review: None,
            extra_env: a.extra_env.clone(),
        };
        Ok::<_, String>(Prepared {
            idx: i,
            mediator,
            iso,
            checkpoints,
            cfg,
        })
    };
    let base_now = |queue: &mut MergeQueue<'_>| {
        let main = queue.main();
        let store_arc = main.shared_store();
        let store = lock_store(&store_arc);
        anchor_snapshot::snapshot(
            &store,
            main.workspace(),
            main.snapshot_options(),
            "team base",
        )
        .map(|r| r.id())
        .map_err(|e| e.to_string())
    };

    let submit = |p: &Prepared| {
        let store_arc = p.mediator.shared_store();
        let store = lock_store(&store_arc);
        iso::submit(&store, &p.iso).ok()
    };
    let max_attempts = team.verify.as_ref().map_or(1, |v| v.max_attempts);

    let mut results: Vec<Option<AgentResult>> = (0..team.agents.len()).map(|_| None).collect();
    // Merge an agent's work; on a rejection worth another try, give the
    // agent (a fresh session) the feedback and its rejected candidate, and
    // try again, up to `max_attempts` submissions (spec §8.2(c)).
    let mut finish = |queue: &mut MergeQueue<'_>,
                      mut p: Prepared,
                      mut session: Result<SessionReport, String>,
                      mut submitted: Option<anchor_snapshot::Id>|
     -> Result<(), String> {
        let i = p.idx;
        let mut attempts: Vec<Outcome> = Vec::new();
        let mut sessions = Vec::new();
        loop {
            let outcome = match submitted {
                Some(sub) if finished_ok(&session) => {
                    let claims = session
                        .as_ref()
                        .map(|r| r.claims.clone())
                        .unwrap_or_default();
                    let e = queue.submit(ids[i], &p.iso.base, &sub, &claims)?;
                    Some(queue.process(e.seq)?)
                }
                _ => None,
            };
            let retry = match &outcome {
                Some(Outcome::Rejected {
                    reason: reason @ (RejectKind::Gate | RejectKind::Conflict),
                    feedback,
                    pre: Some(pre),
                    candidate: Some(cand),
                    ..
                }) if attempts.len() + 1 < max_attempts => {
                    Some((*reason, feedback.clone(), *pre, *cand))
                }
                _ => None,
            };
            if let Some(o) = outcome {
                attempts.push(o);
            }
            let Some((reason, feedback, pre, cand)) = retry else {
                break;
            };
            let attempt = attempts.len() + 1;
            let _ = queue.main().engine().log(
                "repair_started",
                serde_json::json!({
                    "agent": team.agents[i].name,
                    "agent_id": ids[i],
                    "attempt": attempt,
                    "after": attempts.last().map(Outcome::seq),
                    "reason": reason.as_str(),
                }),
            );
            let instruction = format!(
                "{}\n\n{}",
                team.agents[i].instruction,
                repair_note(reason, &feedback)
            );
            sessions.push(std::mem::replace(&mut session, Err(String::new())));
            // Closed first: on Windows the open workspace pins its
            // directory, which `prepare` removes for the next attempt.
            drop(p);
            p = prepare(queue, i, attempt, &cand, &pre, instruction)?;
            session = run_one(&mut p, cancel).map_err(|e| e.to_string());
            submitted = submit(&p);
        }
        // The agent's work is settled: free its claims for the others.
        if let Ok(claims) = hub.list_claims(Caller::User, true) {
            for c in claims.iter().filter(|c| c.agent_id == ids[i]) {
                let _ = hub.revoke_claim(Caller::User, c.id);
            }
        }
        let merge = attempts.last().cloned();
        if merge.as_ref().is_some_and(Outcome::is_merged) {
            let store_arc = queue.main().shared_store();
            let store = lock_store(&store_arc);
            let (root, dir) = (p.iso.root.clone(), p.iso.ws.root().to_path_buf());
            // Every handle on the directory is closed first: on Windows the
            // open workspace pins it against removal (F-1).
            drop(p);
            iso::teardown(&store, &root, &dir)?;
        }
        results[i] = Some(AgentResult {
            name: team.agents[i].name.clone(),
            agent_id: ids[i],
            model_id: idents[i].1.clone(),
            family: idents[i].2.clone(),
            session,
            earlier_sessions: sessions,
            merge,
            attempts,
        });
        Ok(())
    };

    match team.schedule {
        Schedule::Sequential => {
            for i in 0..team.agents.len() {
                let base = base_now(&mut queue)?;
                let instruction = team.agents[i].instruction.clone();
                let mut p = prepare(&mut queue, i, 1, &base, &base, instruction)?;
                let session = run_one(&mut p, cancel).map_err(|e| e.to_string());
                let sub = submit(&p);
                finish(&mut queue, p, session, sub)?;
            }
        }
        Schedule::Parallel => {
            let base = base_now(&mut queue)?;
            let mut prepared = Vec::new();
            for i in 0..team.agents.len() {
                let instruction = team.agents[i].instruction.clone();
                prepared.push(prepare(&mut queue, i, 1, &base, &base, instruction)?);
            }
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|s| -> Result<(), String> {
                for mut p in prepared {
                    let tx = tx.clone();
                    s.spawn(move || {
                        let session = run_one(&mut p, cancel).map_err(|e| e.to_string());
                        let sub = submit(&p);
                        let _ = tx.send((p, session, sub));
                    });
                }
                drop(tx);
                // Merge in completion order, one at a time.
                for (p, session, sub) in rx {
                    finish(&mut queue, p, session, sub)?;
                }
                Ok(())
            })?;
        }
    }
    for id in &ids {
        let _ = hub.retire_agent(Caller::User, *id);
    }
    report.agents = results.into_iter().flatten().collect();
    Ok(report)
}
