//! `anchor` command-line harness for the backend (Phase 1/2; the desktop
//! shell comes later and uses the same library).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod hubcli;

use anchor_backend::interact::{Interaction, Scripted, Terminal};
use anchor_backend::session::{self, SessionConfig};
use anchor_backend::{StatePaths, default_exec_read_only, default_state_base, open_workspace};
use anchor_checkpoint::CheckpointStore;
use anchor_policy::{AuditLog, Policy};
use anchor_protocol::ResumeChoice;

const USAGE: &str = "\
anchor — trust-first coding agent (backend harness)

USAGE:
  anchor run --workspace DIR --task TEXT --roles FILE [options]
      --policy FILE          use this policy instead of the workspace's policy.toml
      --mock-script FILE     scripted responses for `kind = \"mock\"` providers
      --threshold F          pause at this fraction of the context window (default 0.8)
      --resume ID            seed a fresh session from checkpoint ID
      --auto-approve         answer every prompt `yes` (recorded as automatic)
      --auto-deny            answer every prompt `no`  (recorded as automatic)
      --exec-read-only DIR   extra read-only directory for agent commands (repeatable)
      --agent PATH           path to anchor-agent (default: next to this binary)
  anchor audit verify --workspace DIR
  anchor audit query  --workspace DIR [--kind KIND]
  anchor policy check FILE
  anchor checkpoint list --workspace DIR
  anchor rollback --workspace DIR (--turn N | --snapshot REF|ID) [--yes]
  anchor snapshot list|create|verify --workspace DIR
  anchor snapshot gc --workspace DIR [--keep N]
  anchor mark-not-append-only --workspace DIR PATH
  anchor state-dir --workspace DIR
  anchor detect-local [--agent PATH]

  anchor team run --workspace DIR --team FILE [options]
      --auto-merge           apply merges without asking (still audited)
      --auto-approve | --auto-deny | --policy | --agent | --threshold  as for `run`
  anchor queue list   --workspace DIR
  anchor queue revert --workspace DIR (--seq N | --agent NAME|ID)
  anchor hub serve    --workspace DIR [--port N]     MCP + A2A on 127.0.0.1
  anchor hub agent add --workspace DIR --name N --model M --family F
                       --claim-within PATH... [--role R] [--hold-messages]
  anchor hub agent list|retire ID --workspace DIR
  anchor hub claims [--all] | messages [--room R] | tasks | decisions --workspace DIR
  anchor hub approve|reject MSG_ID --workspace DIR
  anchor hub decide ID (--accept | --reject) [--reason TEXT] --workspace DIR
  anchor hub log | verify --workspace DIR

Per-workspace state (policy, audit log, checkpoints, snapshots) lives outside
the workspace, under $ANCHOR_STATE_DIR or the per-user state directory.
";

pub(crate) struct Args {
    pub(crate) pos: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Result<Self, String> {
        const BOOL: &[&str] = &[
            "--auto-approve",
            "--auto-deny",
            "--yes",
            "--auto-merge",
            "--accept",
            "--reject",
            "--hold-messages",
            "--all",
        ];
        let mut pos = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.into_iter();
        while let Some(a) = it.next() {
            if a.starts_with("--") {
                if BOOL.contains(&a.as_str()) {
                    flags.push((a, None));
                } else {
                    let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                    flags.push((a, Some(v)));
                }
            } else {
                pos.push(a);
            }
        }
        Ok(Args { pos, flags })
    }
    pub(crate) fn get(&self, k: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| f == k)
            .and_then(|(_, v)| v.as_deref())
    }
    pub(crate) fn all(&self, k: &str) -> Vec<&str> {
        self.flags
            .iter()
            .filter(|(f, _)| f == k)
            .filter_map(|(_, v)| v.as_deref())
            .collect()
    }
    pub(crate) fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|(f, _)| f == k)
    }
    pub(crate) fn need(&self, k: &str) -> Result<&str, String> {
        self.get(k).ok_or_else(|| format!("missing {k}"))
    }
}

pub(crate) fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

pub(crate) fn interaction(a: &Args) -> Result<Box<dyn Interaction>, String> {
    match (a.has("--auto-approve"), a.has("--auto-deny")) {
        (true, true) => Err("--auto-approve and --auto-deny are exclusive".into()),
        (true, false) | (false, true) => {
            let mut s = Scripted::new(a.has("--auto-approve"), ResumeChoice::FreshSession);
            s.echo = true;
            Ok(Box::new(s))
        }
        (false, false) => Ok(Box::new(Terminal)),
    }
}

fn cmd_run(a: &Args) -> Result<ExitCode, String> {
    let ws = PathBuf::from(a.need("--workspace")?);
    let roles = read(a.need("--roles")?)?;
    let mut ro = default_exec_read_only();
    ro.extend(a.all("--exec-read-only").into_iter().map(PathBuf::from));
    let mut opened = open_workspace(&ws, a.get("--policy").map(Path::new), interaction(a)?, ro)
        .map_err(|e| e.to_string())?;
    let resume_from = match a.get("--resume") {
        Some(id) => Some(opened.checkpoints.load(id).map_err(|e| e.to_string())?),
        None => None,
    };
    let threshold = a
        .get("--threshold")
        .map(|t| t.parse::<f64>().map_err(|e| format!("--threshold: {e}")))
        .transpose()?
        .unwrap_or(anchor_checkpoint::DEFAULT_THRESHOLD);
    let cfg = SessionConfig {
        agent_bin: a
            .get("--agent")
            .map(PathBuf::from)
            .unwrap_or_else(session::default_agent_bin),
        session_id: session::new_session_id(),
        instruction: a.need("--task")?.to_string(),
        roles_toml: roles,
        checkpoint_threshold: threshold,
        resume_from,
        mock_script: a.get("--mock-script").map(read).transpose()?,
        hub: None,
    };
    eprintln!(
        "anchor: workspace {}",
        opened.mediator.workspace().root().display()
    );
    eprintln!(
        "anchor: session {} (audit log {})",
        cfg.session_id,
        opened.paths.audit.display()
    );
    let report =
        session::run(&cfg, &mut opened.mediator, &opened.checkpoints).map_err(|e| e.to_string())?;
    println!("── session {} finished ──", cfg.session_id);
    if let Some(d) = &report.done {
        println!("result: {d}");
    }
    if let Some(f) = &report.failed {
        println!("FAILED: {f}");
    }
    println!(
        "tool calls: {}  denied: {}  file mutations: {}  commands: {}",
        report.stats.tool_calls, report.stats.denied, report.stats.mutations, report.stats.commands
    );
    println!(
        "tokens: {} in / {} out   estimated cost: ${:.4}",
        report.tokens_in, report.tokens_out, report.cost_usd
    );
    println!("checkpoints: {}", report.checkpoints.len());
    if let Some(p) = &report.paused_at {
        println!("paused at checkpoint {p}; resume with: anchor run ... --resume {p}");
    }
    Ok(if report.failed.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// State paths for `--workspace`, creating the state directory if needed.
fn state_paths(a: &Args) -> Result<StatePaths, String> {
    let ws =
        anchor_fs::Workspace::open(Path::new(a.need("--workspace")?)).map_err(|e| e.to_string())?;
    StatePaths::create(ws.root(), &default_state_base()).map_err(|e| e.to_string())
}

fn cmd_state_dir(a: &Args) -> Result<ExitCode, String> {
    println!("{}", state_paths(a)?.dir.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_audit(a: &Args) -> Result<ExitCode, String> {
    let path = state_paths(a)?.audit;
    match a.pos.get(1).map(String::as_str) {
        Some("verify") => match AuditLog::verify(&path) {
            Ok(n) => {
                println!(
                    "audit log OK: {n} entries, hash chain intact ({})",
                    path.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Err(e) => {
                println!("audit log INVALID: {e}");
                Ok(ExitCode::FAILURE)
            }
        },
        Some("query") => {
            use std::io::Write;
            let kind = a.get("--kind");
            let mut out = std::io::stdout().lock();
            for e in AuditLog::read_entries(&path).map_err(|e| e.to_string())? {
                if kind.is_none_or(|k| k == e.kind)
                    && writeln!(out, "{}", serde_json::to_string(&e).unwrap_or_default()).is_err()
                {
                    break; // reader went away (e.g. `| head`)
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => Err("usage: anchor audit verify|query --workspace DIR".into()),
    }
}

fn cmd_policy(a: &Args) -> Result<ExitCode, String> {
    let file = a.pos.get(2).ok_or("usage: anchor policy check FILE")?;
    match Policy::parse(&read(file)?, &anchor_backend::home_dir()) {
        Ok(p) => {
            println!(
                "policy OK: {} rules, default {:?}",
                p.rules().len(),
                p.default_action()
            );
            for r in p.rules() {
                println!("  {:<40} {:?}", r.name, r.action);
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("policy INVALID: {e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn cmd_checkpoint(a: &Args) -> Result<ExitCode, String> {
    let store = CheckpointStore::open(&state_paths(a)?.checkpoints).map_err(|e| e.to_string())?;
    let (ok, bad) = store.list().map_err(|e| e.to_string())?;
    for c in ok {
        println!(
            "{}\tsession {}\tturn {}",
            c.checkpoint_id, c.session_id, c.turn
        );
    }
    for (p, e) in bad {
        println!("INVALID {}: {e}", p.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn open_cli(a: &Args) -> Result<anchor_backend::Opened, String> {
    let ws = PathBuf::from(a.need("--workspace")?);
    let opened =
        open_workspace(&ws, None, Box::new(Terminal), vec![]).map_err(|e| e.to_string())?;
    if let Some(r) = &opened.recovered {
        eprintln!(
            "anchor: finished an interrupted restore: the workspace is back to its state before it ({} operation(s))",
            r.ops.len()
        );
    }
    Ok(opened)
}

/// A snapshot named by ref (`turns/0000000003`, `HEAD`) or full id.
fn resolve_snapshot(
    store: &anchor_snapshot::Store,
    s: &str,
) -> Result<anchor_snapshot::Id, String> {
    if let Some(id) = anchor_snapshot::Id::parse(s) {
        return Ok(id);
    }
    store
        .get_ref(s)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no snapshot named {s:?}"))
}

fn cmd_rollback(a: &Args) -> Result<ExitCode, String> {
    let mut opened = open_cli(a)?;
    let m = &mut opened.mediator;
    let (target, what) = match (a.get("--turn"), a.get("--snapshot")) {
        (Some(t), None) => {
            let turn: u64 = t.parse().map_err(|e| format!("--turn: {e}"))?;
            (m.snapshot_for_turn(turn)?, format!("before turn {turn}"))
        }
        (None, Some(s)) => (
            resolve_snapshot(&m.snapshots(), s)?,
            format!("snapshot {s}"),
        ),
        _ => return Err("give exactly one of --turn N or --snapshot REF".into()),
    };
    let changes = m.preview_restore(&target)?;
    if changes.is_empty() {
        println!("the workspace already matches the state {what}");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Rolling back to the state {what} changes {} path(s):",
        changes.len()
    );
    for c in changes.iter().take(50) {
        println!("  {c}");
    }
    if changes.len() > 50 {
        println!("  ... and {} more", changes.len() - 50);
    }
    if !a.has("--yes") {
        eprint!("Proceed? The current state is kept and can be restored. [y/N] ");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        if s.trim() != "y" {
            println!("aborted");
            return Ok(ExitCode::FAILURE);
        }
    }
    let log = m.rollback_to_snapshot(&target)?;
    for l in &log {
        println!("{l}");
    }
    println!("rollback complete and verified: {} operation(s)", log.len());
    Ok(ExitCode::SUCCESS)
}

fn cmd_snapshot(a: &Args) -> Result<ExitCode, String> {
    let opened = open_cli(a)?;
    let guard = opened.mediator.snapshots();
    let store = &*guard;
    match a.pos.get(1).map(String::as_str) {
        Some("list") => {
            for (name, id) in store.all_refs().map_err(|e| e.to_string())? {
                match store.snapshot(&id) {
                    Ok(s) => println!(
                        "{name:<24} {}  {:>7} files  {}",
                        &id.hex()[..16],
                        s.files,
                        s.label
                    ),
                    Err(e) => println!("{name:<24} {}  INVALID: {e}", &id.hex()[..16]),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some("create") => {
            let ws = opened.mediator.workspace();
            let r = anchor_snapshot::take_snapshot(
                store,
                ws,
                &anchor_snapshot::SnapshotOptions::default(),
                &format!(
                    "manual/{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                ),
                "manual",
            )
            .map_err(|e| e.to_string())?;
            println!(
                "snapshot {} : {} files, {} re-hashed, {} new objects, {} ms",
                r.id().hex(),
                r.files,
                r.hashed_files,
                r.new_objects,
                r.total_ms
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("verify") => {
            let v = anchor_snapshot::verify(store).map_err(|e| e.to_string())?;
            for e in &v.errors {
                println!("ERROR {e}");
            }
            println!(
                "snapshot store {}: {} refs, {} objects checked",
                if v.ok() { "OK" } else { "DAMAGED" },
                v.refs,
                v.objects
            );
            Ok(if v.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Some("gc") => {
            let keep = a
                .get("--keep")
                .map(|k| k.parse::<usize>().map_err(|e| format!("--keep: {e}")))
                .transpose()?
                .unwrap_or(200);
            let g = anchor_snapshot::gc(store, keep, 20).map_err(|e| e.to_string())?;
            println!(
                "gc: {} ref(s) pruned, {} object(s) removed, {} bytes freed",
                g.refs_pruned, g.objects_removed, g.bytes_freed
            );
            Ok(ExitCode::SUCCESS)
        }
        _ => Err("usage: anchor snapshot list|create|verify|gc --workspace DIR".into()),
    }
}

fn cmd_mark(a: &Args) -> Result<ExitCode, String> {
    let rel = a
        .pos
        .get(1)
        .ok_or("usage: anchor mark-not-append-only --workspace DIR PATH")?;
    let mut opened = open_cli(a)?;
    let r = opened
        .mediator
        .workspace()
        .confine(rel)
        .map_err(|e| e.to_string())?;
    let st = opened.mediator.mark_not_append_only(&r)?;
    println!(
        "{}: now {:?} (override persisted in {})",
        r.display(),
        st,
        opened.paths.workspace_config.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_detect(a: &Args) -> Result<ExitCode, String> {
    let agent = a
        .get("--agent")
        .map(PathBuf::from)
        .unwrap_or_else(session::default_agent_bin);
    let out = std::process::Command::new(agent)
        .arg("--detect-local")
        .output()
        .map_err(|e| e.to_string())?;
    let s = String::from_utf8_lossy(&out.stdout);
    if s.trim().is_empty() {
        println!("no OpenAI-compatible local inference server found on the probed ports");
    } else {
        print!("{s}");
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let a = match Args::parse(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("anchor: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let r = match a.pos.first().map(String::as_str) {
        Some("run") => cmd_run(&a),
        Some("audit") => cmd_audit(&a),
        Some("policy") => cmd_policy(&a),
        Some("checkpoint") => cmd_checkpoint(&a),
        Some("rollback") => cmd_rollback(&a),
        Some("snapshot") => cmd_snapshot(&a),
        Some("mark-not-append-only") => cmd_mark(&a),
        Some("state-dir") => cmd_state_dir(&a),
        Some("detect-local") => cmd_detect(&a),
        Some("team") => hubcli::cmd_team(&a),
        Some("hub") => hubcli::cmd_hub(&a),
        Some("queue") => hubcli::cmd_queue(&a),
        _ => {
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(c) => c,
        Err(e) => {
            eprintln!("anchor: {e}");
            ExitCode::FAILURE
        }
    }
}
