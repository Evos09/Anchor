//! Runs one agent session: spawns the runtime as a separate OS process,
//! speaks the NDJSON protocol with it, and routes every request through the
//! [`Mediator`].

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

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
    /// With `resume_from`: the user's follow-up message, which the runtime
    /// adds to the resumed conversation (0.12.0).
    pub continue_with: Option<String>,
    /// Secrets for the runtime's environment (the app's BYOK keys or
    /// free-tier token, named by the models file's `api_key_env`), on
    /// top of what [`runtime_env`] passes through. Never logged.
    pub extra_env: crate::inference::keys::SecretEnv,
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

/// `s` cut to at most `max` bytes, on a character boundary.
fn cap(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
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

/// Providers (and proxies) on the loopback interface: an AppContainer can
/// never reach them, so on Windows such a session's runtime runs
/// unconfined, warned and audited (AR-7, ADR-0026).
pub fn loopback_endpoints(roles_toml: &str, env: &[(String, String)]) -> Vec<String> {
    fn is_loopback(url: &str) -> bool {
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = if let Some(v6) = host_port.strip_prefix('[') {
            v6.split(']').next().unwrap_or("")
        } else {
            host_port.split(':').next().unwrap_or("")
        };
        let host = host.to_ascii_lowercase();
        host == "localhost"
            || host.ends_with(".localhost")
            || host == "::1"
            || host == "0.0.0.0"
            || host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|a| a.is_loopback())
    }
    let mut found = Vec::new();
    if let Ok(v) = roles_toml.parse::<toml::Table>()
        && let Some(providers) = v.get("providers").and_then(|p| p.as_table())
    {
        for (name, p) in providers {
            if let Some(u) = p.get("base_url").and_then(|u| u.as_str())
                && is_loopback(u)
            {
                found.push(format!("provider {name} ({u})"));
            }
        }
    }
    for (k, v) in env {
        if k.to_ascii_lowercase().ends_with("_proxy")
            && !k.eq_ignore_ascii_case("no_proxy")
            && is_loopback(v)
        {
            found.push(format!("{k} ({v})"));
        }
    }
    found
}

/// The runtime process, and how it was started.
pub struct RuntimeProcess {
    pub stdin: Option<Box<dyn Write + Send>>,
    pub stdout: Option<Box<dyn Read + Send>>,
    pub stderr: Option<Box<dyn Read + Send>>,
    /// Started inside an AppContainer by the backend (Windows). On Linux
    /// and macOS the runtime confines itself, so this stays false there.
    pub isolated: bool,
    child: RuntimeChild,
}

impl std::fmt::Debug for RuntimeProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeProcess")
            .field("pid", &self.id())
            .field("isolated", &self.isolated)
            .finish()
    }
}

enum RuntimeChild {
    Plain(Child),
    #[cfg(windows)]
    Isolated(anchor_exec::IsolatedChild),
}

impl RuntimeProcess {
    pub fn id(&self) -> u32 {
        match &self.child {
            RuntimeChild::Plain(c) => c.id(),
            #[cfg(windows)]
            RuntimeChild::Isolated(c) => c.id(),
        }
    }

    pub fn kill(&mut self) {
        match &mut self.child {
            RuntimeChild::Plain(c) => {
                let _ = c.kill();
            }
            #[cfg(windows)]
            RuntimeChild::Isolated(c) => {
                let _ = c.kill();
            }
        }
    }

    pub fn wait(&mut self) {
        match &mut self.child {
            RuntimeChild::Plain(c) => {
                let _ = c.wait();
            }
            #[cfg(windows)]
            RuntimeChild::Isolated(c) => {
                let _ = c.wait();
            }
        }
    }
}

/// Start `anchor-agent` with `args` and exactly `env`. On Windows it runs
/// in its own AppContainer (`internetClient` only, no file grants) unless
/// `unconfined` (a loopback provider; see [`loopback_endpoints`]). On
/// Linux and macOS it sandboxes itself before reading its first frame.
pub fn spawn_runtime(
    agent_bin: &Path,
    args: &[String],
    env: Vec<(String, String)>,
    unconfined: bool,
) -> std::io::Result<RuntimeProcess> {
    #[cfg(windows)]
    if !unconfined {
        let spec = anchor_exec::IsolatedSpec {
            program: agent_bin.to_path_buf(),
            args: args.to_vec(),
            env,
            profile: "anchor.runtime".into(),
            internet: true,
        };
        let mut c = anchor_exec::spawn_isolated(&spec).map_err(std::io::Error::other)?;
        return Ok(RuntimeProcess {
            stdin: c.stdin.take().map(|f| Box::new(f) as Box<dyn Write + Send>),
            stdout: c.stdout.take().map(|f| Box::new(f) as Box<dyn Read + Send>),
            stderr: c.stderr.take().map(|f| Box::new(f) as Box<dyn Read + Send>),
            isolated: true,
            child: RuntimeChild::Isolated(c),
        });
    }
    let _ = unconfined;
    let mut cmd = Command::new(agent_bin);
    // Its own process group: `kill(0, ..)` from the runtime then reaches
    // only itself, never the backend's group (round 2 finding K-2).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut c = cmd
        .args(args)
        .env_clear()
        // Windows needs SystemRoot and friends even to open a socket
        // (os error 10106 without them; the AppContainer path adds the
        // same variables).
        .envs(anchor_exec::with_os_env(env))
        // The runtime never needs the workspace as its cwd.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    Ok(RuntimeProcess {
        stdin: c.stdin.take().map(|f| Box::new(f) as Box<dyn Write + Send>),
        stdout: c.stdout.take().map(|f| Box::new(f) as Box<dyn Read + Send>),
        stderr: c.stderr.take().map(|f| Box::new(f) as Box<dyn Read + Send>),
        isolated: false,
        child: RuntimeChild::Plain(c),
    })
}

/// Checkpoints kept per session (the newest).
const CHECKPOINTS_KEPT: usize = 8;

/// Runtime stderr lines held for display at most, and the longest line
/// shown; the rest is dropped, never buffered (review finding T-5).
const STDERR_QUEUE: usize = 256;
const STDERR_LINE: usize = 4096;

/// Forward the runtime's stderr line by line, bounded in line length and
/// in lines queued: a runtime that floods stderr loses lines, it cannot
/// grow the backend's memory or stall on a full pipe.
fn forward_stderr(stderr: impl std::io::Read, tx: &std::sync::mpsc::SyncSender<String>) {
    use std::sync::mpsc::TrySendError;
    let mut r = BufReader::new(stderr);
    let mut line = Vec::new();
    let mut dropped = 0u64;
    loop {
        line.clear();
        let mut cut = false;
        // Read one line, keeping at most STDERR_LINE bytes of it.
        let eof = loop {
            let buf = match r.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break true,
            };
            if buf.is_empty() {
                break true;
            }
            let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (&buf[..i], Some(i + 1)),
                None => (buf, None),
            };
            let room = STDERR_LINE.saturating_sub(line.len());
            cut |= chunk.len() > room;
            line.extend_from_slice(&chunk[..chunk.len().min(room)]);
            let n = done.unwrap_or(buf.len());
            r.consume(n);
            if done.is_some() {
                break false;
            }
        };
        if eof && line.is_empty() {
            break;
        }
        let mut text = String::from_utf8_lossy(&line).into_owned();
        if cut {
            text.push_str(" [line truncated]");
        }
        if dropped > 0 {
            match tx.try_send(format!("[{dropped} runtime stderr lines dropped]")) {
                Ok(()) => dropped = 0,
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        match tx.try_send(text) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => dropped += 1,
            Err(TrySendError::Disconnected(_)) => break,
        }
        if eof {
            break;
        }
    }
}

/// Stops a running session from another thread (the desktop app's Stop
/// button): kills the runtime, and the session ends as failed with
/// [`CANCELLED`]. Pending approvals are the caller's to refuse (the
/// broker's `cancel_pending`).
#[derive(Debug, Default)]
pub struct Cancel {
    state: std::sync::Mutex<CancelState>,
}

#[derive(Debug, Default)]
struct CancelState {
    cancelled: bool,
    /// Every runtime started under this cancel (two builders run at once).
    children: Vec<Arc<std::sync::Mutex<RuntimeProcess>>>,
}

/// How a cancelled session's failure reads.
pub const CANCELLED: &str = "cancelled by the user";

impl Cancel {
    pub fn new() -> Arc<Cancel> {
        Arc::new(Cancel::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CancelState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stop the session: kill its runtime now, or as soon as it starts.
    pub fn cancel(&self) {
        let children = {
            let mut st = self.lock();
            st.cancelled = true;
            st.children.clone()
        };
        // Killing a runtime that already exited is a no-op.
        for c in children {
            c.lock().unwrap_or_else(|p| p.into_inner()).kill();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Ready for the next session.
    pub fn reset(&self) {
        let mut st = self.lock();
        st.cancelled = false;
        st.children.clear();
    }

    fn register(&self, c: &Arc<std::sync::Mutex<RuntimeProcess>>) {
        let cancelled = {
            let mut st = self.lock();
            st.children.push(c.clone());
            st.cancelled
        };
        if cancelled {
            c.lock().unwrap_or_else(|p| p.into_inner()).kill();
        }
    }
}

struct Wire {
    child: Arc<std::sync::Mutex<RuntimeProcess>>,
    stdin: Box<dyn Write + Send>,
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
    run_cancellable(cfg, mediator, checkpoints, None)
}

/// [`run`], stoppable from another thread through `cancel`.
pub fn run_cancellable(
    cfg: &SessionConfig,
    mediator: &mut Mediator,
    checkpoints: &CheckpointStore,
    cancel: Option<&Cancel>,
) -> Result<SessionReport, SessionError> {
    mediator.set_session(&cfg.session_id);
    let mut env = runtime_env(&cfg.roles_toml);
    env.extend(cfg.extra_env.0.iter().cloned());
    // On Windows a loopback provider cannot be reached from an
    // AppContainer: that runtime runs unconfined, said and audited (AR-7).
    let loopback = if cfg!(windows) {
        loopback_endpoints(&cfg.roles_toml, &env)
    } else {
        Vec::new()
    };
    let mut child = spawn_runtime(&cfg.agent_bin, &[], env, !loopback.is_empty())
        .map_err(|e| SessionError::Spawn(cfg.agent_bin.clone(), e))?;
    if !loopback.is_empty() {
        mediator.interact().notify(&format!(
            "warning: the agent runtime runs without its sandbox, because a Windows \
             AppContainer cannot reach loopback endpoints: {}",
            loopback.join(", ")
        ));
        let _ = mediator.log(
            "runtime_unconfined",
            serde_json::json!({
                "session_id": cfg.session_id,
                "reason": "loopback_endpoint",
                "endpoints": loopback,
            }),
        );
    }
    let stdout = child.stdout.take().ok_or(SessionError::RuntimeExited)?;
    let stderr = child.stderr.take().ok_or(SessionError::RuntimeExited)?;
    let stdin = child.stdin.take().ok_or(SessionError::RuntimeExited)?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(STDERR_QUEUE);
    let stderr_thread = std::thread::spawn(move || forward_stderr(stderr, &tx));
    let mut report = SessionReport {
        runtime_pid: child.id(),
        ..Default::default()
    };
    let isolated = child.isolated;
    let child = Arc::new(std::sync::Mutex::new(child));
    if let Some(c) = cancel {
        c.register(&child);
    }
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
            "runtime_isolated": isolated,
            "review": cfg.review.is_some(),
            "resumed_from": cfg.resume_from.as_ref().map(|c| &c.checkpoint_id),
            "follow_up": cfg.continue_with.is_some(),
        }),
    );

    let mut last_turn = 0u64;
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
                        continue_with: cfg.continue_with.clone(),
                    })))?;
                }
                Message::TurnStarted { turn } => {
                    // Rollback finds a turn by its number, so a runtime may
                    // not reuse or reorder them (review finding T-4).
                    if turn <= last_turn {
                        return Err(SessionError::Protocol(format!(
                            "turn {turn} started after turn {last_turn}: turn numbers must increase"
                        )));
                    }
                    last_turn = turn;
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
                    // Only the latest few are ever resumed from; a runtime
                    // cannot fill the disk with them (review finding T-6).
                    checkpoints
                        .prune(&cfg.session_id, CHECKPOINTS_KEPT)
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

    let child = |w: &Wire| w.child.lock().unwrap_or_else(|p| p.into_inner()).kill();
    if result.is_err() {
        child(&wire);
    }
    // A cancelled session ends as failed, whatever the dying runtime's
    // pipe made of the turn in flight.
    let result = match cancel {
        Some(c) if c.is_cancelled() => {
            child(&wire);
            report.failed = Some(CANCELLED.into());
            Ok(())
        }
        _ => result,
    };
    if let Some((stop, t)) = ticker {
        drop(stop);
        let _ = t.join();
    }
    wire.child.lock().unwrap_or_else(|p| p.into_inner()).wait();
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
            "result": report.done.as_deref().map(|d| cap(d, 8000)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding T-5: a flood of runtime stderr is cut to bounded
    /// lines and a bounded queue; nothing grows with the input.
    #[test]
    fn runtime_stderr_is_bounded() {
        let mut input = vec![b'x'; 3 * STDERR_LINE];
        input.extend_from_slice(b"\nshort\n");
        for i in 0..(STDERR_QUEUE * 4) {
            input.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        input.extend_from_slice(b"no newline at end");
        let (tx, rx) = std::sync::mpsc::sync_channel(STDERR_QUEUE);
        forward_stderr(&input[..], &tx);
        drop(tx);
        let got: Vec<String> = rx.iter().collect();
        assert_eq!(got.len(), STDERR_QUEUE);
        assert!(got[0].ends_with(" [line truncated]"));
        assert!(got[0].len() <= STDERR_LINE + 20);
        assert_eq!(got[1], "short");

        // Drained as it goes, nothing is lost and the final partial line
        // arrives.
        let (tx, rx) = std::sync::mpsc::sync_channel(STDERR_QUEUE);
        let t = std::thread::spawn(move || rx.iter().collect::<Vec<String>>());
        let small = b"a\nb\nc";
        forward_stderr(&small[..], &tx);
        drop(tx);
        assert_eq!(t.join().unwrap(), ["a", "b", "c"]);
    }
}

#[cfg(test)]
mod loopback_tests {
    use super::loopback_endpoints;

    /// Which runtimes Windows must leave outside the AppContainer: those
    /// with a provider or proxy on loopback, and only those (AR-7).
    #[test]
    fn loopback_endpoints_are_found() {
        let roles = r#"
[providers.local]
kind = "openai_compatible"
base_url = "http://127.0.0.1:11434/v1"
[providers.other]
kind = "openai_compatible"
base_url = "http://LOCALHOST:8080/v1"
[providers.v6]
kind = "openai_compatible"
base_url = "http://[::1]:1234/v1"
[providers.hosted]
kind = "openai_compatible"
base_url = "https://api.example.com/v1"
[providers.tricky]
kind = "openai_compatible"
base_url = "https://user@127.0.0.1.example.com/v1"
"#;
        let env = vec![
            (
                "HTTPS_PROXY".to_string(),
                "http://127.0.0.2:3128".to_string(),
            ),
            ("NO_PROXY".to_string(), "localhost".to_string()),
            (
                "http_proxy".to_string(),
                "http://proxy.corp:3128".to_string(),
            ),
        ];
        let found = loopback_endpoints(roles, &env);
        let names: Vec<&str> = found
            .iter()
            .map(|f| f.split(' ').nth(1).unwrap_or(f.as_str()))
            .collect();
        assert_eq!(found.len(), 4, "{found:?}");
        for n in ["local", "other", "v6"] {
            assert!(names.contains(&n), "{found:?}");
        }
        assert!(
            found.iter().any(|f| f.starts_with("HTTPS_PROXY")),
            "{found:?}"
        );
        assert!(loopback_endpoints("[providers.h]\nbase_url = \"https://x.y/v1\"", &[]).is_empty());
    }
}
