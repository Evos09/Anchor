//! Runs one agent session: spawns the runtime as a separate OS process,
//! speaks the NDJSON protocol with it, and routes every request through the
//! [`Mediator`].

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use anchor_checkpoint::CheckpointStore;
use anchor_protocol::{
    Checkpoint, CheckpointSaved, Frame, Message, PROTOCOL_VERSION, Resume, Task, ToolResult,
    Validate, Welcome, read_frame, write_frame,
};

use crate::mediator::{Mediator, Stats};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub agent_bin: PathBuf,
    pub session_id: String,
    pub instruction: String,
    pub roles_toml: String,
    pub checkpoint_threshold: f64,
    pub resume_from: Option<Checkpoint>,
    pub mock_script: Option<String>,
    /// Multi-agent runs: the agent's Hub identity and tools.
    pub hub: Option<anchor_protocol::HubInfo>,
    /// Review sessions (Phase 5): what the reviewer judges. The runtime
    /// gets no workspace tools; any tool call ends the session.
    pub review: Option<anchor_protocol::verify::ReviewInput>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    pub done: Option<String>,
    /// The executor's claims, reported with `Done`.
    pub claims: Vec<anchor_protocol::verify::Claim>,
    /// A review session's verdicts.
    pub verdicts: Option<Vec<anchor_protocol::verify::ClaimVerdict>>,
    /// Routing and fallback notices, in order (the verify gate re-checks
    /// the families a reviewer actually ran on).
    pub notices: Vec<anchor_protocol::Notice>,
    pub failed: Option<String>,
    pub paused_at: Option<String>,
    pub checkpoints: Vec<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub stats: Stats,
    pub runtime_pid: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("failed to start agent runtime {0}: {1}")]
    Spawn(PathBuf, std::io::Error),
    #[error("protocol violation by runtime: {0}")]
    Protocol(String),
    #[error("runtime exited unexpectedly")]
    RuntimeExited,
    #[error("checkpoint store: {0}")]
    Checkpoint(String),
}

pub fn new_session_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("s{secs}-{}", std::process::id())
}

/// Environment passed to the runtime: proxy/TLS settings and the API key
/// variables named in the roles config. Nothing else.
fn runtime_env(roles_toml: &str) -> Vec<(String, String)> {
    let mut names: Vec<String> = [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Ok(v) = roles_toml.parse::<toml::Table>()
        && let Some(providers) = v.get("providers").and_then(|p| p.as_table())
    {
        for p in providers.values() {
            if let Some(k) = p.get("api_key_env").and_then(|k| k.as_str())
                && !k.is_empty()
            {
                names.push(k.to_string());
            }
        }
    }
    names
        .into_iter()
        .filter_map(|n| std::env::var(&n).ok().map(|v| (n, v)))
        .collect()
}

struct Wire {
    child: Child,
    stdin: ChildStdin,
    next_id: u64,
}

impl Wire {
    fn send(&mut self, msg: Message) -> Result<(), SessionError> {
        let f = Frame::new(self.next_id, msg);
        self.next_id += 1;
        write_frame(&mut self.stdin, &f).map_err(|e| SessionError::Protocol(e.to_string()))
    }
}

/// Run a session to completion.
pub fn run(
    cfg: &SessionConfig,
    mediator: &mut Mediator,
    checkpoints: &CheckpointStore,
) -> Result<SessionReport, SessionError> {
    mediator.set_session(&cfg.session_id);
    let mut child = Command::new(&cfg.agent_bin)
        .env_clear()
        .envs(runtime_env(&cfg.roles_toml))
        // The runtime never needs the workspace as its cwd.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SessionError::Spawn(cfg.agent_bin.clone(), e))?;
    let stdout = child.stdout.take().ok_or(SessionError::RuntimeExited)?;
    let stderr = child.stderr.take().ok_or(SessionError::RuntimeExited)?;
    let stdin = child.stdin.take().ok_or(SessionError::RuntimeExited)?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut report = SessionReport {
        runtime_pid: child.id(),
        ..Default::default()
    };
    let mut wire = Wire {
        child,
        stdin,
        next_id: 1,
    };
    let mut reader = BufReader::new(stdout);
    // A team agent stays alive on the Hub while its runtime runs, however
    // long one model turn takes.
    let ticker = mediator.hub().cloned().map(heartbeat_ticker);
    let audit = |m: &Mediator, kind: &str, data: serde_json::Value| {
        let _ = m.log(kind, data);
    };
    audit(
        mediator,
        "session_started",
        serde_json::json!({
            "session_id": cfg.session_id,
            "runtime_pid": report.runtime_pid,
            "review": cfg.review.is_some(),
        }),
    );

    let result = (|| -> Result<(), SessionError> {
        loop {
            while let Ok(line) = rx.try_recv() {
                mediator.interact().notify(&format!("[runtime] {line}"));
            }
            let frame = match read_frame(&mut reader) {
                Ok(Some(f)) => f,
                Ok(None) => return Err(SessionError::RuntimeExited),
                Err(e) => return Err(SessionError::Protocol(e.to_string())),
            };
            match frame.msg {
                Message::Hello(h) => {
                    if h.protocol_version != PROTOCOL_VERSION {
                        return Err(SessionError::Protocol(format!(
                            "runtime speaks protocol {}",
                            h.protocol_version
                        )));
                    }
                    wire.send(Message::Welcome(Welcome {
                        session_id: cfg.session_id.clone(),
                        protocol_version: PROTOCOL_VERSION,
                    }))?;
                    wire.send(Message::Task(Box::new(Task {
                        session_id: cfg.session_id.clone(),
                        instruction: cfg.instruction.clone(),
                        roles_toml: cfg.roles_toml.clone(),
                        checkpoint_threshold: cfg.checkpoint_threshold,
                        resume_from: cfg.resume_from.clone().map(Box::new),
                        mock_script: cfg.mock_script.clone(),
                        hub: cfg.hub.clone(),
                        review: cfg.review.clone().map(Box::new),
                    })))?;
                }
                Message::TurnStarted { turn } => {
                    mediator.set_turn(turn);
                    audit(
                        mediator,
                        "turn_started",
                        serde_json::json!({ "turn": turn }),
                    );
                    mediator.interact().notify(&format!("── turn {turn} ──"));
                }
                Message::TurnFinished { turn } => {
                    audit(
                        mediator,
                        "turn_finished",
                        serde_json::json!({ "turn": turn }),
                    );
                }
                Message::ToolCall(call) if cfg.review.is_some() => {
                    // A reviewer has no workspace access (ADR-0009): fail
                    // closed rather than answer.
                    audit(
                        mediator,
                        "review_tool_refused",
                        serde_json::json!({ "tool": call.request.name() }),
                    );
                    return Err(SessionError::Protocol(format!(
                        "review session requested tool {}",
                        call.request.name()
                    )));
                }
                Message::ToolCall(call) => {
                    let name = call.request.name();
                    let outcome = mediator.handle(call.request);
                    let short = match &outcome {
                        anchor_protocol::ToolOutcome::Ok { .. } => "ok".to_string(),
                        anchor_protocol::ToolOutcome::Denied { by, detail } => {
                            format!("DENIED by {by:?}: {detail}")
                        }
                        anchor_protocol::ToolOutcome::Error { detail } => {
                            format!("error: {detail}")
                        }
                    };
                    mediator.interact().notify(&format!("  {name} -> {short}"));
                    wire.send(Message::ToolResult(ToolResult {
                        call_id: call.call_id,
                        outcome,
                    }))?;
                }
                Message::Checkpoint(cp) => {
                    if cp.session_id != cfg.session_id {
                        return Err(SessionError::Protocol(
                            "checkpoint for another session".into(),
                        ));
                    }
                    cp.validate().map_err(SessionError::Protocol)?;
                    let path = checkpoints
                        .save(&cp)
                        .map_err(|e| SessionError::Checkpoint(e.to_string()))?;
                    audit(
                        mediator,
                        "checkpoint_saved",
                        serde_json::json!({ "checkpoint_id": cp.checkpoint_id, "reason": cp.reason, "path": path }),
                    );
                    report.checkpoints.push(cp.checkpoint_id.clone());
                    wire.send(Message::CheckpointSaved(CheckpointSaved {
                        checkpoint_id: cp.checkpoint_id,
                    }))?;
                }
                Message::ContextUsage(u) => {
                    mediator.interact().notify(&format!(
                        "  context: {}/{} tokens ({:.0}%) on {}",
                        u.used_tokens,
                        u.window_tokens,
                        100.0 * u.used_tokens as f64 / u.window_tokens as f64,
                        u.model_id
                    ));
                }
                Message::Usage(u) => {
                    report.tokens_in += u.input_tokens;
                    report.tokens_out += u.output_tokens;
                    report.cost_usd += u.cost_usd;
                    audit(
                        mediator,
                        "usage",
                        serde_json::to_value(&u).unwrap_or_default(),
                    );
                    mediator.interact().notify(&format!(
                        "  cost: {} in / {} out tokens via {} ({}), ${:.4} this call, ${:.4} session",
                        u.input_tokens, u.output_tokens, u.model_id, u.role, u.cost_usd, report.cost_usd
                    ));
                }
                Message::Notice(n) => {
                    report.notices.push(n.clone());
                    audit(
                        mediator,
                        "notice",
                        serde_json::to_value(&n).unwrap_or_default(),
                    );
                    mediator
                        .interact()
                        .notify(&format!("  notice ({:?}): {}", n.kind, n.detail));
                }
                Message::Paused(p) => {
                    report.paused_at = Some(p.checkpoint_id.clone());
                    let choice = mediator
                        .interact()
                        .resume_choice(&p.checkpoint_id, "the configured threshold");
                    audit(
                        mediator,
                        "pause_resolved",
                        serde_json::json!({ "checkpoint_id": p.checkpoint_id, "choice": choice }),
                    );
                    wire.send(Message::Resume(Resume { choice }))?;
                }
                Message::Verdicts(v) if cfg.review.is_some() && report.verdicts.is_none() => {
                    audit(
                        mediator,
                        "review_verdicts",
                        serde_json::json!({ "verdicts": v.verdicts }),
                    );
                    report.verdicts = Some(v.verdicts);
                }
                Message::Done(d) => {
                    if !d.claims.is_empty() {
                        audit(
                            mediator,
                            "claims_submitted",
                            serde_json::json!({ "claims": d.claims }),
                        );
                    }
                    report.claims = d.claims;
                    report.done = Some(d.summary);
                    wire.send(Message::Shutdown)?;
                    return Ok(());
                }
                Message::Failed(f) => {
                    report.failed = Some(f.error);
                    wire.send(Message::Shutdown)?;
                    return Ok(());
                }
                other => {
                    return Err(SessionError::Protocol(format!(
                        "runtime sent a backend-only message: {other:?}"
                    )));
                }
            }
        }
    })();

    if result.is_err() {
        let _ = wire.child.kill();
    }
    if let Some((stop, t)) = ticker {
        drop(stop);
        let _ = t.join();
    }
    let _ = wire.child.wait();
    let _ = stderr_thread.join();
    while let Ok(line) = rx.try_recv() {
        mediator.interact().notify(&format!("[runtime] {line}"));
    }
    report.stats = mediator.stats.clone();
    audit(
        mediator,
        "session_finished",
        serde_json::json!({
            "session_id": cfg.session_id,
            "done": report.done.is_some(),
            "failed": report.failed,
            "error": result.as_ref().err().map(|e| e.to_string()),
            "tokens_in": report.tokens_in,
            "tokens_out": report.tokens_out,
            "cost_usd": report.cost_usd,
        }),
    );
    result.map(|_| report)
}

/// Heartbeat `b.agent` every third of the Hub's interval until the
/// returned sender is dropped.
fn heartbeat_ticker(
    b: crate::mediator::HubBinding,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (stop, rx) = std::sync::mpsc::channel::<()>();
    let every = std::time::Duration::from_millis((b.hub.config().heartbeat_ms / 3).max(10));
    let t = std::thread::spawn(move || {
        let caller = anchor_hub::Caller::Agent(b.agent);
        loop {
            let _ = b.hub.heartbeat(caller);
            match rx.recv_timeout(every) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                _ => return,
            }
        }
    });
    (stop, t)
}

/// Locate `anchor-agent` next to the current executable.
pub fn default_agent_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = if cfg!(windows) {
        "anchor-agent.exe"
    } else {
        "anchor-agent"
    };
    let direct = dir.join(name);
    if direct.exists() {
        return direct;
    }
    // `cargo test` puts test binaries in target/<profile>/deps.
    dir.parent().map(|p| p.join(name)).unwrap_or(direct)
}

/// Flush helper for CLI output.
pub fn flush() {
    let _ = std::io::stdout().flush();
}
