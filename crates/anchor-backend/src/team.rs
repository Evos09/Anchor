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
use crate::queue::{MergeGate, MergeQueue, Outcome};
use crate::session::{self, SessionConfig, SessionReport};
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
    pub messages_need_approval: bool,
}

#[derive(Debug, Clone)]
pub struct Team {
    pub schedule: Schedule,
    pub agents: Vec<TeamAgent>,
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
                name: a.name,
                instruction: a.instruction,
                claim_within: a.claim_within,
                messages_need_approval: a.messages_need_approval,
            });
        }
        let team = Team {
            schedule: file.schedule,
            agents,
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
        Ok(())
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
        Shared {
            name: name.to_string(),
            inner: inner.clone(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn Interaction>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Interaction for Shared {
    fn confirm(&mut self, view: &PromptView) -> UserResponse {
        let mut v = view.clone();
        v.rule = format!("[{}] {}", self.name, v.rule);
        self.lock().confirm(&v)
    }
    fn resume_choice(&mut self, checkpoint_id: &str, ratio_hint: &str) -> ResumeChoice {
        self.lock().resume_choice(checkpoint_id, ratio_hint)
    }
    fn notify(&mut self, line: &str) {
        self.lock().notify(&format!("[{}] {line}", self.name));
    }
    fn responder(&self) -> Responder {
        self.lock().responder()
    }
}

pub struct TeamOptions {
    pub agent_bin: PathBuf,
    pub state_base: PathBuf,
    pub auto_merge: bool,
    pub checkpoint_threshold: f64,
    pub gate: Box<dyn MergeGate>,
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
    pub session: Result<SessionReport, String>,
    pub merge: Option<Outcome>,
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

fn finished_ok(r: &Result<SessionReport, String>) -> bool {
    matches!(r, Ok(rep) if rep.done.is_some() && rep.failed.is_none())
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
    let _ = opened.mediator.engine().log(
        "team_started",
        serde_json::json!({
            "schedule": team.schedule,
            "agents": team.agents.iter().zip(&ids).zip(&idents).map(|((a, id), (role, model, family))| {
                serde_json::json!({ "name": a.name, "agent_id": id, "role": role, "model_id": model, "family": family })
            }).collect::<Vec<_>>(),
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

    let gate = std::mem::replace(&mut opts.gate, Box::new(crate::queue::NoGate));
    let paths = opened.paths.clone();
    let mut queue = MergeQueue::new(hub.clone(), &mut opened.mediator, gate, opts.auto_merge);
    let mut report = TeamReport::default();
    for seq in queue.recover()? {
        report.recovered.push(queue.process(seq)?);
    }

    // Isolated workspace, mediator and session config for agent `i`, based
    // on the main workspace as it is now.
    let prepare = |queue: &mut MergeQueue<'_>, i: usize, base: &anchor_snapshot::Id| {
        let a = &team.agents[i];
        let main = queue.main();
        let store_arc = main.shared_store();
        let iso = {
            let store = lock_store(&store_arc);
            iso::create(
                &store,
                main.snapshot_options(),
                main.engine(),
                &iso_base,
                iso::root_for(ids[i]),
                base,
            )?
        };
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
        let cfg = SessionConfig {
            agent_bin: opts.agent_bin.clone(),
            session_id: format!("{}-{}", session::new_session_id(), a.name),
            instruction: a.instruction.clone(),
            roles_toml: a.roles_toml.clone(),
            checkpoint_threshold: opts.checkpoint_threshold,
            resume_from: None,
            mock_script: a.mock_script.clone(),
            hub: Some(hub_info(i)),
            review: None,
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

    let mut results: Vec<Option<AgentResult>> = (0..team.agents.len()).map(|_| None).collect();
    let mut finish = |queue: &mut MergeQueue<'_>,
                      p: Prepared,
                      session: Result<SessionReport, String>,
                      submitted: Option<anchor_snapshot::Id>|
     -> Result<(), String> {
        let i = p.idx;
        let merge = match submitted {
            Some(sub) if finished_ok(&session) => {
                let e = queue.submit(ids[i], &p.iso.base, &sub)?;
                Some(queue.process(e.seq)?)
            }
            _ => None,
        };
        // The agent's work is settled: free its claims for the others.
        if let Ok(claims) = hub.list_claims(Caller::User, true) {
            for c in claims.iter().filter(|c| c.agent_id == ids[i]) {
                let _ = hub.revoke_claim(Caller::User, c.id);
            }
        }
        if merge.as_ref().is_some_and(Outcome::is_merged) {
            let store_arc = queue.main().shared_store();
            let store = lock_store(&store_arc);
            iso::teardown(&store, &p.iso.root, p.iso.ws.root())?;
        }
        results[i] = Some(AgentResult {
            name: team.agents[i].name.clone(),
            agent_id: ids[i],
            model_id: idents[i].1.clone(),
            family: idents[i].2.clone(),
            session,
            merge,
        });
        Ok(())
    };
    let submit = |p: &Prepared| {
        let store_arc = p.mediator.shared_store();
        let store = lock_store(&store_arc);
        iso::submit(&store, &p.iso).ok()
    };

    match team.schedule {
        Schedule::Sequential => {
            for i in 0..team.agents.len() {
                let base = base_now(&mut queue)?;
                let mut p = prepare(&mut queue, i, &base)?;
                let session = session::run(&p.cfg, &mut p.mediator, &p.checkpoints)
                    .map_err(|e| e.to_string());
                let sub = submit(&p);
                finish(&mut queue, p, session, sub)?;
            }
        }
        Schedule::Parallel => {
            let base = base_now(&mut queue)?;
            let mut prepared = Vec::new();
            for i in 0..team.agents.len() {
                prepared.push(prepare(&mut queue, i, &base)?);
            }
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|s| -> Result<(), String> {
                for mut p in prepared {
                    let tx = tx.clone();
                    s.spawn(move || {
                        let session = session::run(&p.cfg, &mut p.mediator, &p.checkpoints)
                            .map_err(|e| e.to_string());
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
