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

use super::{Server, Sources, State, bad, broker::Broker, broker::UiInteraction, ok};
use crate::session::{self, Cancel, SessionConfig};

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
    pub exec_read_only: Vec<PathBuf>,
    pub agent_bin: PathBuf,
    pub policy: Option<PathBuf>,
}

/// What `app.json` remembers between launches.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    #[serde(default)]
    recent: Vec<PathBuf>,
    #[serde(default)]
    roles_path: Option<PathBuf>,
}

struct Project {
    root: PathBuf,
    state: Arc<State>,
    broker: Arc<Broker>,
    checkpoints: Arc<CheckpointStore>,
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
        Arc::new(App {
            inner: Mutex::new(Inner {
                roles_path,
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
            "task" => match (allowed(&["text"]), text("text")) {
                (true, Some(t)) => self.task(t),
                _ => bad("text required (and nothing else)"),
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
            "models": {
                "path": g.roles_path,
                "ok": models.is_ok(),
                "error": models.err(),
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
            cancel: Cancel::new(),
            worker: None,
        });
        self.save(&g);
        ok(json!({ "project": root, "recovered": opened.recovered.is_some() }))
    }

    fn task(&self, text: &str) -> Response {
        let text = text.trim();
        if text.is_empty() {
            return bad("the task is empty");
        }
        if text.len() > MAX_TASK {
            return bad("the task is too long");
        }
        let mut g = self.lock();
        let roles_path = g.roles_path.clone();
        let Some(p) = g.project.as_mut() else {
            return conflict("open a project first");
        };
        if p.running() {
            return conflict("a task is already running");
        }
        if let Err(e) = check_models(&roles_path) {
            return conflict(&format!("the models file is not usable: {e}"));
        }
        let roles_toml = match std::fs::read_to_string(&roles_path) {
            Ok(t) => t,
            Err(e) => return conflict(&format!("{}: {e}", roles_path.display())),
        };
        if let Err(r) = self.audit(&p.state, "start_task", json!({ "text": text })) {
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
        let cfg = SessionConfig {
            agent_bin: self.cfg.agent_bin.clone(),
            session_id: format!("{}-{n}", session::new_session_id()),
            instruction: text.to_string(),
            roles_toml,
            checkpoint_threshold: anchor_checkpoint::DEFAULT_THRESHOLD,
            resume_from: None,
            mock_script: self.cfg.mock_script.clone(),
            hub: None,
            review: None,
        };
        p.broker.resume();
        p.cancel.reset();
        p.state.running.store(true, Ordering::SeqCst);
        let state = p.state.clone();
        let checkpoints = p.checkpoints.clone();
        let cancel = p.cancel.clone();
        let session_id = cfg.session_id.clone();
        let worker = std::thread::spawn(move || {
            if let Err(e) =
                session::run_cancellable(&cfg, &mut mediator, &checkpoints, Some(&cancel))
            {
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
        ok(json!({ "ok": true, "session_id": session_id }))
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
