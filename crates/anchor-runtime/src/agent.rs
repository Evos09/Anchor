//! The agent loop. Talks to the backend only through a [`Transport`]; every
//! tool execution is a request the backend may refuse.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use anchor_checkpoint::{ContextStatus, ContextTracker};
use anchor_protocol::{
    CHECKPOINT_FORMAT_VERSION, ChatMessage, ChatRole, Checkpoint, CheckpointReason, ContextUsage,
    Done, Failed, Frame, Hello, Message, Notice, NoticeKind, PROTOCOL_VERSION, PauseReason, Paused,
    ResumeChoice, Task, ToolCall, ToolOutcome, Usage,
};

use crate::config::RolesConfig;
use crate::prompt::{PrefixMonitor, SYSTEM_PROMPT, TOOLS_JSON};
use crate::provider::{self, ChatResponse, Provider, ProviderError};
use crate::router::{Router, TaskKind};

pub const MAX_TURNS: u64 = 100;
/// Messages kept verbatim at the end of history when compacting.
const COMPACT_KEEP_TAIL: usize = 6;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("backend closed the connection")]
    Closed,
    #[error("shutdown requested")]
    Shutdown,
    #[error("{0}")]
    Fatal(String),
}

pub trait Transport {
    fn send(&mut self, msg: Message) -> Result<(), AgentError>;
    fn recv(&mut self) -> Result<Message, AgentError>;
}

/// NDJSON over a reader/writer pair (stdin/stdout in production).
#[derive(Debug)]
pub struct StdioTransport<R, W> {
    r: R,
    w: W,
    next_id: u64,
}

impl<R: BufRead, W: Write> StdioTransport<R, W> {
    pub fn new(r: R, w: W) -> Self {
        StdioTransport { r, w, next_id: 1 }
    }
}

impl<R: BufRead, W: Write> Transport for StdioTransport<R, W> {
    fn send(&mut self, msg: Message) -> Result<(), AgentError> {
        let f = Frame::new(self.next_id, msg);
        self.next_id += 1;
        anchor_protocol::write_frame(&mut self.w, &f)
            .map_err(|e| AgentError::Protocol(e.to_string()))
    }
    fn recv(&mut self) -> Result<Message, AgentError> {
        match anchor_protocol::read_frame(&mut self.r) {
            Ok(Some(f)) => Ok(f.msg),
            Ok(None) => Err(AgentError::Closed),
            Err(e) => Err(AgentError::Protocol(e.to_string())),
        }
    }
}

pub type ProviderFactory<'a> = dyn Fn(&crate::config::ProviderConfig, Option<&str>) -> Result<Box<dyn Provider>, ProviderError>
    + 'a;

struct Session<'a, T: Transport> {
    t: &'a mut T,
    task: Task,
    cfg: RolesConfig,
    role: String,
    providers: BTreeMap<String, Box<dyn Provider>>,
    factory: &'a ProviderFactory<'a>,
    messages: Vec<ChatMessage>,
    turn: u64,
    tracker: ContextTracker,
    prefix: PrefixMonitor,
    last_used: u64,
}

/// Run one session: handshake, task, loop until done.
pub fn run_session<T: Transport>(
    t: &mut T,
    factory: &ProviderFactory<'_>,
) -> Result<(), AgentError> {
    t.send(Message::Hello(Hello {
        runtime_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
    }))?;
    match t.recv()? {
        Message::Welcome(w) if w.protocol_version == PROTOCOL_VERSION => {}
        other => {
            return Err(AgentError::Protocol(format!(
                "expected welcome, got {other:?}"
            )));
        }
    }
    let task = match t.recv()? {
        Message::Task(task) => *task,
        Message::Shutdown => return Ok(()),
        other => {
            return Err(AgentError::Protocol(format!(
                "expected task, got {other:?}"
            )));
        }
    };
    let cfg = match RolesConfig::parse(&task.roles_toml) {
        Ok(c) => c,
        Err(e) => {
            t.send(Message::Failed(Failed {
                error: e.to_string(),
            }))?;
            return wait_shutdown(t);
        }
    };
    let router = Router::new(&cfg);
    let route = router.route(TaskKind::Agentic).clone();
    t.send(Message::Notice(Notice {
        kind: NoticeKind::RoutingDecision,
        detail: format!(
            "role={} provider={} model={} reason: {}",
            route.role, route.provider, route.model_id, route.reason
        ),
    }))?;
    let tracker = ContextTracker::new(task.checkpoint_threshold)
        .map_err(|e| AgentError::Fatal(e.to_string()))?;

    let (messages, turn) = match &task.resume_from {
        Some(cp) => {
            let mut m = cp.messages.clone();
            m.push(ChatMessage {
                role: ChatRole::User,
                content: format!(
                    "[Session resumed from checkpoint {} at turn {}. Continue the task.]",
                    cp.checkpoint_id, cp.turn
                ),
                tool_calls: vec![],
                tool_call_id: None,
            });
            (m, cp.turn)
        }
        None => (
            vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: SYSTEM_PROMPT.into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: task.instruction.clone(),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
            ],
            0,
        ),
    };

    let mut s = Session {
        t,
        task,
        cfg,
        role: route.role,
        providers: BTreeMap::new(),
        factory,
        messages,
        turn,
        tracker,
        prefix: PrefixMonitor::default(),
        last_used: 0,
    };
    let result = s.run();
    match result {
        Ok(()) => wait_shutdown(s.t),
        Err(AgentError::Shutdown) => Ok(()),
        Err(AgentError::Fatal(msg)) => {
            s.t.send(Message::Failed(Failed { error: msg }))?;
            wait_shutdown(s.t)
        }
        Err(e) => Err(e),
    }
}

fn wait_shutdown<T: Transport>(t: &mut T) -> Result<(), AgentError> {
    loop {
        match t.recv() {
            Ok(Message::Shutdown) | Err(AgentError::Closed) => return Ok(()),
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
}

impl<T: Transport> Session<'_, T> {
    fn run(&mut self) -> Result<(), AgentError> {
        let start_turn = self.turn;
        loop {
            if self.turn - start_turn >= MAX_TURNS {
                return Err(AgentError::Fatal(format!("turn limit {MAX_TURNS} reached")));
            }
            self.turn += 1;
            self.t.send(Message::TurnStarted { turn: self.turn })?;

            if self.prefix.check(&self.messages, &TOOLS_JSON).is_err() {
                self.t.send(Message::Notice(Notice {
                    kind: NoticeKind::PromptPrefixChanged,
                    detail: "prompt prefix hash changed between turns; provider cache invalidated"
                        .into(),
                }))?;
                if cfg!(debug_assertions) {
                    return Err(AgentError::Fatal(
                        "prompt prefix changed (debug build)".into(),
                    ));
                }
            }

            let resp = self.complete_with_fallback()?;
            self.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: resp.content.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            });

            for call in &resp.tool_calls {
                let outcome = self.execute(call.clone())?;
                let content = match outcome {
                    ToolOutcome::Ok { output } => output,
                    ToolOutcome::Denied { by, detail } => format!("DENIED by {by:?}: {detail}"),
                    ToolOutcome::Error { detail } => format!("ERROR: {detail}"),
                };
                self.messages.push(ChatMessage {
                    role: ChatRole::Tool,
                    content,
                    tool_calls: vec![],
                    tool_call_id: Some(call.call_id.clone()),
                });
            }
            for bad in &resp.invalid_calls {
                self.messages.push(ChatMessage {
                    role: ChatRole::User,
                    content: format!(
                        "[Tool call {} ({}) was rejected before execution: {}]",
                        bad.id, bad.name, bad.error
                    ),
                    tool_calls: vec![],
                    tool_call_id: None,
                });
            }

            let status = self.report_context(&resp)?;
            self.checkpoint(CheckpointReason::TurnBoundary, None)?;
            self.t.send(Message::TurnFinished { turn: self.turn })?;

            if resp.tool_calls.is_empty() && resp.invalid_calls.is_empty() {
                self.t.send(Message::Done(Done {
                    summary: resp.content,
                }))?;
                return Ok(());
            }
            if let ContextStatus::PauseRequired { ratio } = status
                && !self.pause(ratio)?
            {
                return Ok(());
            }
        }
    }

    fn role_cfg(&self) -> &crate::config::RoleConfig {
        &self.cfg.roles[&self.role]
    }

    fn complete_with_fallback(&mut self) -> Result<ChatResponse, AgentError> {
        let role = self.role_cfg().clone();
        let chain: Vec<String> = std::iter::once(role.provider.clone())
            .chain(role.fallback.iter().cloned())
            .collect();
        let mut last_err = None;
        for (i, pid) in chain.iter().enumerate() {
            if i > 0 {
                // Never a silent downgrade.
                self.t.send(Message::Notice(Notice {
                    kind: NoticeKind::ProviderFallback,
                    detail: format!(
                        "provider {:?} failed ({}); falling back to {pid:?}",
                        chain[i - 1],
                        last_err
                            .as_ref()
                            .map(|e: &ProviderError| e.to_string())
                            .unwrap_or_default()
                    ),
                }))?;
            }
            if !self.providers.contains_key(pid) {
                let pcfg = &self.cfg.providers[pid];
                match (self.factory)(pcfg, self.task.mock_script.as_deref()) {
                    Ok(p) => {
                        self.providers.insert(pid.clone(), p);
                    }
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                }
            }
            let Some(p) = self.providers.get_mut(pid) else {
                continue;
            };
            match p.complete(&role.model_id, &self.messages, &TOOLS_JSON, &role.params) {
                Ok(r) => {
                    let usage = r.usage.unwrap_or_default();
                    let cost = (usage.prompt_tokens as f64 * role.price_in_per_mtok
                        + usage.completion_tokens as f64 * role.price_out_per_mtok)
                        / 1_000_000.0;
                    self.t.send(Message::Usage(Usage {
                        role: self.role.clone(),
                        model_id: role.model_id.clone(),
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        cost_usd: cost,
                    }))?;
                    return Ok(r);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(AgentError::Fatal(format!(
            "all providers failed for role {}: {}",
            self.role,
            last_err.map(|e| e.to_string()).unwrap_or_default()
        )))
    }

    fn execute(&mut self, call: ToolCall) -> Result<ToolOutcome, AgentError> {
        let id = call.call_id.clone();
        self.t.send(Message::ToolCall(call))?;
        match self.t.recv()? {
            Message::ToolResult(r) if r.call_id == id => Ok(r.outcome),
            Message::Shutdown => Err(AgentError::Shutdown),
            other => Err(AgentError::Protocol(format!(
                "expected tool_result for {id}, got {other:?}"
            ))),
        }
    }

    fn report_context(&mut self, resp: &ChatResponse) -> Result<ContextStatus, AgentError> {
        let used = match resp.usage {
            Some(u) if u.prompt_tokens + u.completion_tokens > 0 => {
                u.prompt_tokens + u.completion_tokens
            }
            // No usage reported: estimate ~4 bytes per token.
            _ => {
                self.messages
                    .iter()
                    .map(|m| m.content.len() as u64)
                    .sum::<u64>()
                    / 4
            }
        };
        self.last_used = used;
        let role = self.role_cfg().clone();
        self.t.send(Message::ContextUsage(ContextUsage {
            model_id: role.model_id.clone(),
            used_tokens: used,
            window_tokens: role.context_window,
        }))?;
        Ok(self
            .tracker
            .update(&role.model_id, used, role.context_window))
    }

    fn checkpoint(
        &mut self,
        reason: CheckpointReason,
        suffix: Option<&str>,
    ) -> Result<String, AgentError> {
        let role = self.role_cfg().clone();
        let id = format!(
            "{}-t{:04}{}",
            self.task.session_id,
            self.turn,
            suffix.map(|s| format!("-{s}")).unwrap_or_default()
        );
        let cp = Checkpoint {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: id.clone(),
            session_id: self.task.session_id.clone(),
            reason,
            turn: self.turn,
            model_id: role.model_id,
            context_used_tokens: self.last_used,
            context_window_tokens: role.context_window,
            messages: self.messages.clone(),
            tasks: vec![],
            file_claims: vec![],
            pending_decisions: vec![],
            summary: None,
        };
        self.t.send(Message::Checkpoint(Box::new(cp)))?;
        match self.t.recv()? {
            Message::CheckpointSaved(s) if s.checkpoint_id == id => Ok(id),
            Message::Shutdown => Err(AgentError::Shutdown),
            other => Err(AgentError::Protocol(format!(
                "expected checkpoint_saved, got {other:?}"
            ))),
        }
    }

    /// Pause at the context threshold. Returns `true` to continue.
    fn pause(&mut self, ratio: f64) -> Result<bool, AgentError> {
        let id = self.checkpoint(CheckpointReason::ContextThreshold, Some("pause"))?;
        self.t.send(Message::Paused(Paused {
            reason: PauseReason::ContextThreshold,
            checkpoint_id: id.clone(),
        }))?;
        let choice = match self.t.recv()? {
            Message::Resume(r) => r.choice,
            Message::Shutdown => return Err(AgentError::Shutdown),
            other => {
                return Err(AgentError::Protocol(format!(
                    "expected resume, got {other:?}"
                )));
            }
        };
        match choice {
            ResumeChoice::ContinueWithCompaction => {
                compact(&mut self.messages, &id);
                Ok(true)
            }
            ResumeChoice::FreshSession | ResumeChoice::Rollback => {
                self.t.send(Message::Done(Done {
                    summary: format!(
                        "Paused at {:.0}% context. Session ended by user choice ({choice:?}); \
                         state is preserved in checkpoint {id}.",
                        ratio * 100.0
                    ),
                }))?;
                Ok(false)
            }
        }
    }
}

/// Compact history: keep leading system messages, the first user message, a
/// marker pointing to the checkpoint, and the most recent messages (never
/// starting the tail on an orphaned tool reply). The full history stays in
/// the checkpoint on disk.
pub fn compact(messages: &mut Vec<ChatMessage>, checkpoint_id: &str) {
    let head_len = messages
        .iter()
        .take_while(|m| m.role == ChatRole::System)
        .count()
        + usize::from(messages.iter().any(|m| m.role == ChatRole::User));
    if messages.len() <= head_len + COMPACT_KEEP_TAIL {
        return;
    }
    let mut tail_start = messages.len() - COMPACT_KEEP_TAIL;
    while tail_start < messages.len() && messages[tail_start].role == ChatRole::Tool {
        tail_start += 1;
    }
    if tail_start <= head_len {
        return;
    }
    let removed = tail_start - head_len;
    let tail = messages.split_off(tail_start);
    messages.truncate(head_len);
    messages.push(ChatMessage {
        role: ChatRole::User,
        content: format!(
            "[Context compacted: {removed} earlier messages removed. The full history is preserved in checkpoint {checkpoint_id}.]"
        ),
        tool_calls: vec![],
        tool_call_id: None,
    });
    messages.extend(tail);
}

/// Default provider factory.
pub fn default_factory() -> Box<ProviderFactory<'static>> {
    Box::new(provider::build)
}
