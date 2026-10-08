//! `anchor team`, `anchor hub` and `anchor queue` commands.
//!
//! Hub commands only open the Hub database and the audit log (both safe
//! to share between processes), so they work while a team run is going.
//! Commands that touch the snapshot store (`queue revert`) need the
//! store's lock and refuse while another process holds it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anchor_backend::queue::{MergeQueue, NoGate, Outcome};
use anchor_backend::session;
use anchor_backend::team::{self, Team, TeamOptions};
use anchor_backend::{StatePaths, default_exec_read_only, default_state_base, open_workspace};
use anchor_hub::{Agent, AgentId, Caller, Hub, MessageQuery, NewAgent, Permissions};
use anchor_policy::AuditLog;

use crate::{Args, interaction};

fn open_hub(a: &Args) -> Result<(Arc<Hub>, StatePaths), String> {
    let ws =
        anchor_fs::Workspace::open(Path::new(a.need("--workspace")?)).map_err(|e| e.to_string())?;
    let paths = StatePaths::create(ws.root(), &default_state_base()).map_err(|e| e.to_string())?;
    let audit = Arc::new(AuditLog::open(&paths.audit).map_err(|e| e.to_string())?);
    let hub = team::open_hub(&paths, ws.root(), audit)?;
    Ok((hub, paths))
}

fn json_line(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string(v).unwrap_or_default());
}

fn agent_line(g: &Agent) {
    println!(
        "{:>4}  {:<16} {:<8} {:<20} {:<16} {:<10} within {:?}",
        g.id, g.display_name, g.status, g.model_id, g.family, g.role, g.permissions.claim_within
    );
}

fn id_arg(a: &Args, i: usize, what: &str) -> Result<i64, String> {
    a.pos
        .get(i)
        .ok_or_else(|| format!("missing {what}"))?
        .parse()
        .map_err(|e| format!("{what}: {e}"))
}

pub fn cmd_team(a: &Args) -> Result<ExitCode, String> {
    if a.pos.get(1).map(String::as_str) != Some("run") {
        return Err("usage: anchor team run --workspace DIR --team FILE [options]".into());
    }
    let team = Team::load(Path::new(a.need("--team")?))?;
    match (&team.verify, a.has("--unverified")) {
        (None, false) => {
            return Err(
                "the team file has no [verify] section, so merges would not be \
                        verified: add one (spec §8), or pass --unverified to merge without \
                        verification"
                    .into(),
            );
        }
        (Some(_), true) => {
            return Err("--unverified conflicts with the team file's [verify] section".into());
        }
        _ => {}
    }
    let ws = PathBuf::from(a.need("--workspace")?);
    let mut ro = default_exec_read_only();
    ro.extend(a.all("--exec-read-only").into_iter().map(PathBuf::from));
    let want_ui = a.has("--ui");
    // With the dashboard, prompts are answered there (unless the run
    // answers them automatically).
    let broker = (want_ui && !a.has("--auto-approve") && !a.has("--auto-deny"))
        .then(anchor_backend::ui::Broker::new);
    let interact = team::Shared::wrap(match &broker {
        Some(b) => Box::new(anchor_backend::ui::UiInteraction::new(b.clone())),
        None => interaction(a)?,
    });
    let mut opened = open_workspace(
        &ws,
        a.get("--policy").map(Path::new),
        Box::new(team::Shared::new("merge", &interact)),
        ro,
    )
    .map_err(|e| e.to_string())?;
    let threshold = a
        .get("--threshold")
        .map(|t| t.parse::<f64>().map_err(|e| format!("--threshold: {e}")))
        .transpose()?
        .unwrap_or(anchor_checkpoint::DEFAULT_THRESHOLD);
    let opts = TeamOptions {
        agent_bin: a
            .get("--agent")
            .map(PathBuf::from)
            .unwrap_or_else(session::default_agent_bin),
        state_base: default_state_base(),
        auto_merge: a.has("--auto-merge"),
        checkpoint_threshold: threshold,
        gate: Box::new(NoGate),
    };
    eprintln!(
        "anchor: team of {} on {} ({:?})",
        team.agents.len(),
        opened.mediator.workspace().root().display(),
        team.schedule
    );
    let ui = if want_ui {
        let audit = opened.mediator.engine().audit_arc();
        if let Some(b) = &broker {
            b.set_audit(audit.clone());
        }
        let root = opened.mediator.workspace().root().to_path_buf();
        let hub = team::open_hub(&opened.paths, &root, audit.clone())?;
        let ui = anchor_backend::ui::start(
            anchor_backend::ui::Sources {
                workspace: root,
                audit,
                hub: Some(hub),
                store: Some(opened.mediator.shared_store()),
                store_note: None,
                broker: broker.clone(),
                mode: "live",
            },
            ui_port(a)?,
        )
        .map_err(|e| format!("dashboard: {e}"))?;
        announce_ui(a, &ui)?;
        ui.set_running(true);
        Some(ui)
    } else {
        None
    };
    let report = team::run(&mut opened, &team, opts, interact);
    if let Some(ui) = &ui {
        ui.set_running(false);
    }
    let report = report?;
    for o in &report.recovered {
        println!("recovered merge {}: {}", o.seq(), outcome_text(o));
    }
    let mut ok = true;
    for r in &report.agents {
        println!(
            "── agent {} (#{}, {} / {}) ──",
            r.name, r.agent_id, r.model_id, r.family
        );
        match &r.session {
            Ok(s) => {
                if let Some(d) = &s.done {
                    println!("result: {d}");
                }
                if let Some(f) = &s.failed {
                    ok = false;
                    println!("FAILED: {f}");
                }
                println!(
                    "tool calls: {}  denied: {}  file mutations: {}  tokens: {} in / {} out  ${:.4}",
                    s.stats.tool_calls,
                    s.stats.denied,
                    s.stats.mutations,
                    s.tokens_in,
                    s.tokens_out,
                    s.cost_usd
                );
            }
            Err(e) => {
                ok = false;
                println!("SESSION ERROR: {e}");
            }
        }
        for (n, o) in r.attempts.iter().enumerate() {
            println!("attempt {}: merge {}: {}", n + 1, o.seq(), outcome_text(o));
        }
        match &r.merge {
            Some(o) => ok &= o.is_merged(),
            None => println!("not merged (the session did not finish)"),
        }
    }
    if let Some(ui) = ui
        && !a.has("--ui-exit")
    {
        // The run is over: the dashboard keeps the workspace (rollback)
        // until the user quits.
        ui.attach_workspace(opened.mediator);
        eprintln!("anchor: the run finished; the dashboard stays open. Press Ctrl-C to quit.");
        ui.wait();
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::Merged { merged, .. } => format!("merged ({})", merged.hex()),
        Outcome::Rejected {
            reason, feedback, ..
        } => match reason {
            anchor_backend::queue::RejectKind::Gate => format!(
                "REJECTED by verification\n{}",
                anchor_backend::verify::render_gate_feedback(feedback)
            ),
            r => format!("REJECTED ({}) {feedback}", r.as_str()),
        },
    }
}

pub fn cmd_hub(a: &Args) -> Result<ExitCode, String> {
    let sub = a.pos.get(1).map(String::as_str);
    if sub == Some("serve") {
        return hub_serve(a);
    }
    let (hub, paths) = open_hub(a)?;
    let user = Caller::User;
    let e = |e: anchor_hub::HubError| e.to_string();
    match sub {
        Some("agent") => match a.pos.get(2).map(String::as_str) {
            Some("add") => {
                let within: Vec<&str> = a.all("--claim-within");
                if within.is_empty() {
                    return Err("give at least one --claim-within PATH".into());
                }
                let mut permissions = Permissions::collaborator(&within);
                permissions.messages_need_approval = a.has("--hold-messages");
                let (g, token) = hub
                    .register_agent(
                        user,
                        NewAgent {
                            display_name: a.need("--name")?.into(),
                            model_id: a.need("--model")?.into(),
                            role: a.get("--role").unwrap_or("executor").into(),
                            family: a.need("--family")?.into(),
                            capabilities: serde_json::json!({ "runtime": "external" }),
                            permissions,
                        },
                    )
                    .map_err(e)?;
                agent_line(&g);
                println!(
                    "token (shown once; give it to the agent as `Authorization: Bearer <token>`):\n{token}"
                );
            }
            Some("list") => {
                for g in hub.agents().map_err(e)? {
                    agent_line(&g);
                }
            }
            Some("retire") => {
                hub.retire_agent(user, id_arg(a, 3, "agent id")?)
                    .map_err(e)?;
                println!("retired; its claims are revoked");
            }
            _ => return Err("usage: anchor hub agent add|list|retire".into()),
        },
        Some("claims") => {
            for c in hub.list_claims(user, !a.has("--all")).map_err(e)? {
                println!(
                    "{:>4}  agent {:>3}  {:<7} {:<8} {}{}",
                    c.id,
                    c.agent_id,
                    c.claim_type,
                    c.status,
                    c.path,
                    if c.isolated { " (isolated)" } else { "" }
                );
            }
        }
        Some("messages") => {
            let q = MessageQuery {
                since_id: None,
                room: a.get("--room").map(String::from),
                limit: Some(500),
            };
            for m in hub.query_messages(user, q).map_err(e)? {
                json_line(&m);
            }
        }
        Some("approve") | Some("reject") => {
            let id = id_arg(a, 2, "message id")?;
            let m = if sub == Some("approve") {
                hub.approve_message(user, id)
            } else {
                hub.reject_message(user, id)
            }
            .map_err(e)?;
            json_line(&m);
        }
        Some("decisions") => {
            for d in hub.decisions(user).map_err(e)? {
                json_line(&d);
            }
        }
        Some("decide") => {
            let id = id_arg(a, 2, "decision id")?;
            let accept = match (a.has("--accept"), a.has("--reject")) {
                (true, false) => true,
                (false, true) => false,
                _ => return Err("give exactly one of --accept or --reject".into()),
            };
            let d = hub
                .decide(user, id, accept, a.get("--reason").unwrap_or(""))
                .map_err(e)?;
            json_line(&d);
        }
        Some("tasks") => {
            for t in hub.tasks(user).map_err(e)? {
                json_line(&t);
            }
        }
        Some("artifacts") => {
            // Evidence the verification gate stored (and other artifacts).
            let prefix = a.get("--prefix").map(|p| format!("evidence:{p}"));
            for art in hub.list_artifacts(user, prefix.as_deref()).map_err(e)? {
                println!(
                    "{:>5}  {:<15} {}  {}  {}",
                    art.id,
                    art.kind,
                    &art.sha256[..16.min(art.sha256.len())],
                    art.path,
                    art.evidence
                );
            }
        }
        Some("show") => {
            let path = a.pos.get(2).ok_or("usage: anchor hub show EVIDENCE_PATH")?;
            let art = hub
                .list_artifacts(user, Some(path))
                .map_err(e)?
                .into_iter()
                .find(|x| &x.path == path)
                .ok_or_else(|| format!("no artifact {path}"))?;
            let bytes = hub.read_evidence(user, &art).map_err(e)?;
            print!("{}", String::from_utf8_lossy(&bytes));
        }
        Some("log") => {
            // The full, replayable history of the Hub: every call, refusals
            // included, from the hash-chained audit log.
            for en in AuditLog::read_entries(&paths.audit).map_err(|e| e.to_string())? {
                if en.kind.starts_with("hub_") || en.kind.starts_with("merge_") {
                    json_line(&en);
                }
            }
        }
        Some("verify") => {
            let r = hub.verify().map_err(e)?;
            if r.ok() {
                println!(
                    "hub OK: {} row(s) match the audit log ({} hub entries)",
                    r.rows, r.hub_entries
                );
            } else {
                println!("hub INVALID:");
                for err in &r.errors {
                    println!("  {err}");
                }
                return Ok(ExitCode::FAILURE);
            }
        }
        _ => {
            return Err(
                "usage: anchor hub serve|agent|claims|messages|approve|reject|decisions|decide|tasks|log|verify --workspace DIR"
                    .into(),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn hub_serve(a: &Args) -> Result<ExitCode, String> {
    let (hub, paths) = open_hub(a)?;
    let port = a
        .get("--port")
        .map(|p| p.parse::<u16>().map_err(|e| format!("--port: {e}")))
        .transpose()?
        .unwrap_or(0);
    let h = anchor_hub::http::serve(hub, port, anchor_hub::http::ServerConfig::default())
        .map_err(|e| e.to_string())?;
    let endpoint = format!("http://127.0.0.1:{}", h.port());
    std::fs::write(paths.dir.join("hub.endpoint"), format!("{endpoint}\n"))
        .map_err(|e| e.to_string())?;
    println!("anchor hub listening on {endpoint} (loopback only)");
    println!("  MCP:        POST {endpoint}/mcp   (protocol 2026-07-28)");
    println!("  A2A:        POST {endpoint}/a2a   (A2A-Version: 1.0)");
    println!("  Agent Card: GET  {endpoint}/.well-known/agent-card.json");
    println!("Register agents with `anchor hub agent add`; every request needs its token.");
    session::flush();
    h.wait();
    Ok(ExitCode::SUCCESS)
}

pub fn cmd_queue(a: &Args) -> Result<ExitCode, String> {
    match a.pos.get(1).map(String::as_str) {
        Some("list") => {
            let (hub, _) = open_hub(a)?;
            for m in hub.merges().map_err(|e| e.to_string())? {
                println!(
                    "{:>4}  {:<6} agent {:<4} {:<8} {}",
                    m.seq,
                    m.kind,
                    m.agent_id.map(|i| i.to_string()).unwrap_or("-".into()),
                    m.status,
                    if m.feedback.as_object().is_some_and(|o| o.is_empty()) {
                        String::new()
                    } else {
                        m.feedback.to_string()
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Some("revert") => {
            let ws = PathBuf::from(a.need("--workspace")?);
            let mut opened =
                open_workspace(&ws, None, interaction(a)?, vec![]).map_err(|e| e.to_string())?;
            let ws_root = opened.mediator.workspace().root().to_path_buf();
            let hub = team::open_hub(
                &opened.paths,
                &ws_root,
                opened.mediator.engine().audit_arc(),
            )?;
            let mut q = MergeQueue::new(hub.clone(), &mut opened.mediator, Box::new(NoGate), true);
            let seq = match (a.get("--seq"), a.get("--agent")) {
                (Some(s), None) => s.parse::<i64>().map_err(|e| format!("--seq: {e}"))?,
                (None, Some(who)) => {
                    let id = resolve_agent(&hub, who)?;
                    q.latest_merge_of(id)?
                        .ok_or_else(|| format!("agent {who} has no applied merge"))?
                }
                _ => return Err("give exactly one of --seq N or --agent NAME|ID".into()),
            };
            let row = q.revert(seq)?;
            println!("reverted merge {seq} (revert entry {})", row.seq);
            Ok(ExitCode::SUCCESS)
        }
        _ => Err("usage: anchor queue list|revert --workspace DIR".into()),
    }
}

/// An agent by id, or by name (the most recently registered of that name).
fn resolve_agent(hub: &Hub, who: &str) -> Result<AgentId, String> {
    if let Ok(id) = who.parse::<AgentId>() {
        return Ok(id);
    }
    hub.agents()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|g| g.display_name == who)
        .map(|g| g.id)
        .max()
        .ok_or_else(|| format!("no agent named {who:?}"))
}

fn ui_port(a: &Args) -> Result<u16, String> {
    a.get("--ui-port")
        .map(|p| p.parse::<u16>().map_err(|e| format!("--ui-port: {e}")))
        .transpose()
        .map(|p| p.unwrap_or(0))
}

/// Print the dashboard's URL (it carries the access token), and write it
/// to `--ui-url-file` (owner-only) for scripts.
fn announce_ui(a: &Args, ui: &anchor_backend::ui::Ui) -> Result<(), String> {
    eprintln!("anchor: dashboard at {}", ui.url());
    if let Some(f) = a.get("--ui-url-file") {
        write_secret_file(Path::new(f), format!("{}\n", ui.url()).as_bytes())
            .map_err(|e| format!("{f}: {e}"))?;
    }
    Ok(())
}

/// Write `bytes` to `path` so that only the owner can ever read them: a
/// fresh owner-only file (never an existing one, never through a symlink)
/// renamed over `path`, which replaces whatever was there instead of
/// writing through it (review finding U-1).
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("not a file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let r = o.open(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `anchor ui --workspace DIR`: the dashboard for a workspace, outside a
/// run. If a team run holds the snapshot store, the timeline and rollback
/// are unavailable (that run's own `--ui` dashboard has them).
pub fn cmd_ui(a: &Args) -> Result<ExitCode, String> {
    let ws = PathBuf::from(a.need("--workspace")?);
    let deny = Box::new(anchor_backend::interact::Scripted::new(
        false,
        anchor_protocol::ResumeChoice::FreshSession,
    ));
    let (src, mediator) = match open_workspace(&ws, None, deny, default_exec_read_only()) {
        Ok(opened) => {
            let audit = opened.mediator.engine().audit_arc();
            let root = opened.mediator.workspace().root().to_path_buf();
            let hub = opened
                .paths
                .dir
                .join("hub.sqlite")
                .exists()
                .then(|| team::open_hub(&opened.paths, &root, audit.clone()))
                .transpose()?;
            (
                anchor_backend::ui::Sources {
                    workspace: root,
                    audit,
                    hub,
                    store: Some(opened.mediator.shared_store()),
                    store_note: None,
                    broker: None,
                    mode: "inspect",
                },
                Some(opened.mediator),
            )
        }
        Err(anchor_backend::OpenError::Snapshot(anchor_snapshot::Error::Locked)) => {
            let w = anchor_fs::Workspace::open(&ws).map_err(|e| e.to_string())?;
            let paths =
                StatePaths::create(w.root(), &default_state_base()).map_err(|e| e.to_string())?;
            let audit = Arc::new(AuditLog::open(&paths.audit).map_err(|e| e.to_string())?);
            let hub = paths
                .dir
                .join("hub.sqlite")
                .exists()
                .then(|| team::open_hub(&paths, w.root(), audit.clone()))
                .transpose()?;
            (
                anchor_backend::ui::Sources {
                    workspace: w.root().to_path_buf(),
                    audit,
                    hub,
                    store: None,
                    store_note: Some(
                        "a running team holds the snapshot store; its own dashboard \
                         (`anchor team run --ui`) shows the timeline"
                            .into(),
                    ),
                    broker: None,
                    mode: "inspect",
                },
                None,
            )
        }
        Err(e) => return Err(e.to_string()),
    };
    let port = a
        .get("--port")
        .map(|p| p.parse::<u16>().map_err(|e| format!("--port: {e}")))
        .transpose()?
        .unwrap_or(0);
    let ui = anchor_backend::ui::start(src, port).map_err(|e| format!("dashboard: {e}"))?;
    if let Some(m) = mediator {
        ui.attach_workspace(m);
    }
    announce_ui(a, &ui)?;
    eprintln!("anchor: press Ctrl-C to quit.");
    ui.wait();
    Ok(ExitCode::SUCCESS)
}

/// The models file the app starts with: two local Ollama models
/// (`examples/local/roles.ollama.toml`; a test keeps the copy identical).
const DEFAULT_APP_ROLES: &str = include_str!("../assets/roles.ollama.toml");

/// `anchor app`: the desktop app's backend (ADR-0028). Serves the app on
/// loopback; the user opens projects and runs tasks from the page.
pub fn cmd_app(a: &Args) -> Result<ExitCode, String> {
    let state_base = default_state_base();
    let keys: std::sync::Arc<dyn anchor_backend::inference::keys::KeyStore> = match a.get("--keys")
    {
        None | Some("system") => std::sync::Arc::new(anchor_backend::inference::keys::Keychain),
        // Demos and tests: keys live only as long as the app.
        Some("memory") => {
            std::sync::Arc::new(anchor_backend::inference::keys::MemoryKeys::default())
        }
        Some(other) => return Err(format!("--keys {other}: expected system or memory")),
    };
    let free_url = a
        .get("--free-url")
        .map(str::to_string)
        .or_else(|| std::env::var("ANCHOR_FREE_URL").ok());
    let inference = anchor_backend::inference::Inference::new(keys, free_url)?;
    let roles_path = match a.get("--roles") {
        Some(r) => std::fs::canonicalize(r).map_err(|e| format!("--roles {r}: {e}"))?,
        None => {
            // First run: the default, where the user can find and edit it.
            let dir = state_base.join("app");
            let p = dir.join("roles.toml");
            if !p.exists() {
                std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(&p, DEFAULT_APP_ROLES))
                    .map_err(|e| format!("{}: {e}", p.display()))?;
            }
            p
        }
    };
    let mut ro = default_exec_read_only();
    ro.extend(a.all("--exec-read-only").into_iter().map(PathBuf::from));
    let app = anchor_backend::ui::app::App::new(anchor_backend::ui::app::AppConfig {
        state_base,
        roles_path,
        mock_script: a
            .get("--mock-script")
            .map(|f| std::fs::read_to_string(f).map_err(|e| format!("--mock-script {f}: {e}")))
            .transpose()?,
        exec_read_only: ro,
        agent_bin: a
            .get("--agent")
            .map(PathBuf::from)
            .unwrap_or_else(session::default_agent_bin),
        policy: a.get("--policy").map(PathBuf::from),
        roles_explicit: a.get("--roles").is_some(),
        inference,
    });
    let ui =
        anchor_backend::ui::start_app(app.clone(), ui_port(a)?).map_err(|e| format!("app: {e}"))?;
    announce_ui(a, &ui)?;
    if a.has("--exit-with-stdin") {
        // The desktop shell holds our stdin open; when it goes away, even
        // without a chance to stop us, the pipe closes and so do we.
        std::thread::spawn(move || {
            let mut sink = [0u8; 64];
            let mut stdin = std::io::stdin();
            while std::io::Read::read(&mut stdin, &mut sink).is_ok_and(|n| n > 0) {}
            app.shutdown();
            // Let a cancelled task's runtime be killed and audited.
            std::thread::sleep(std::time::Duration::from_millis(500));
            std::process::exit(0);
        });
    }
    eprintln!("anchor: the app is running. Press Ctrl-C to quit.");
    ui.wait();
    Ok(ExitCode::SUCCESS)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Review finding U-1: the URL (it carries the token) never lands in
    /// a file someone else made or can read, nor through a planted link.
    #[test]
    fn secret_file_replaces_links_and_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("elsewhere");
        std::fs::write(&target, "untouched").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let f = d.path().join("url");
        std::os::unix::fs::symlink(&target, &f).unwrap();
        write_secret_file(&f, b"http://x/?t=secret\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        let md = std::fs::symlink_metadata(&f).unwrap();
        assert!(md.is_file());
        assert_eq!(md.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "http://x/?t=secret\n");
        // An existing world-readable file is replaced, not reused.
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret_file(&f, b"new").unwrap();
        let md = std::fs::symlink_metadata(&f).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o600);
    }
}
