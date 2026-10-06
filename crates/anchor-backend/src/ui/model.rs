//! The dashboard's model (spec §12.2, §12.3): a fold over the audit log.
//!
//! Every audit entry becomes one [`Event`] with a deterministic,
//! template-generated sentence (never a model's words). Kinds this module
//! does not know still become an event (`kind` plus its JSON), so nothing
//! the log records is ever dropped from the dashboard. The same fold keeps
//! sessions (for the roster) and the cost meter, which recomputes every
//! reported cost from the prices in the user's own roles files.

use std::collections::BTreeMap;

use anchor_hub::{Agent, AgentStatus, MergeEntry, MergeStatus};
use anchor_policy::AuditEntry;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Bookkeeping (heartbeats, reads); shown on request.
    Trace,
    Info,
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    pub ts_ms: u64,
    pub kind: String,
    pub category: &'static str,
    pub level: Level,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub text: String,
    pub data: Value,
}

/// Token and spend totals. `spend_usd` uses the user's prices where they
/// are known (`computed_usd`) and the runtime's report otherwise.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Tally {
    pub calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// What the runtimes reported.
    pub reported_usd: f64,
    /// Recomputed from the roles files' prices (priced calls only).
    pub computed_usd: f64,
    /// Calls whose role had no known price (their reported cost is used).
    pub unpriced_calls: u64,
    pub spend_usd: f64,
}

impl Tally {
    fn add(&mut self, tin: u64, tout: u64, reported: f64, computed: Option<f64>) {
        self.calls += 1;
        self.tokens_in += tin;
        self.tokens_out += tout;
        self.reported_usd += reported;
        match computed {
            Some(c) => {
                self.computed_usd += c;
                self.spend_usd += c;
            }
            None => {
                self.unpriced_calls += 1;
                self.spend_usd += reported;
            }
        }
    }
}

/// One model call, as priced by the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct CostLine {
    pub seq: u64,
    pub agent: String,
    pub session_id: Option<String>,
    pub role: String,
    pub model_id: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub reported_usd: f64,
    /// `(in, out)` USD per million tokens, when known.
    pub price: Option<(f64, f64)>,
    pub computed_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Cost {
    pub total: Tally,
    pub per_agent: BTreeMap<String, Tally>,
    pub per_session: BTreeMap<String, Tally>,
    pub per_model: BTreeMap<String, Tally>,
    pub lines: Vec<CostLine>,
    /// Every place where reported and recomputed numbers disagree. Shown
    /// in red, never hidden.
    pub discrepancies: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Session {
    pub id: String,
    pub agent: Option<String>,
    pub review: bool,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub turn: u64,
    pub in_turn: bool,
    pub done: Option<bool>,
    pub failed: Option<String>,
    pub error: Option<String>,
    pub tally: Tally,
    pub tool_calls: u64,
    pub denied: u64,
}

/// Where an agent stands now (§12.2 roster).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Thinking,
    Waiting,
    Blocked,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RosterRow {
    pub name: String,
    pub agent_id: Option<i64>,
    pub role: String,
    pub model_id: String,
    pub family: String,
    pub hub_status: Option<AgentStatus>,
    pub status: Status,
    /// Why (e.g. "turn 3", "awaiting your approval", "in merge queue (#4)").
    pub detail: String,
    pub instruction: Option<String>,
    pub claim_within: Vec<String>,
    pub session_id: Option<String>,
    pub sessions: u64,
    pub cost: Tally,
}

/// The fold.
#[derive(Debug, Default)]
pub struct Model {
    pub events: Vec<Event>,
    /// Hub agent id → display name.
    names: BTreeMap<i64, String>,
    /// `team_started` data, if a team ran.
    pub team: Option<Value>,
    /// agent name → role → price.
    prices: BTreeMap<String, BTreeMap<String, (f64, f64)>>,
    pub sessions: BTreeMap<String, Session>,
    pub cost: Cost,
    /// `approval_pending` entries not yet resolved, by id.
    pub pending: BTreeMap<u64, Value>,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn short(v: &Value, max: usize) -> String {
    let t = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if t.chars().count() > max {
        format!("{}…", t.chars().take(max).collect::<String>())
    } else {
        t
    }
}

fn prices_of(v: &Value) -> BTreeMap<String, (f64, f64)> {
    v.as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(role, p)| {
                    let a = p.as_array()?;
                    Some((role.clone(), (a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Relative tolerance for comparing costs (floating-point sums).
fn same_usd(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 + 1e-9 * a.abs().max(b.abs())
}

impl Model {
    pub fn name_of(&self, id: i64) -> String {
        self.names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("agent #{id}"))
    }

    /// Register agent names the Hub knows (e.g. from an earlier run).
    pub fn learn_agents(&mut self, agents: &[Agent]) {
        for a in agents {
            self.names.insert(a.id, a.display_name.clone());
        }
    }

    pub fn apply(&mut self, e: &AuditEntry) {
        let d = &e.data;
        let mut agent = s(&d["agent"]);
        if agent.is_none() {
            if d["caller"]["kind"] == "agent" {
                agent = d["caller"]["id"].as_i64().map(|id| self.name_of(id));
            } else if let Some(id) = d["agent_id"].as_i64() {
                agent = Some(self.name_of(id));
            }
        }
        let session_id = s(&d["session_id"]);
        self.track(e, agent.as_deref(), session_id.as_deref());
        let (category, level, text) = self.narrate(e, agent.as_deref());
        self.events.push(Event {
            seq: e.seq,
            ts_ms: e.ts_ms,
            kind: e.kind.clone(),
            category,
            level,
            agent,
            session_id,
            text,
            data: e.data.clone(),
        });
    }

    fn session(&mut self, id: Option<&str>) -> Option<&mut Session> {
        self.sessions.get_mut(id?)
    }

    /// Sessions, cost, names, pending approvals.
    fn track(&mut self, e: &AuditEntry, agent: Option<&str>, sid: Option<&str>) {
        let d = &e.data;
        match e.kind.as_str() {
            "team_started" => {
                for a in d["agents"].as_array().into_iter().flatten() {
                    if let (Some(id), Some(n)) = (a["agent_id"].as_i64(), a["name"].as_str()) {
                        self.names.insert(id, n.to_string());
                        self.prices.insert(n.to_string(), prices_of(&a["prices"]));
                    }
                }
                if !d["verify"]["prices"].is_null() {
                    self.prices
                        .insert("reviewer".into(), prices_of(&d["verify"]["prices"]));
                }
                self.team = Some(d.clone());
            }
            "hub_register_agent" => {
                let id = d["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|r| r["table"] == "agent")
                    .and_then(|r| r["id"].as_i64());
                if let (Some(id), Some(n)) = (id, d["args"]["display_name"].as_str()) {
                    self.names.insert(id, n.to_string());
                }
            }
            "session_started" => {
                if let Some(id) = sid {
                    self.sessions.insert(
                        id.to_string(),
                        Session {
                            id: id.to_string(),
                            agent: agent.map(str::to_string),
                            review: d["review"].as_bool().unwrap_or(false),
                            started_ms: e.ts_ms,
                            ..Default::default()
                        },
                    );
                }
            }
            "turn_started" => {
                if let Some(s) = self.session(sid) {
                    s.turn = d["turn"].as_u64().unwrap_or(s.turn);
                    s.in_turn = true;
                }
            }
            "turn_finished" => {
                if let Some(s) = self.session(sid) {
                    s.in_turn = false;
                }
            }
            "decision" => {
                if d["action"] == "deny"
                    && let Some(s) = self.session(sid)
                {
                    s.denied += 1;
                }
            }
            "mutation" | "command" | "command_refused" => {
                if let Some(s) = self.session(sid) {
                    s.tool_calls += 1;
                }
            }
            "usage" => self.usage(e, agent, sid),
            "session_finished" => self.finish_session(e, sid),
            "approval_pending" => {
                if let Some(id) = d["id"].as_u64() {
                    self.pending.insert(id, d.clone());
                }
            }
            "approval_resolved" => {
                if let Some(id) = d["id"].as_u64() {
                    self.pending.remove(&id);
                }
            }
            _ => {}
        }
    }

    fn usage(&mut self, e: &AuditEntry, agent: Option<&str>, sid: Option<&str>) {
        let d = &e.data;
        let agent = agent.unwrap_or("unattributed").to_string();
        let role = s(&d["role"]).unwrap_or_default();
        let (tin, tout) = (
            d["input_tokens"].as_u64().unwrap_or(0),
            d["output_tokens"].as_u64().unwrap_or(0),
        );
        let reported = d["cost_usd"].as_f64().unwrap_or(0.0);
        let price = self.prices.get(&agent).and_then(|p| p.get(&role)).copied();
        let computed = price.map(|(pi, po)| (tin as f64 * pi + tout as f64 * po) / 1_000_000.0);
        if let Some(c) = computed
            && !same_usd(c, reported)
        {
            self.cost.discrepancies.push(format!(
                "entry {}: {agent}'s runtime reported ${reported:.6} for {tin} in / {tout} out \
                 tokens as {role}; your prices give ${c:.6}",
                e.seq
            ));
        }
        let model_id = s(&d["model_id"]).unwrap_or_default();
        for t in [
            Some(&mut self.cost.total),
            Some(self.cost.per_agent.entry(agent.clone()).or_default()),
            Some(self.cost.per_model.entry(model_id.clone()).or_default()),
            sid.map(|id| self.cost.per_session.entry(id.to_string()).or_default()),
        ]
        .into_iter()
        .flatten()
        {
            t.add(tin, tout, reported, computed);
        }
        if let Some(s) = self.session(sid) {
            s.tally.add(tin, tout, reported, computed);
        }
        self.cost.lines.push(CostLine {
            seq: e.seq,
            agent,
            session_id: sid.map(str::to_string),
            role,
            model_id,
            tokens_in: tin,
            tokens_out: tout,
            reported_usd: reported,
            price,
            computed_usd: computed,
        });
    }

    fn finish_session(&mut self, e: &AuditEntry, sid: Option<&str>) {
        let d = &e.data;
        let Some(s) = sid.and_then(|id| self.sessions.get_mut(id)) else {
            return;
        };
        s.ended_ms = Some(e.ts_ms);
        s.in_turn = false;
        s.done = d["done"].as_bool();
        s.failed = s_opt(&d["failed"]);
        s.error = s_opt(&d["error"]);
        let (tin, tout, usd) = (
            d["tokens_in"].as_u64().unwrap_or(0),
            d["tokens_out"].as_u64().unwrap_or(0),
            d["cost_usd"].as_f64().unwrap_or(0.0),
        );
        if tin != s.tally.tokens_in
            || tout != s.tally.tokens_out
            || !same_usd(usd, s.tally.reported_usd)
        {
            self.cost.discrepancies.push(format!(
                "session {}: its summary says {tin} in / {tout} out tokens, ${usd:.6}; \
                 its usage entries add up to {} in / {} out, ${:.6}",
                s.id, s.tally.tokens_in, s.tally.tokens_out, s.tally.reported_usd
            ));
        }
    }

    /// The roster (§12.2): team agents (and the reviewer, if it ran),
    /// with live status.
    pub fn roster(
        &self,
        hub_agents: &[Agent],
        merges: &[MergeEntry],
        pending: &[Value],
    ) -> Vec<RosterRow> {
        let mut rows: Vec<RosterRow> = Vec::new();
        let team_agents: Vec<&Value> = self
            .team
            .as_ref()
            .and_then(|t| t["agents"].as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let mut names: Vec<String> = team_agents.iter().filter_map(|a| s(&a["name"])).collect();
        for a in hub_agents {
            if !names.contains(&a.display_name) {
                names.push(a.display_name.clone());
            }
        }
        for sess in self.sessions.values() {
            if let Some(a) = &sess.agent
                && !names.contains(a)
            {
                names.push(a.clone());
            }
        }
        for name in names {
            let hub = hub_agents.iter().find(|a| a.display_name == name);
            let spec = team_agents.iter().find(|a| a["name"] == name.as_str());
            let mine: Vec<&Session> = self
                .sessions
                .values()
                .filter(|s| s.agent.as_deref() == Some(&name))
                .collect();
            let last = mine.iter().max_by_key(|s| s.started_ms);
            let waiting_merge = hub.and_then(|h| {
                merges
                    .iter()
                    .filter(|m| m.agent_id == Some(h.id))
                    .max_by_key(|m| m.seq)
                    .filter(|m| matches!(m.status, MergeStatus::Queued | MergeStatus::Applying))
            });
            let blocked = pending
                .iter()
                .find(|p| p["agent"].as_str() == Some(name.as_str()));
            let (status, detail) = if let Some(p) = blocked {
                (
                    Status::Blocked,
                    format!(
                        "awaiting your approval ({})",
                        p["summary"].as_str().unwrap_or("approval")
                    ),
                )
            } else if let Some(m) = waiting_merge {
                (Status::Waiting, format!("in the merge queue (#{})", m.seq))
            } else {
                match last {
                    None => (Status::Waiting, "not started".to_string()),
                    Some(s) if s.ended_ms.is_none() && s.in_turn => {
                        (Status::Thinking, format!("turn {}", s.turn))
                    }
                    Some(s) if s.ended_ms.is_none() => (
                        Status::Waiting,
                        format!("between turns (after turn {})", s.turn),
                    ),
                    Some(s) if s.failed.is_some() || s.error.is_some() => (
                        Status::Failed,
                        s.failed
                            .clone()
                            .or_else(|| s.error.clone())
                            .unwrap_or_default(),
                    ),
                    Some(_) => (Status::Done, "finished".to_string()),
                }
            };
            rows.push(RosterRow {
                agent_id: hub.map(|h| h.id),
                role: hub
                    .map(|h| h.role.clone())
                    .or_else(|| spec.and_then(|a| s(&a["role"])))
                    .unwrap_or_else(|| {
                        if name == "reviewer" {
                            "reviewer".into()
                        } else {
                            String::new()
                        }
                    }),
                model_id: hub
                    .map(|h| h.model_id.clone())
                    .or_else(|| spec.and_then(|a| s(&a["model_id"])))
                    .unwrap_or_default(),
                family: hub
                    .map(|h| h.family.clone())
                    .or_else(|| spec.and_then(|a| s(&a["family"])))
                    .unwrap_or_default(),
                hub_status: hub.map(|h| h.status),
                status,
                detail,
                instruction: spec.and_then(|a| s(&a["instruction"])),
                claim_within: spec
                    .and_then(|a| a["claim_within"].as_array())
                    .map(|v| v.iter().filter_map(s).collect())
                    .unwrap_or_default(),
                session_id: last.map(|s| s.id.clone()),
                sessions: mine.len() as u64,
                cost: self.cost.per_agent.get(&name).cloned().unwrap_or_default(),
                name,
            });
        }
        rows
    }

    /// One sentence per entry. Unknown kinds fall through to a generic
    /// line: the dashboard never drops an entry.
    fn narrate(&self, e: &AuditEntry, agent: Option<&str>) -> (&'static str, Level, String) {
        use Level::*;
        let d = &e.data;
        let who = agent.unwrap_or("the backend");
        let path = || {
            s(&d["path"])
                .or_else(|| s(&d["canonical_target"]))
                .or_else(|| s(&d["request"]["canonical_target"]))
                .unwrap_or_else(|| "?".into())
        };
        let n = |id: &Value| {
            id.as_i64()
                .map(|i| self.name_of(i))
                .unwrap_or_else(|| "?".into())
        };
        let args = &d["args"];
        let k = e.kind.as_str();
        match k {
            "team_started" => {
                let names: Vec<String> = d["agents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| {
                        format!(
                            "{} ({} on {}, family {})",
                            a["name"].as_str().unwrap_or("?"),
                            a["role"].as_str().unwrap_or("?"),
                            a["model_id"].as_str().unwrap_or("?"),
                            a["family"].as_str().unwrap_or("?")
                        )
                    })
                    .collect();
                let verify = match d["verify"]["kind"].as_str() {
                    Some(r) => format!("; merges verified, {r} reviewer"),
                    None => "; merges NOT verified".into(),
                };
                (
                    "session",
                    Info,
                    format!(
                        "Team started ({}): {}{verify}",
                        d["schedule"].as_str().unwrap_or("?"),
                        names.join(", ")
                    ),
                )
            }
            "session_started" => (
                "session",
                Info,
                if d["review"] == true {
                    format!("{who} started a review session")
                } else {
                    format!("{who} started a session")
                },
            ),
            "session_finished" => {
                let cost = format!(
                    "{} in / {} out tokens, ${:.4}",
                    d["tokens_in"],
                    d["tokens_out"],
                    d["cost_usd"].as_f64().unwrap_or(0.0)
                );
                if let Some(err) = d["error"].as_str() {
                    (
                        "session",
                        Fail,
                        format!("{who}'s session broke off: {err} ({cost})"),
                    )
                } else if let Some(f) = d["failed"].as_str() {
                    ("session", Fail, format!("{who} gave up: {f} ({cost})"))
                } else if d["done"] == true {
                    (
                        "session",
                        Ok,
                        format!("{who} finished its session ({cost})"),
                    )
                } else {
                    (
                        "session",
                        Warn,
                        format!("{who}'s session ended unfinished ({cost})"),
                    )
                }
            }
            "turn_started" => (
                "session",
                Info,
                format!("{who} is thinking (turn {})", d["turn"]),
            ),
            "turn_finished" => (
                "session",
                Trace,
                format!("{who} finished turn {}", d["turn"]),
            ),
            "usage" => (
                "cost",
                Info,
                format!(
                    "{who} used {} in / {} out tokens on {} (${:.6} reported)",
                    d["input_tokens"],
                    d["output_tokens"],
                    d["model_id"].as_str().unwrap_or("?"),
                    d["cost_usd"].as_f64().unwrap_or(0.0)
                ),
            ),
            "notice" => (
                "session",
                if d["kind"] == "provider_fallback" {
                    Warn
                } else {
                    Info
                },
                format!("{who}: {}", short(&d["detail"], 200)),
            ),
            "checkpoint_saved" => (
                "session",
                Info,
                format!(
                    "{who}'s context was checkpointed ({})",
                    short(&d["reason"], 60)
                ),
            ),
            "pause_resolved" => (
                "session",
                Info,
                format!(
                    "{who} paused at its context limit; you chose {}",
                    short(&d["choice"], 40)
                ),
            ),
            "claims_submitted" => {
                let claims: Vec<String> = d
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| short(&c["text"], 80))
                    .collect();
                (
                    "verify",
                    Info,
                    format!("{who} claims: {}", claims.join("; ")),
                )
            }
            "review_verdicts" => {
                let bad = d
                    .as_array()
                    .map(|v| v.iter().filter(|x| x["verdict"] != "supported").count())
                    .unwrap_or(0);
                (
                    "verify",
                    if bad > 0 { Warn } else { Ok },
                    format!(
                        "The reviewer returned {} verdict(s), {bad} not supported",
                        d.as_array().map_or(0, Vec::len)
                    ),
                )
            }
            "review_tool_refused" => (
                "verify",
                Fail,
                format!(
                    "The reviewer asked for tool {} and was stopped (reviewers get no tools)",
                    short(&d["tool"], 40)
                ),
            ),
            "decision" => {
                let target = path();
                let rule = short(&d["matched_rule"], 60);
                match d["action"].as_str() {
                    Some("deny") => (
                        "policy",
                        Warn,
                        format!(
                            "Policy DENIED {who}: {} {} on {target} (rule {rule})",
                            short(&d["request"]["tool"], 20),
                            short(&d["request"]["operation"], 20)
                        ),
                    ),
                    Some("prompt") => (
                        "policy",
                        Info,
                        format!(
                            "{who} needs approval: {} {} on {target} (rule {rule})",
                            short(&d["request"]["tool"], 20),
                            short(&d["request"]["operation"], 20)
                        ),
                    ),
                    _ => (
                        "policy",
                        Trace,
                        format!(
                            "Policy allowed {who}: {} {} on {target}",
                            short(&d["request"]["tool"], 20),
                            short(&d["request"]["operation"], 20)
                        ),
                    ),
                }
            }
            "prompt_response" => {
                let approved = d["response"] == "approve";
                (
                    "approval",
                    if approved { Ok } else { Warn },
                    format!(
                        "{} {who}'s request on {} ({})",
                        if approved { "Approved" } else { "Denied" },
                        path(),
                        short(&d["responder"], 20)
                    ),
                )
            }
            "user_initiated" => (
                "approval",
                Trace,
                format!(
                    "You ({}) {} {}",
                    short(&d["action"], 30),
                    short(&d["request"]["operation"], 20),
                    path()
                ),
            ),
            "approval_pending" => (
                "approval",
                Warn,
                format!("Waiting for your approval: {}", short(&d["summary"], 200)),
            ),
            "approval_resolved" => (
                "approval",
                Info,
                format!(
                    "Approval #{} answered {} (via {})",
                    d["id"],
                    short(&d["answer"], 20),
                    short(&d["via"], 20)
                ),
            ),
            "mutation" => ("tool", Info, format!("{who} changed {}", path())),
            "command" => {
                let argv = std::iter::once(short(&d["program"], 60))
                    .chain(
                        d["args"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|a| short(a, 40)),
                    )
                    .collect::<Vec<_>>()
                    .join(" ");
                let ok = d["exit"].as_str().is_some_and(|x| x.contains("Code(0)"));
                (
                    "tool",
                    if ok { Info } else { Warn },
                    format!("{who} ran `{argv}` ({})", short(&d["exit"], 40)),
                )
            }
            "command_refused" => (
                "tool",
                Fail,
                format!(
                    "{who}'s command {} was refused: {}",
                    short(&d["program"], 60),
                    short(&d["reason"], 80)
                ),
            ),
            "snapshot" => (
                "snapshot",
                Info,
                format!("Snapshot taken: {}", short(&d["label"], 80)),
            ),
            "snapshot_gc" => (
                "snapshot",
                Trace,
                format!("Old snapshots pruned ({} refs)", d["refs_pruned"]),
            ),
            "rollback" | "restore_recovered" => (
                "snapshot",
                Ok,
                format!(
                    "{} to snapshot {} ({} operations)",
                    if k == "rollback" {
                        "Rolled back"
                    } else {
                        "Recovered an interrupted restore"
                    },
                    short(&d["target"], 16),
                    d["operations"]
                ),
            ),
            "rollback_failed" => (
                "snapshot",
                Fail,
                format!("Rollback FAILED: {}", short(&d["error"], 200)),
            ),
            "user_override" => (
                "policy",
                Warn,
                format!(
                    "You overrode: {} {}",
                    short(&d["action"], 40),
                    short(&d["path"], 80)
                ),
            ),
            "policy_loaded" | "policy_reloaded" => ("policy", Info, "Policy loaded".into()),
            "policy_reload_failed" => (
                "policy",
                Fail,
                format!(
                    "Policy reload FAILED (kept the last good one): {}",
                    short(&d["error"], 200)
                ),
            ),
            "repair_started" => (
                "verify",
                Warn,
                format!(
                    "{who} gets another attempt ({}) to fix its rejected change (merge #{}, {})",
                    d["attempt"],
                    d["after"],
                    short(&d["reason"], 30)
                ),
            ),
            "merge_verified" => {
                let approved = d["summary"]["approved"] == true;
                if d["verified"] == false {
                    (
                        "merge",
                        Warn,
                        format!("Merge #{} was NOT verified (no gate configured)", d["seq"]),
                    )
                } else if approved {
                    (
                        "verify",
                        Ok,
                        format!(
                            "Merge #{} passed verification ({} reviewer, {} tests)",
                            d["seq"],
                            short(&d["summary"]["reviewer"], 20),
                            d["summary"]["tests"]
                        ),
                    )
                } else {
                    ("verify", Info, format!("Merge #{} checked", d["seq"]))
                }
            }
            "merge_rejected" => {
                let fb = &d["feedback"];
                let reason = fb["reason"].as_str().unwrap_or("?");
                let codes: Vec<String> = fb["detail"]["findings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| short(&f["code"], 40))
                    .collect();
                let why = if codes.is_empty() {
                    short(&fb["detail"], 160)
                } else {
                    codes.join(", ")
                };
                (
                    "merge",
                    Fail,
                    format!(
                        "Merge #{} from {} REJECTED ({reason}): {why}",
                        d["seq"],
                        n(&d["agent_id"])
                    ),
                )
            }
            "merge_applied" => (
                "merge",
                Ok,
                format!(
                    "Merge #{} from {} applied to the main workspace",
                    d["seq"],
                    n(&d["agent_id"])
                ),
            ),
            "merge_unverified" => (
                "merge",
                Warn,
                format!("Merge #{} was applied WITHOUT verification", d["seq"]),
            ),
            "merge_retry" => (
                "merge",
                Warn,
                format!(
                    "Merge #{} recomputed: the main workspace changed under it",
                    d["seq"]
                ),
            ),
            "merge_recovered" => (
                "merge",
                Warn,
                format!("Merge #{} was settled after a crash", d["seq"]),
            ),
            "merge_reverted" => ("merge", Warn, format!("Merge #{} was reverted", d["seq"])),
            "hub_denied" => (
                "hub",
                Warn,
                format!(
                    "The Hub refused {who}'s {}: {}",
                    short(&d["method"], 40),
                    short(&d["error"], 160)
                ),
            ),
            "hub_unauthenticated" => (
                "hub",
                Fail,
                format!(
                    "Unauthenticated request to the Hub at {}",
                    short(&d["path"], 60)
                ),
            ),
            "hub_heartbeat" | "hub_query_messages" | "hub_list_claims" | "hub_list_decisions"
            | "hub_list_tasks" | "hub_get_task" => {
                ("hub", Trace, format!("{who}: {}", k[4..].replace('_', " ")))
            }
            "hub_sweep" => (
                "hub",
                Trace,
                "Expired claims and silent agents swept".into(),
            ),
            "hub_open_project" => ("hub", Info, "The Hub was created for this workspace".into()),
            "hub_register_agent" => (
                "hub",
                Info,
                format!(
                    "Agent {} registered ({} on {})",
                    short(&args["display_name"], 40),
                    short(&args["role"], 30),
                    short(&args["model_id"], 60)
                ),
            ),
            "hub_retire_agent" => (
                "hub",
                Info,
                format!("Agent {} retired", n(&args["agent_id"])),
            ),
            "hub_request_claim" => (
                "hub",
                Info,
                format!(
                    "{who} claimed {} for {}",
                    short(&args["path"], 120),
                    short(&args["claim_type"], 20)
                ),
            ),
            "hub_release_claim" => (
                "hub",
                Info,
                format!("{who} released claim #{}", args["claim_id"]),
            ),
            "hub_revoke_claim" => ("hub", Info, format!("Claim #{} revoked", args["claim_id"])),
            "hub_renew_claim" => (
                "hub",
                Trace,
                format!("{who} renewed claim #{}", args["claim_id"]),
            ),
            "hub_send_message" => {
                let to = match args["to"]["kind"].as_str() {
                    Some("agent") => n(&args["to"]["id"]),
                    Some("room") => format!("room {}", short(&args["to"]["id"], 40)),
                    _ => "?".into(),
                };
                (
                    "hub",
                    Info,
                    format!(
                        "{who} → {to}: {} message ({} bytes; see the message stream)",
                        short(&args["message_type"], 20),
                        args["content_len"]
                    ),
                )
            }
            "hub_approve_message" => (
                "approval",
                Ok,
                format!("You released held message #{}", args["message_id"]),
            ),
            "hub_reject_message" => (
                "approval",
                Warn,
                format!("You rejected held message #{}", args["message_id"]),
            ),
            "hub_propose_decision" => (
                "hub",
                Info,
                format!(
                    "{who} proposed a decision: {}",
                    short(&args["summary"], 160)
                ),
            ),
            "hub_accept_decision" | "hub_reject_decision" => (
                "approval",
                Info,
                format!(
                    "You {} decision #{}{}",
                    if k == "hub_accept_decision" {
                        "accepted"
                    } else {
                        "rejected"
                    },
                    args["decision_id"],
                    args["reason"]
                        .as_str()
                        .filter(|r| !r.is_empty())
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ),
            ),
            "hub_create_task" => (
                "hub",
                Info,
                format!("{who} created a task: {}", short(&args["description"], 160)),
            ),
            "hub_assign_task" => (
                "hub",
                Info,
                format!(
                    "Task #{} assigned to {}",
                    args["task_id"],
                    n(&args["agent_id"])
                ),
            ),
            "hub_update_task_status" => (
                "hub",
                Info,
                format!(
                    "{who} set task #{} to {}",
                    args["task_id"],
                    short(&args["status"], 20)
                ),
            ),
            "hub_register_artifact" | "hub_register_evidence" => (
                "verify",
                Trace,
                format!(
                    "Evidence stored: {}",
                    short(
                        args.get("path")
                            .or(args.get("name"))
                            .unwrap_or(&Value::Null),
                        120
                    )
                ),
            ),
            "hub_merge_enqueue" => (
                "merge",
                Info,
                format!(
                    "{} submitted its work to the merge queue",
                    n(&args["agent_id"])
                ),
            ),
            "hub_merge_update" => (
                "merge",
                Trace,
                format!(
                    "Merge #{} is now {}",
                    args["seq"],
                    short(&args["status"], 20)
                ),
            ),
            _ if k.starts_with("hub_") => {
                ("hub", Info, format!("{who}: {}", k[4..].replace('_', " ")))
            }
            _ => ("other", Info, format!("{k}: {}", short(d, 200))),
        }
    }
}

fn s_opt(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A plain-JSON view of the cost meter's formula, for the UI.
pub fn formula() -> Value {
    json!(
        "spend = input tokens × price_in_per_mtok / 1e6 + output tokens × price_out_per_mtok / 1e6, with the prices from each agent's roles file"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, kind: &str, data: Value) -> AuditEntry {
        AuditEntry {
            seq,
            ts_ms: 1000 + seq,
            kind: kind.into(),
            data,
            prev: String::new(),
            hash: String::new(),
        }
    }

    fn team() -> AuditEntry {
        entry(
            0,
            "team_started",
            json!({"schedule": "sequential", "agents": [
                {"name": "alpha", "agent_id": 1, "role": "executor", "model_id": "m-a",
                 "family": "fa", "prices": {"executor": [3.0, 15.0]}},
            ], "verify": {"kind": "model", "prices": {"reviewer": [1.0, 2.0]}}}),
        )
    }

    #[test]
    fn cost_is_recomputed_from_prices_and_mismatches_are_reported() {
        let mut m = Model::default();
        m.apply(&team());
        m.apply(&entry(
            1,
            "session_started",
            json!({"session_id": "s1", "agent": "alpha"}),
        ));
        m.apply(&entry(
            2,
            "usage",
            json!({"session_id": "s1", "agent": "alpha", "role": "executor",
            "model_id": "m-a", "input_tokens": 1000, "output_tokens": 100, "cost_usd": 0.0045}),
        ));
        // A lying runtime: 10x the real price.
        m.apply(&entry(
            3,
            "usage",
            json!({"session_id": "s1", "agent": "alpha", "role": "executor",
            "model_id": "m-a", "input_tokens": 1000, "output_tokens": 100, "cost_usd": 0.045}),
        ));
        m.apply(&entry(
            4,
            "usage",
            json!({"session_id": "r1", "agent": "reviewer", "role": "reviewer",
            "model_id": "m-r", "input_tokens": 500, "output_tokens": 50, "cost_usd": 0.0006}),
        ));
        let a = &m.cost.per_agent["alpha"];
        assert_eq!((a.calls, a.tokens_in, a.tokens_out), (2, 2000, 200));
        assert!(same_usd(a.computed_usd, 0.009));
        assert!(same_usd(m.cost.per_agent["reviewer"].spend_usd, 0.0006));
        assert!(same_usd(m.cost.total.spend_usd, 0.0096));
        assert_eq!(m.cost.discrepancies.len(), 1, "{:?}", m.cost.discrepancies);
        // The session summary disagrees with its usage entries: reported.
        m.apply(&entry(
            5,
            "session_finished",
            json!({"session_id": "s1", "agent": "alpha",
            "done": true, "tokens_in": 2000, "tokens_out": 200, "cost_usd": 0.01}),
        ));
        assert_eq!(m.cost.discrepancies.len(), 2);
    }

    #[test]
    fn roster_status_follows_the_log() {
        let mut m = Model::default();
        m.apply(&team());
        let st = |m: &Model, pending: &[Value]| m.roster(&[], &[], pending)[0].status;
        assert_eq!(st(&m, &[]), Status::Waiting);
        m.apply(&entry(
            1,
            "session_started",
            json!({"session_id": "s1", "agent": "alpha"}),
        ));
        m.apply(&entry(
            2,
            "turn_started",
            json!({"session_id": "s1", "agent": "alpha", "turn": 1}),
        ));
        assert_eq!(st(&m, &[]), Status::Thinking);
        assert_eq!(
            st(&m, &[json!({"agent": "alpha", "summary": "write x"})]),
            Status::Blocked
        );
        m.apply(&entry(
            3,
            "turn_finished",
            json!({"session_id": "s1", "agent": "alpha", "turn": 1}),
        ));
        assert_eq!(st(&m, &[]), Status::Waiting);
        m.apply(&entry(
            4,
            "session_finished",
            json!({"session_id": "s1", "agent": "alpha",
            "done": false, "failed": "gave up", "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0}),
        ));
        assert_eq!(st(&m, &[]), Status::Failed);
    }

    #[test]
    fn unknown_kinds_are_kept_and_hub_callers_attributed() {
        let mut m = Model::default();
        m.apply(&team());
        m.apply(&entry(1, "something_new", json!({"x": 1})));
        m.apply(&entry(
            2,
            "hub_request_claim",
            json!({"caller": {"kind": "agent", "id": 1},
            "args": {"path": "src/a.py", "claim_type": "edit"}, "rows": []}),
        ));
        assert_eq!(m.events.len(), 3);
        assert!(m.events[1].text.contains("something_new"));
        assert_eq!(m.events[2].agent.as_deref(), Some("alpha"));
        assert!(
            m.events[2].text.contains("src/a.py"),
            "{}",
            m.events[2].text
        );
    }
}
