//! The desktop app's backend (`anchor app`, ADR-0028): the user opens a
//! project folder, types a task, watches the agent, answers its approvals
//! and undoes turns, all from the page.
//!
//! One project and one task at a time. A task runs on a worker thread
//! that borrows the project's [`Mediator`] for the session and hands it
//! back afterwards, so the existing rollback endpoints work whenever no
//! task is running (the `can_rollback` rule). Approvals go through the
//! same [`Broker`] as `team run --ui`; Stop kills the runtime and refuses
//! whatever it was waiting on.
//!
//! The folder picker lists **directory names only**, never file names or
//! contents, and only behind the token like every other endpoint.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anchor_checkpoint::CheckpointStore;
use anchor_hub::http::Response;
use serde_json::{Value, json};

use super::pair::{self, PairSpec};
use super::{Server, Sources, State, bad, broker::Broker, broker::UiInteraction, ok};
use crate::inference::keys::SecretEnv;
use crate::inference::{ByokChoice, Collab, Inference, Routed, Settings, Source};
use crate::session::{self, Cancel, SessionConfig};
use crate::team;

/// Most directory names one listing returns.
const MAX_DIRS: usize = 2000;
/// Most recent projects remembered.
const MAX_RECENT: usize = 10;
/// Longest task text accepted.
const MAX_TASK: usize = 20_000;

/// How `anchor app` was started.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Where every workspace's state lives (and `app.json`).
    pub state_base: PathBuf,
    /// The models (roles) file used until the user picks another.
    pub roles_path: PathBuf,
    /// Scripted responses for `kind = "mock"` providers (demos, tests).
    pub mock_script: Option<String>,
    /// Scripted reviewer models, one per review (the last repeats). With
    /// `mock_script` and none of these, builder + checker reviews nothing.
    pub review_mock_scripts: Vec<String>,
    /// Scripted builder models for review rounds 2, 3, … (last repeats).
    pub repair_mock_scripts: Vec<String>,
    /// Scripted model of the second builder (two-builder tasks; the first
    /// uses `mock_script`).
    pub team_mock_script: Option<String>,
    pub exec_read_only: Vec<PathBuf>,
    pub agent_bin: PathBuf,
    pub policy: Option<PathBuf>,
    /// `--roles` was given: tasks use that file, not the router.
    pub roles_explicit: bool,
    /// The three inference paths (Update #1).
    pub inference: Inference,
}

/// What `app.json` remembers between launches.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    #[serde(default)]
    recent: Vec<PathBuf>,
    #[serde(default)]
    roles_path: Option<PathBuf>,
    #[serde(default)]
    inference: Settings,
}

struct Project {
    root: PathBuf,
    state: Arc<State>,
    broker: Arc<Broker>,
    checkpoints: Arc<CheckpointStore>,
    paths: crate::StatePaths,
    cancel: Arc<Cancel>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Project {
    fn running(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| !w.is_finished())
    }
}

struct Inner {
    roles_path: PathBuf,
    settings: Settings,
    recent: Vec<PathBuf>,
    project: Option<Project>,
}

pub struct App {
    cfg: AppConfig,
    inner: Mutex<Inner>,
    tasks: AtomicU64,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("cfg", &self.cfg).finish()
    }
}

fn conflict(msg: &str) -> Response {
    Response::error(409, msg)
}

impl App {
    pub fn new(cfg: AppConfig) -> Arc<App> {
        let saved: Saved = std::fs::read(cfg.state_base.join("app.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let roles_path = saved
            .roles_path
            .filter(|p| p.is_file())
            .unwrap_or_else(|| cfg.roles_path.clone());
        let mut settings = saved.inference;
        if cfg.roles_explicit {
            settings.source = Source::File;
        }
        Arc::new(App {
            inner: Mutex::new(Inner {
                roles_path,
                settings,
                recent: saved.recent,
                project: None,
            }),
            cfg,
            tasks: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, g: &Inner) {
        let saved = Saved {
            recent: g.recent.clone(),
            roles_path: Some(g.roles_path.clone()),
            inference: g.settings.clone(),
        };
        let path = self.cfg.state_base.join("app.json");
        let tmp = self.cfg.state_base.join(".app.json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(&saved) {
            // Best effort: losing the recent list is harmless.
            let _ = std::fs::create_dir_all(&self.cfg.state_base)
                .and_then(|_| std::fs::write(&tmp, bytes))
                .and_then(|_| std::fs::rename(&tmp, &path));
        }
    }

    /// Stop the running task (the window is closing).
    pub fn shutdown(&self) {
        let g = self.lock();
        if let Some(p) = &g.project {
            p.cancel.cancel();
            p.broker.cancel_pending();
        }
    }

    pub(super) fn get(&self, api: &str, q: &BTreeMap<String, String>) -> Response {
        match api {
            "state" => self.state(),
            "dirs" => dirs(q.get("path").map(String::as_str)),
            "inference" => self.inference_status(),
            "key_models" => match q.get("provider") {
                Some(p) => match self.cfg.inference.key_models(p) {
                    Ok(models) => ok(json!({ "models": models })),
                    Err(e) => conflict(&e),
                },
                None => bad("provider required"),
            },
            _ => Response::error(404, "no such endpoint"),
        }
    }

    pub(super) fn post(&self, srv: &Server, api: &str, body: &Value) -> Response {
        let allowed = |keys: &[&str]| {
            body.as_object()
                .is_some_and(|m| m.keys().all(|k| keys.contains(&k.as_str())))
        };
        let text = |k: &str| body[k].as_str();
        match api {
            "open" => match (allowed(&["path"]), text("path")) {
                (true, Some(p)) => self.open(srv, p),
                _ => bad("path required (and nothing else)"),
            },
            "task" => match (allowed(&["text", "mode"]), text("text")) {
                (true, Some(t)) => match &body["mode"] {
                    Value::Null => self.task(t, None),
                    m => match serde_json::from_value::<Collab>(m.clone()) {
                        Ok(m) => self.task(t, Some(m)),
                        Err(_) => bad("mode must be single, pair or team"),
                    },
                },
                _ => bad("text required (and mode, nothing else)"),
            },
            "cancel" => {
                if !allowed(&[]) {
                    return bad("unexpected fields");
                }
                self.cancel()
            }
            "models" => match (allowed(&["path"]), text("path")) {
                (true, Some(p)) => self.models(p),
                _ => bad("path required (and nothing else)"),
            },
            "inference" => self.set_inference(body),
            "pin" => match (allowed(&["path"]), &body["path"]) {
                (true, Value::Null) => self.pin(None),
                (true, v) => match serde_json::from_value::<anchor_inference::Path>(v.clone()) {
                    Ok(p) => self.pin(Some(p)),
                    Err(_) => bad("path must be A, B, C or null"),
                },
                _ => bad("path required (and nothing else)"),
            },
            "key" => match (allowed(&["provider", "key"]), text("provider"), text("key")) {
                (true, Some(p), Some(k)) => self.add_key(p, k),
                _ => bad("provider and key required (and nothing else)"),
            },
            "key_model" => match (
                allowed(&["provider", "model"]),
                text("provider"),
                text("model"),
            ) {
                (true, Some(p), Some(m)) => self.key_model(p, m),
                _ => bad("provider and model required (and nothing else)"),
            },
            "key_remove" => match (allowed(&["provider"]), text("provider")) {
                (true, Some(p)) => self.remove_key(p),
                _ => bad("provider required (and nothing else)"),
            },
            "free_start" => {
                if !allowed(&[]) {
                    return bad("unexpected fields");
                }
                self.start_free()
            }
            _ => Response::error(404, "no such action"),
        }
    }

    fn state(&self) -> Response {
        let g = self.lock();
        let models = check_models(&g.roles_path);
        ok(json!({
            "project": g.project.as_ref().map(|p| &p.root),
            "running": g.project.as_ref().is_some_and(Project::running),
            "recent": g.recent,
            // Recent folders that have since gone (moved or deleted).
            "missing": g.recent.iter().filter(|p| !p.is_dir()).collect::<Vec<_>>(),
            "models": {
                "path": g.roles_path,
                "ok": models.is_ok(),
                "error": models.err(),
                "roles": role_models(&g.roles_path),
            },
            "inference": {
                "source": g.settings.source,
                "preference": g.settings.preference,
                "collab": g.settings.collab,
                "pair_rounds": g.settings.pair_rounds,
                "pin": g.project.as_ref().and_then(|p| g.settings.pins.get(&p.root)),
                "free_offered": self.cfg.inference.free_url.is_some(),
            },
            "mock": self.cfg.mock_script.is_some(),
            "platform": std::env::consts::OS,
            "home": crate::home_dir(),
        }))
    }

    /// Audited like every dashboard action, in the project being acted on.
    fn audit(&self, state: &State, action: &str, args: Value) -> Result<(), Response> {
        state.audit_action(action, &args)
    }

    fn open(&self, srv: &Server, path: &str) -> Response {
        let wanted = Path::new(path);
        if !wanted.is_absolute() {
            return bad("the project path must be absolute");
        }
        let root = match std::fs::canonicalize(wanted) {
            Ok(r) if r.is_dir() => anchor_fs::normalize_verbatim(&r),
            Ok(_) => return bad("not a folder"),
            Err(e) => return bad(&format!("{path}: {e}")),
        };
        let mut g = self.lock();
        if g.project.as_ref().is_some_and(Project::running) {
            return conflict("a task is running; stop it or wait for it to finish");
        }
        // Let go of the old project first: its snapshot store is locked
        // while open, and it may be the very folder being reopened.
        if let Some(old) = g.project.take() {
            old.broker.close();
            if let Some(w) = old.worker {
                let _ = w.join();
            }
        }
        srv.set_current(None);
        let broker = Broker::new();
        let opened = match crate::open_workspace_in(
            &root,
            &self.cfg.state_base,
            self.cfg.policy.as_deref(),
            Box::new(UiInteraction::new(broker.clone())),
            self.cfg.exec_read_only.clone(),
        ) {
            Ok(o) => o,
            Err(e) => return conflict(&format!("cannot open {}: {e}", root.display())),
        };
        let audit = opened.mediator.engine().audit_arc();
        broker.set_audit(audit.clone());
        let state = Arc::new(State::new(Sources {
            workspace: root.clone(),
            audit,
            hub: None,
            store: Some(opened.mediator.shared_store()),
            store_note: None,
            broker: Some(broker.clone()),
            mode: "app",
        }));
        if let Err(r) = self.audit(&state, "open_project", json!({ "path": root })) {
            return r;
        }
        *state.workspace.lock().unwrap_or_else(|p| p.into_inner()) = Some(opened.mediator);
        srv.set_current(Some(state.clone()));
        g.recent.retain(|p| p != &root);
        g.recent.insert(0, root.clone());
        g.recent.truncate(MAX_RECENT);
        g.project = Some(Project {
            root: root.clone(),
            state,
            broker,
            checkpoints: Arc::new(opened.checkpoints),
            paths: opened.paths.clone(),
            cancel: Cancel::new(),
            worker: None,
        });
        self.save(&g);
        ok(json!({ "project": root, "recovered": opened.recovered.is_some() }))
    }

    fn task(&self, text: &str, mode: Option<Collab>) -> Response {
        let mut text = text.trim();
        let mut mode = mode;
        if let Some(rest) = text.strip_prefix("/team ") {
            // `/team <task>`: two builders, for this task only.
            text = rest.trim();
            mode = Some(Collab::Team);
        }
        if text.is_empty() {
            return bad("the task is empty");
        }
        if text.len() > MAX_TASK {
            return bad("the task is too long");
        }
        // Route outside the lock: it probes Ollama and the free tier.
        let (settings, root) = {
            let g = self.lock();
            let Some(p) = g.project.as_ref() else {
                return conflict("open a project first");
            };
            if p.running() {
                return conflict("a task is already running");
            }
            (g.settings.clone(), p.root.clone())
        };
        let collab = mode.unwrap_or(settings.collab);
        let routed = (settings.source == Source::Routed)
            .then(|| self.cfg.inference.route(&settings, &root, text));
        let (text, plan, skipped) = match routed {
            Some(Routed::Prompt(prompt)) => {
                // Not an error: the page asks the user what to do.
                return ok(json!({ "ok": false, "prompt": prompt }));
            }
            Some(Routed::Run {
                plan,
                skipped,
                text,
            }) => (text, Some(plan), skipped),
            None => (text.to_string(), None, Vec::new()),
        };
        if text.is_empty() {
            return bad("the task is empty");
        }
        let mut g = self.lock();
        let roles_path = g.roles_path.clone();
        let Some(p) = g.project.as_mut() else {
            return conflict("open a project first");
        };
        if p.running() {
            return conflict("a task is already running");
        }
        let (roles_toml, extra_env) = match &plan {
            Some(plan) => (plan.roles_toml.clone(), plan.env.clone()),
            None => {
                if let Err(e) = check_models(&roles_path) {
                    return conflict(&format!("the models file is not usable: {e}"));
                }
                match std::fs::read_to_string(&roles_path) {
                    Ok(t) => (t, Default::default()),
                    Err(e) => return conflict(&format!("{}: {e}", roles_path.display())),
                }
            }
        };
        let route = plan.as_ref().map(|plan| {
            json!({
                "path": plan.path,
                "executor": { "model": plan.executor.model_id, "family": plan.executor.family, "path": plan.executor.path },
                "reviewer": { "model": plan.reviewer.model_id, "family": plan.reviewer.family, "path": plan.reviewer.path },
                "skipped": skipped.iter().map(|(p, why)| json!({ "path": p, "why": why })).collect::<Vec<_>>(),
            })
        });
        if collab == Collab::Team {
            return self.start_team(p, text, plan.as_deref(), roles_toml, extra_env, route);
        }
        // Builder + checker reviews nothing in a scripted demo that has
        // no scripted reviewer.
        let scripted_without_reviewer =
            self.cfg.mock_script.is_some() && self.cfg.review_mock_scripts.is_empty();
        let rounds = match collab {
            Collab::Pair if !scripted_without_reviewer => {
                settings.pair_rounds.min(pair::MAX_ROUNDS)
            }
            _ => 0,
        };
        let collab = if rounds == 0 { Collab::Single } else { collab };
        if let Err(r) = self.audit(
            &p.state,
            "start_task",
            json!({ "text": text, "route": route, "collab": collab, "rounds": rounds }),
        ) {
            return r;
        }
        let Some(mut mediator) = p
            .state
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return conflict("the workspace is busy");
        };
        let n = self.tasks.fetch_add(1, Ordering::SeqCst) + 1;
        let spec = PairSpec {
            rounds,
            state_base: self.cfg.state_base.clone(),
            paths: p.paths.clone(),
            env: extra_env.clone(),
            review_mock_scripts: self.cfg.review_mock_scripts.clone(),
            repair_mock_scripts: self.cfg.repair_mock_scripts.clone(),
        };
        let cfg = SessionConfig {
            agent_bin: self.cfg.agent_bin.clone(),
            session_id: format!("{}-{n}", session::new_session_id()),
            instruction: text.clone(),
            roles_toml,
            checkpoint_threshold: anchor_checkpoint::DEFAULT_THRESHOLD,
            resume_from: None,
            mock_script: self.cfg.mock_script.clone(),
            hub: None,
            review: None,
            extra_env,
        };
        p.broker.resume();
        p.cancel.reset();
        p.state.running.store(true, Ordering::SeqCst);
        let state = p.state.clone();
        let checkpoints = p.checkpoints.clone();
        let cancel = p.cancel.clone();
        let session_id = cfg.session_id.clone();
        if let Some(r) = &route {
            let _ = mediator.log("notice", json!({ "kind": "route", "route": r }));
        }
        let worker = std::thread::spawn(move || {
            if let Err(e) = pair::run(&cfg, &mut mediator, &checkpoints, &cancel, &spec) {
                // session_finished already carries the error; this says
                // the session never started.
                let _ = mediator.log(
                    "notice",
                    json!({ "kind": "task_error", "detail": format!("the task could not run: {e}") }),
                );
            }
            *state.workspace.lock().unwrap_or_else(|p| p.into_inner()) = Some(mediator);
            state.running.store(false, Ordering::SeqCst);
        });
        p.worker = Some(worker);
        ok(json!({ "ok": true, "session_id": session_id, "route": route, "collab": collab }))
    }

    /// Two builders at once (ADR-0039): a parallel team of two agents in
    /// isolated copies of the project, coordinating through the Hub; every
    /// merge into the project is confirmed by the user.
    fn start_team(
        &self,
        p: &mut Project,
        text: String,
        plan: Option<&crate::inference::plan::Plan>,
        roles_a: String,
        env_a: SecretEnv,
        route: Option<Value>,
    ) -> Response {
        // Routed: builder B runs the plan's second model (another family).
        let (roles_b, env_b) = match plan {
            Some(plan) => {
                let b = crate::inference::plan::roles(plan.path, &plan.reviewer, &plan.executor);
                (b.roles_toml, b.env)
            }
            None => (roles_a.clone(), env_a.clone()),
        };
        let model = |roles: &str| {
            team::roles_identity(roles)
                .map(|(_, model, _)| model)
                .unwrap_or_else(|_| "another model".into())
        };
        // Hub names stay taken after a run (agents are retired, not
        // deleted): each two-builder task gets its own.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 10_000_000_000)
            .unwrap_or(0);
        let names = [format!("builder-a-{stamp}"), format!("builder-b-{stamp}")];
        let models = [model(&roles_a), model(&roles_b)];
        let instruction = |me: usize| {
            let other = 1 - me;
            format!(
                "{text}\n\nYou are {}, one of two builders working on this project at the same \
                 time; your partner is {} ({}). Split the work: first claim the files you will \
                 change (hub_request_claim), message your partner about who does what \
                 (hub_send_message), and do not touch files your partner claimed. When your part \
                 is done, finish with a short summary of what you changed.",
                names[me], names[other], models[other]
            )
        };
        let agent =
            |i: usize, roles: String, env: SecretEnv, script: Option<String>| team::TeamAgent {
                name: names[i].clone(),
                roles_toml: roles,
                instruction: instruction(i),
                claim_within: vec![".".into()],
                mock_script: script,
                repair_mock_scripts: Vec::new(),
                messages_need_approval: false,
                extra_env: env,
            };
        let squad = team::Team {
            schedule: team::Schedule::Parallel,
            agents: vec![
                agent(0, roles_a, env_a, self.cfg.mock_script.clone()),
                agent(
                    1,
                    roles_b,
                    env_b,
                    self.cfg
                        .team_mock_script
                        .clone()
                        .or_else(|| self.cfg.mock_script.clone()),
                ),
            ],
            verify: None,
        };
        if let Err(e) = squad.validate() {
            return conflict(&format!("two builders cannot start: {e}"));
        }
        let checkpoints = match CheckpointStore::open(&p.paths.checkpoints) {
            Ok(c) => c,
            Err(e) => return conflict(&format!("checkpoints: {e}")),
        };
        if let Err(r) = self.audit(
            &p.state,
            "start_task",
            json!({ "text": text, "route": route, "collab": Collab::Team, "builders": names.iter().zip(&models).map(|(n, m)| json!({ "name": n, "model": m })).collect::<Vec<_>>() }),
        ) {
            return r;
        }
        let Some(mediator) = p
            .state
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return conflict("the workspace is busy");
        };
        if let Some(r) = &route {
            let _ = mediator.log("notice", json!({ "kind": "route", "route": r }));
        }
        p.broker.resume();
        p.cancel.reset();
        p.state.running.store(true, Ordering::SeqCst);
        let state = p.state.clone();
        let cancel = p.cancel.clone();
        let broker = p.broker.clone();
        let opts = team::TeamOptions {
            agent_bin: self.cfg.agent_bin.clone(),
            state_base: self.cfg.state_base.clone(),
            auto_merge: false,
            checkpoint_threshold: anchor_checkpoint::DEFAULT_THRESHOLD,
            gate: Box::new(crate::queue::NoGate),
            cancel: Some(cancel),
        };
        let mut opened = crate::Opened {
            paths: p.paths.clone(),
            mediator,
            checkpoints,
            recovered: None,
        };
        let worker = std::thread::spawn(move || {
            let interact = team::Shared::wrap(Box::new(UiInteraction::new(broker)));
            match team::run(&mut opened, &squad, opts, interact) {
                Ok(rep) => {
                    let agents: Vec<Value> = rep
                        .agents
                        .iter()
                        .map(|a| {
                            json!({
                                "name": a.name,
                                "model": a.model_id,
                                "family": a.family,
                                "merged": a.merge.as_ref().is_some_and(crate::queue::Outcome::is_merged),
                                "done": a.session.as_ref().ok().and_then(|r| r.done.clone()),
                                "failed": match &a.session {
                                    Ok(r) => r.failed.clone(),
                                    Err(e) => Some(e.clone()),
                                },
                            })
                        })
                        .collect();
                    let _ = opened
                        .mediator
                        .log("team_finished", json!({ "agents": agents }));
                }
                Err(e) => {
                    let _ = opened.mediator.log(
                        "notice",
                        json!({ "kind": "task_error", "detail": format!("the builders could not run: {e}") }),
                    );
                }
            }
            *state.workspace.lock().unwrap_or_else(|p| p.into_inner()) = Some(opened.mediator);
            state.running.store(false, Ordering::SeqCst);
        });
        p.worker = Some(worker);
        ok(json!({ "ok": true, "collab": Collab::Team, "route": route, "builders": names }))
    }

    fn cancel(&self) -> Response {
        let g = self.lock();
        let Some(p) = g.project.as_ref() else {
            return conflict("no project is open");
        };
        if !p.running() {
            return conflict("no task is running");
        }
        if let Err(r) = self.audit(&p.state, "cancel_task", json!({})) {
            return r;
        }
        p.cancel.cancel();
        p.broker.cancel_pending();
        ok(json!({ "ok": true }))
    }

    fn models(&self, path: &str) -> Response {
        let p = Path::new(path);
        if !p.is_absolute() {
            return bad("the models file path must be absolute");
        }
        if let Err(e) = check_models(p) {
            return bad(&e);
        }
        let mut g = self.lock();
        if let Some(proj) = &g.project
            && let Err(r) = self.audit(&proj.state, "set_models", json!({ "path": p }))
        {
            return r;
        }
        g.roles_path = p.to_path_buf();
        self.save(&g);
        ok(json!({ "ok": true }))
    }
}

impl App {
    fn inference_status(&self) -> Response {
        let (settings, root) = {
            let g = self.lock();
            (
                g.settings.clone(),
                g.project.as_ref().map(|p| p.root.clone()),
            )
        };
        let mut v = self.cfg.inference.status(&settings, root.as_deref());
        v["models_file"] = json!(self.lock().roles_path);
        ok(v)
    }

    /// Audited in the open project, if any. Never carries a key.
    fn audit_open(&self, g: &Inner, action: &str, args: Value) -> Result<(), Response> {
        match &g.project {
            Some(p) => self.audit(&p.state, action, args),
            None => Ok(()),
        }
    }

    fn set_inference(&self, body: &Value) -> Response {
        const KEYS: [&str; 8] = [
            "collab",
            "pair_rounds",
            "source",
            "preference",
            "order",
            "ollama_port",
            "preferred",
            "local_family",
        ];
        let Some(m) = body
            .as_object()
            .filter(|m| m.keys().all(|k| KEYS.contains(&k.as_str())))
        else {
            return bad(&format!("expected some of: {}", KEYS.join(", ")));
        };
        let mut g = self.lock();
        let mut s = g.settings.clone();
        let parse = |k: &str| m.get(k).cloned().map(serde_json::from_value::<Value>);
        if let Some(Ok(v)) = parse("source") {
            match serde_json::from_value(v) {
                Ok(src) => s.source = src,
                Err(_) => return bad("source must be routed or file"),
            }
        }
        if let Some(Ok(v)) = parse("preference") {
            match serde_json::from_value(v) {
                Ok(p) => s.preference = p,
                Err(_) => {
                    return bad("preference must be auto, prefer_local, prefer_byok or free_only");
                }
            }
        }
        if let Some(Ok(v)) = parse("order") {
            match serde_json::from_value::<Option<Vec<anchor_inference::Path>>>(v) {
                Ok(Some(o)) if !o.is_empty() => {
                    let mut seen = o.clone();
                    seen.sort();
                    seen.dedup();
                    if seen.len() != o.len() {
                        return bad("order lists a path twice");
                    }
                    s.order = Some(o);
                }
                Ok(_) => s.order = None,
                Err(_) => return bad("order must be a list of A, B, C (or null)"),
            }
        }
        if let Some(v) = m.get("ollama_port") {
            match v
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p > 0)
            {
                Some(p) => s.ollama_port = p,
                None => return bad("ollama_port must be 1-65535"),
            }
        }
        if let Some(v) = m.get("preferred") {
            let path = v
                .get("path")
                .cloned()
                .map(serde_json::from_value::<anchor_inference::Path>);
            match (path, v.get("model")) {
                (Some(Ok(p)), Some(Value::String(model))) if !model.is_empty() => {
                    s.preferred.insert(p, model.clone());
                }
                (Some(Ok(p)), Some(Value::Null)) => {
                    s.preferred.remove(&p);
                }
                _ => return bad("preferred must be {path, model}"),
            }
        }
        if let Some(v) = m.get("local_family") {
            match (v.get("model").and_then(Value::as_str), v.get("family")) {
                (Some(model), Some(Value::String(f))) if valid_family(f) => {
                    s.local_families
                        .insert(model.to_string(), f.trim().to_ascii_lowercase());
                }
                (Some(model), Some(Value::Null)) => {
                    s.local_families.remove(model);
                }
                _ => {
                    return bad(
                        "local_family must be {model, family}; a family is letters, digits, - or _",
                    );
                }
            }
        }
        if let Some(v) = m.get("collab") {
            match serde_json::from_value(v.clone()) {
                Ok(c) => s.collab = c,
                Err(_) => return bad("collab must be single, pair or team"),
            }
        }
        if let Some(v) = m.get("pair_rounds") {
            match v
                .as_u64()
                .filter(|n| (1..=u64::from(pair::MAX_ROUNDS)).contains(n))
            {
                Some(n) => s.pair_rounds = n as u32,
                None => return bad("pair_rounds must be 1-5"),
            }
        }
        if let Err(r) = self.audit_open(&g, "set_inference", body.clone()) {
            return r;
        }
        g.settings = s;
        self.save(&g);
        ok(json!({ "ok": true }))
    }

    fn pin(&self, path: Option<anchor_inference::Path>) -> Response {
        let mut g = self.lock();
        let Some(root) = g.project.as_ref().map(|p| p.root.clone()) else {
            return conflict("open a project first");
        };
        if let Err(r) = self.audit_open(&g, "pin_path", json!({ "path": path })) {
            return r;
        }
        match path {
            Some(p) => g.settings.pins.insert(root, p),
            None => g.settings.pins.remove(&root),
        };
        self.save(&g);
        ok(json!({ "ok": true }))
    }

    fn add_key(&self, provider: &str, key: &str) -> Response {
        // The verify call happens outside the lock.
        let models = match self.cfg.inference.add_key(provider, key) {
            Ok(m) => m,
            Err(e) => return conflict(&e),
        };
        let mut g = self.lock();
        if let Err(r) = self.audit_open(&g, "add_key", json!({ "provider": provider })) {
            return r;
        }
        let keep = g
            .settings
            .byok
            .get(provider)
            .map(|c| c.model.clone())
            .filter(|m| models.contains(m));
        g.settings.byok.insert(
            provider.to_string(),
            ByokChoice {
                model: keep.unwrap_or_default(),
            },
        );
        self.save(&g);
        ok(json!({ "ok": true, "models": models }))
    }

    fn key_model(&self, provider: &str, model: &str) -> Response {
        let mut g = self.lock();
        let Some(choice) = g.settings.byok.get_mut(provider) else {
            return conflict("add a key for this provider first");
        };
        choice.model = model.to_string();
        if let Err(r) = self.audit_open(
            &g,
            "set_key_model",
            json!({ "provider": provider, "model": model }),
        ) {
            return r;
        }
        self.save(&g);
        ok(json!({ "ok": true }))
    }

    fn remove_key(&self, provider: &str) -> Response {
        if let Err(e) = self.cfg.inference.remove_key(provider) {
            return conflict(&e);
        }
        let mut g = self.lock();
        g.settings.byok.remove(provider);
        let _ = self.audit_open(&g, "remove_key", json!({ "provider": provider }));
        self.save(&g);
        ok(json!({ "ok": true }))
    }

    fn start_free(&self) -> Response {
        let device = {
            let mut g = self.lock();
            let d = match g.settings.free_device.clone() {
                Some(d) => d,
                None => match new_device_id() {
                    Ok(d) => d,
                    Err(e) => return conflict(&e),
                },
            };
            g.settings.free_device = Some(d.clone());
            self.save(&g);
            d
        };
        match self.cfg.inference.start_free(&device) {
            Ok(()) => {
                let g = self.lock();
                let _ = self.audit_open(&g, "start_free", json!({}));
                ok(json!({ "ok": true }))
            }
            Err(e) => conflict(&e),
        }
    }
}

fn valid_family(f: &str) -> bool {
    let f = f.trim();
    !f.is_empty()
        && f.len() <= 40
        && f.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A random install id for the free tier: 32 hex digits, nothing about
/// the machine (ADR-0037).
fn new_device_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("no randomness for a device id: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Is `path` a models (roles) file Anchor can run with? A quick check for
/// the page; the runtime validates it fully when a task starts.
pub fn check_models(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let t: toml::Table = toml::from_str(&text).map_err(|e| format!("not valid TOML: {e}"))?;
    let family = |role: &str| -> Result<String, String> {
        let provider = t
            .get("roles")
            .and_then(|r| r.get(role))
            .and_then(|r| r.get("provider"))
            .and_then(|p| p.as_str())
            .ok_or_else(|| format!("[roles.{role}] with a provider is required"))?;
        t.get("providers")
            .and_then(|p| p.get(provider))
            .and_then(|p| p.get("family"))
            .and_then(|f| f.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("provider {provider:?} (role {role}) needs a family"))
    };
    let (exec, review) = (family("executor")?, family("reviewer")?);
    if exec == review {
        return Err(format!(
            "the reviewer must be a different model family than the executor (both {exec:?})"
        ));
    }
    Ok(())
}

/// The model each main role uses, for the page to show (`null` where the
/// file does not say).
fn role_models(path: &Path) -> Value {
    let t: Option<toml::Table> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok());
    let model = |role: &str| {
        t.as_ref()
            .and_then(|t| t.get("roles"))
            .and_then(|r| r.get(role))
            .and_then(|r| r.get("model_id"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
    };
    json!({ "executor": model("executor"), "reviewer": model("reviewer") })
}

/// The folder picker: the directory names under `path` (absolute), or the
/// starting points (home and the filesystem roots) without one.
fn dirs(path: Option<&str>) -> Response {
    let home = crate::home_dir();
    let Some(path) = path.filter(|p| !p.is_empty()) else {
        return ok(json!({ "path": null, "parent": null, "dirs": roots(), "home": home }));
    };
    let p = Path::new(path);
    if !p.is_absolute() {
        return bad("the path must be absolute");
    }
    let dir = match std::fs::canonicalize(p) {
        Ok(d) if d.is_dir() => anchor_fs::normalize_verbatim(&d),
        Ok(_) => return bad("not a folder"),
        Err(e) => return bad(&format!("{path}: {e}")),
    };
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => return conflict(&format!("{}: {e}", dir.display())),
    };
    let mut names: Vec<String> = rd
        .flatten()
        // Directories only (not symlinks to them): names, never contents.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    let truncated = names.len() > MAX_DIRS;
    names.truncate(MAX_DIRS);
    ok(json!({
        "path": dir,
        "parent": dir.parent(),
        "dirs": names.iter().map(|n| dir.join(n)).collect::<Vec<_>>(),
        "names": names,
        "truncated": truncated,
        "home": home,
    }))
}

/// Where browsing starts: the home folder and the filesystem roots.
fn roots() -> Vec<PathBuf> {
    let mut out = vec![crate::home_dir()];
    if cfg!(windows) {
        for d in b'A'..=b'Z' {
            let p = PathBuf::from(format!("{}:\\", d as char));
            if p.is_dir() {
                out.push(p);
            }
        }
    } else {
        out.push(PathBuf::from("/"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_files_are_checked() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("roles.toml");
        let write = |s: &str| std::fs::write(&f, s).unwrap();
        write(
            "[providers.a]\nfamily = \"x\"\n[providers.b]\nfamily = \"y\"\n\
             [roles.executor]\nprovider = \"a\"\n[roles.reviewer]\nprovider = \"b\"\n",
        );
        assert_eq!(check_models(&f), Ok(()));
        write(
            "[providers.a]\nfamily = \"x\"\n\
             [roles.executor]\nprovider = \"a\"\n[roles.reviewer]\nprovider = \"a\"\n",
        );
        assert!(
            check_models(&f)
                .unwrap_err()
                .contains("different model family")
        );
        write("[roles.executor]\nprovider = \"a\"\n");
        assert!(check_models(&f).is_err());
        write("not toml [");
        assert!(check_models(&f).unwrap_err().contains("TOML"));
        let ollama =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/local/roles.ollama.toml");
        assert_eq!(check_models(&ollama), Ok(()));
        assert_eq!(
            role_models(&ollama),
            json!({ "executor": "qwen2.5-coder:7b", "reviewer": "llama3.1:8b" })
        );
        assert_eq!(
            role_models(&d.path().join("absent.toml")),
            json!({ "executor": null, "reviewer": null })
        );
    }

    fn test_inference() -> Inference {
        Inference::new(
            Arc::new(crate::inference::keys::MemoryKeys::default()),
            None,
        )
        .unwrap()
    }

    fn test_app(d: &Path, explicit: bool) -> Arc<App> {
        App::new(AppConfig {
            state_base: d.join("state"),
            roles_path: d.join("roles.toml"),
            mock_script: None,
            review_mock_scripts: vec![],
            repair_mock_scripts: vec![],
            team_mock_script: None,
            exec_read_only: vec![],
            agent_bin: PathBuf::from("anchor-agent"),
            policy: None,
            roles_explicit: explicit,
            inference: test_inference(),
        })
    }

    fn body(r: &Response) -> Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    #[test]
    fn inference_settings_are_checked_saved_and_hold_no_secret() {
        let d = tempfile::tempdir().unwrap();
        let app = test_app(d.path(), false);
        assert_eq!(app.lock().settings.source, Source::Routed);
        let r = app.set_inference(
            &json!({ "preference": "prefer_local", "order": ["C", "A"], "ollama_port": 11500 }),
        );
        assert_eq!(r.status, 200);
        for bad_body in [
            json!({ "preference": "cheapest" }),
            json!({ "order": ["A", "A"] }),
            json!({ "ollama_port": 0 }),
            json!({ "local_family": { "model": "m", "family": "no spaces" } }),
            json!({ "api_key": "sk-x" }),
        ] {
            assert_eq!(app.set_inference(&bad_body).status, 400, "{bad_body}");
        }
        assert_eq!(
            app.set_inference(
                &json!({ "local_family": { "model": "my-model:latest", "family": "Qwen" } })
            )
            .status,
            200
        );
        // An unknown provider's key is refused before any network call.
        assert_eq!(app.add_key("nobody", "sk-x").status, 409);
        let saved = std::fs::read_to_string(d.path().join("state/app.json")).unwrap();
        let v: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(v["inference"]["preference"], "prefer_local");
        assert_eq!(v["inference"]["order"], json!(["C", "A"]));
        assert_eq!(v["inference"]["local_families"]["my-model:latest"], "qwen");
        assert!(!saved.contains("sk-x"));
        // Reloaded as saved; `--roles` switches to the file.
        assert_eq!(
            test_app(d.path(), false).lock().settings.preference,
            anchor_inference::Preference::PreferLocal
        );
        assert_eq!(
            test_app(d.path(), true).lock().settings.source,
            Source::File
        );
    }

    #[test]
    fn a_pin_needs_an_open_project() {
        let d = tempfile::tempdir().unwrap();
        let app = test_app(d.path(), false);
        assert_eq!(app.pin(Some(anchor_inference::Path::C)).status, 409);
        let v = body(&app.state());
        assert_eq!(v["inference"]["source"], "routed");
        assert_eq!(v["inference"]["free_offered"], false);
    }

    #[test]
    fn device_ids_are_random_hex() {
        let (a, b) = (new_device_id().unwrap(), new_device_id().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn the_state_names_recent_folders_that_are_gone() {
        let d = tempfile::tempdir().unwrap();
        let (here, gone) = (d.path().join("here"), d.path().join("gone"));
        std::fs::create_dir(&here).unwrap();
        let app = App::new(AppConfig {
            state_base: d.path().join("state"),
            roles_path: d.path().join("roles.toml"),
            mock_script: None,
            review_mock_scripts: vec![],
            repair_mock_scripts: vec![],
            team_mock_script: None,
            exec_read_only: vec![],
            agent_bin: PathBuf::from("anchor-agent"),
            policy: None,
            roles_explicit: false,
            inference: test_inference(),
        });
        app.lock().recent = vec![here.clone(), gone.clone()];
        let v: Value = serde_json::from_slice(&app.state().body).unwrap();
        assert_eq!(v["recent"], json!([here, gone]));
        assert_eq!(v["missing"], json!([gone]));
    }

    #[test]
    fn the_picker_lists_directory_names_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("secret.txt"), "x").unwrap();
        let r = dirs(Some(d.path().to_str().unwrap()));
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["names"], json!(["sub"]));
        assert!(!String::from_utf8_lossy(&r.body).contains("secret"));
        assert_eq!(dirs(Some("relative/path")).status, 400);
        assert_eq!(
            dirs(Some(d.path().join("secret.txt").to_str().unwrap())).status,
            400
        );
    }
}
