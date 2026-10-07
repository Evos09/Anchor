//! Approval gates answered from the dashboard.
//!
//! With `anchor team run --ui`, every prompt the backend would ask on the
//! terminal (a tool approval, a merge, a context-limit choice) becomes a
//! pending approval: audited as `approval_pending`, shown in the
//! dashboard, and answered there (`approval_resolved`, `via: "ui"`). The
//! asking thread blocks until then. The answer then goes through the same
//! policy path as a terminal answer (`prompt_response`, responder
//! `interactive`). Approvals are serialized only per agent, so one agent
//! waiting on the user never stalls another.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use anchor_policy::{AuditLog, Responder, UserResponse};
use anchor_protocol::ResumeChoice;
use serde::Serialize;
use serde_json::{Value, json};

use crate::interact::{Interaction, PromptView};

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub id: u64,
    /// `tool`, `merge` or `resume`.
    pub kind: &'static str,
    pub agent: Option<String>,
    pub merge_seq: Option<i64>,
    /// One line for lists.
    pub summary: String,
    pub tool: Option<String>,
    pub operation: Option<String>,
    pub target: Option<String>,
    pub rule: Option<String>,
    /// Full context (a deletion diff, a merge's change list and verdict).
    pub detail: Option<String>,
    pub choices: Vec<&'static str>,
    pub created_ms: u64,
}

#[derive(Debug, Default)]
struct Inner {
    next: u64,
    pending: BTreeMap<u64, Pending>,
    answers: BTreeMap<u64, &'static str>,
    closed: bool,
    /// A cancelled task: refuse until [`Broker::resume`].
    refusing: bool,
}

#[derive(Debug)]
pub struct Broker {
    inner: Mutex<Inner>,
    cv: Condvar,
    /// Set once the workspace is open; nothing is asked before.
    audit: OnceLock<Arc<AuditLog>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Broker {
    pub fn new() -> Arc<Broker> {
        Arc::new(Broker {
            inner: Mutex::new(Inner {
                next: 1,
                ..Default::default()
            }),
            cv: Condvar::new(),
            audit: OnceLock::new(),
        })
    }

    /// The audit log approvals are recorded in. Until it is set, every
    /// request is refused (an approval must be auditable).
    pub fn set_audit(&self, audit: Arc<AuditLog>) {
        let _ = self.audit.set(audit);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Ask, and block until the dashboard answers. Returns one of
    /// `p.choices`. Once the broker is closed (or if the request cannot be
    /// audited) the answer is the last choice, which callers make the
    /// refusing one.
    pub fn ask(&self, mut p: Pending) -> &'static str {
        let refuse = p.choices.last().copied().unwrap_or("deny");
        let id = {
            let mut g = self.lock();
            if g.closed || g.refusing {
                return refuse;
            }
            let id = g.next;
            g.next += 1;
            p.id = id;
            p.created_ms = now_ms();
            g.pending.insert(id, p.clone());
            id
        };
        // Audited before anyone can answer it.
        let logged = self.audit.get().map(|a| {
            a.append(
                "approval_pending",
                serde_json::to_value(&p).unwrap_or_else(|_| json!({ "id": id })),
            )
        });
        if !matches!(logged, Some(Ok(_))) {
            self.lock().pending.remove(&id);
            return refuse;
        }
        let mut g = self.lock();
        let answer = loop {
            if let Some(a) = g.answers.remove(&id) {
                break a;
            }
            if g.closed || g.refusing {
                break refuse;
            }
            g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
        };
        g.pending.remove(&id);
        drop(g);
        let _ = self.audit.get().map(|a| {
            a.append(
                "approval_resolved",
                json!({ "id": id, "answer": answer, "via": "ui", "agent": p.agent }),
            )
        });
        answer
    }

    /// Answer approval `id` with `choice` (one of its choices).
    pub fn answer(&self, id: u64, choice: &str) -> Result<(), String> {
        let mut g = self.lock();
        let p = g
            .pending
            .get(&id)
            .ok_or_else(|| format!("approval {id} is not pending"))?;
        let c = p
            .choices
            .iter()
            .copied()
            .find(|c| *c == choice)
            .ok_or_else(|| format!("{choice:?} is not a choice for approval {id}"))?;
        if g.answers.contains_key(&id) {
            return Err(format!("approval {id} was already answered"));
        }
        g.answers.insert(id, c);
        drop(g);
        self.cv.notify_all();
        Ok(())
    }

    pub fn pending(&self) -> Vec<Pending> {
        let g = self.lock();
        g.pending
            .values()
            .filter(|p| !g.answers.contains_key(&p.id))
            .cloned()
            .collect()
    }

    /// Refuse everything pending, and everything asked until
    /// [`Broker::resume`] (the user stopped the task).
    pub fn cancel_pending(&self) {
        self.lock().refusing = true;
        self.cv.notify_all();
    }

    /// Ask the user again (a new task starts).
    pub fn resume(&self) {
        self.lock().refusing = false;
    }

    /// Refuse everything pending and everything asked from now on (the
    /// dashboard is going away).
    pub fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }
}

/// The [`Interaction`] of a `--ui` run.
#[derive(Debug, Clone)]
pub struct UiInteraction {
    broker: Arc<Broker>,
    /// Who this interaction speaks for (set by [`Interaction::split`]).
    name: Option<String>,
}

impl UiInteraction {
    pub fn new(broker: Arc<Broker>) -> Self {
        UiInteraction { broker, name: None }
    }
}

impl Interaction for UiInteraction {
    fn confirm(&mut self, v: &PromptView) -> UserResponse {
        let agent = v.agent.clone().or_else(|| self.name.clone());
        let target = v.canonical_target.as_ref().map(|t| t.display().to_string());
        let (kind, summary) = match v.merge_seq {
            Some(seq) => (
                "merge",
                format!(
                    "merge #{seq} from {} into the main workspace",
                    agent.as_deref().unwrap_or("an agent")
                ),
            ),
            None => (
                "tool",
                format!(
                    "{}: {:?} {:?} {}",
                    agent.as_deref().unwrap_or("agent"),
                    v.tool,
                    v.operation,
                    target.as_deref().unwrap_or("")
                ),
            ),
        };
        let answer = self.broker.ask(Pending {
            id: 0,
            kind,
            agent,
            merge_seq: v.merge_seq,
            summary,
            tool: Some(format!("{:?}", v.tool)),
            operation: Some(format!("{:?}", v.operation)),
            target,
            rule: Some(v.rule.clone()),
            detail: v.detail.clone(),
            choices: vec!["approve", "deny"],
            created_ms: 0,
        });
        if answer == "approve" {
            UserResponse::Approve
        } else {
            UserResponse::Deny
        }
    }

    fn resume_choice(&mut self, checkpoint_id: &str, ratio_hint: &str) -> ResumeChoice {
        let answer = self.broker.ask(Pending {
            id: 0,
            kind: "resume",
            agent: self.name.clone(),
            merge_seq: None,
            summary: format!(
                "{} reached {ratio_hint} of its context window (checkpoint {checkpoint_id})",
                self.name.as_deref().unwrap_or("the agent")
            ),
            tool: None,
            operation: None,
            target: None,
            rule: None,
            detail: Some(
                "continue: compact the context and go on (checkpoint kept)\n\
                 fresh: stop; a fresh session can be seeded from the checkpoint\n\
                 rollback: stop; roll back to an earlier snapshot"
                    .into(),
            ),
            choices: vec!["continue", "rollback", "fresh"],
            created_ms: 0,
        });
        match answer {
            "continue" => ResumeChoice::ContinueWithCompaction,
            "rollback" => ResumeChoice::Rollback,
            _ => ResumeChoice::FreshSession,
        }
    }

    fn notify(&mut self, line: &str) {
        eprintln!("{line}");
    }

    fn responder(&self) -> Responder {
        Responder::Interactive
    }

    fn split(&self, name: &str) -> Option<Box<dyn Interaction>> {
        Some(Box::new(UiInteraction {
            broker: self.broker.clone(),
            name: Some(name.to_string()),
        }))
    }
}

/// Pending approvals as JSON (for the roster's "blocked" status).
pub fn pending_json(b: &Broker) -> Vec<Value> {
    b.pending()
        .into_iter()
        .map(|p| serde_json::to_value(p).unwrap_or_default())
        .collect()
}
