//! The collaboration dashboard (spec §12, ADR-0025): a loopback-only web
//! UI served by the backend itself, either inside `anchor team run --ui`
//! (live: approvals are answered here), by `anchor ui` (after a run), or
//! by `anchor app` (the desktop app, ADR-0028: the user opens projects and
//! runs tasks from the page; see [`app`]).
//!
//! Security model (threat model, "Dashboard"):
//! * bound to `127.0.0.1` only, through the Hub's hardened HTTP core
//!   (size limits, deadlines, one request per connection);
//! * `Host` must be exactly `127.0.0.1:<port>` or `localhost:<port>`
//!   (DNS rebinding), and a request carrying `Origin` must come from that
//!   same origin (cross-site requests);
//! * every `/api/` call needs `Authorization: Bearer <token>`, a 256-bit
//!   per-launch token handed over in the URL *fragment*, which browsers
//!   never send to servers or in `Referer`. No cookies, so no ambient
//!   authority for a forged request to ride on;
//! * a strict Content-Security-Policy; agent-written text is only ever
//!   rendered as text;
//! * every action is audited (`ui_action`) and goes through the same
//!   paths as the CLI (Hub user calls, `user_initiated` rollback).
//!
//! The user's own views do not write audit entries (they are not
//! inter-agent interactions); the Hub's agent-facing reads still do.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anchor_hub::http::{Request, Response, ServerConfig, ServerHandle, Service};
use anchor_hub::{Caller, Hub, MergeEntry, MergeStatus};
use anchor_policy::{AuditLog, Tailer};
use anchor_snapshot::Id;
use serde_json::{Value, json};

use crate::mediator::{Mediator, SharedStore, lock_store};

pub mod app;
mod assets;
pub mod broker;
pub mod model;
pub mod pair;
pub mod project_tests;
pub mod snapshots;

pub use broker::{Broker, UiInteraction};

/// What the dashboard reads.
pub struct Sources {
    /// The main workspace's canonical root.
    pub workspace: PathBuf,
    pub audit: Arc<AuditLog>,
    pub hub: Option<Arc<Hub>>,
    /// The snapshot store, if this process holds it.
    pub store: Option<SharedStore>,
    /// Why the store is unavailable, when it is.
    pub store_note: Option<String>,
    /// Approvals answered from the dashboard (live runs).
    pub broker: Option<Arc<Broker>>,
    /// `live` (inside a team run), `inspect` (`anchor ui`) or `app`.
    pub mode: &'static str,
}

impl std::fmt::Debug for Sources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sources")
            .field("workspace", &self.workspace)
            .field("mode", &self.mode)
            .finish()
    }
}

struct Feed {
    tailer: Tailer,
    model: model::Model,
    error: Option<String>,
}

/// The server: its credential, and the project it currently shows.
struct Server {
    token: String,
    port: u16,
    /// The dashboard's data; swapped when the app opens another project.
    project: RwLock<Option<Arc<State>>>,
    /// Set in app mode (`anchor app`).
    app: Option<Arc<app::App>>,
}

/// One project's dashboard data.
struct State {
    src: Sources,
    feed: Mutex<Feed>,
    /// The main workspace's mediator, when no run holds it: enables
    /// one-click rollback.
    workspace: Mutex<Option<Mediator>>,
    running: AtomicBool,
    diffs: Mutex<BTreeMap<(Id, Id), Vec<snapshots::FileChange>>>,
    /// Rollback confirmations: nonce → (target id, ref, issued).
    confirms: Mutex<BTreeMap<String, (Id, String, Instant)>>,
}

/// A running dashboard; dropping it stops the server and refuses every
/// approval still pending.
pub struct Ui {
    state: Arc<Server>,
    server: Option<ServerHandle>,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui")
            .field("port", &self.state.port)
            .finish()
    }
}

fn random_hex(n: usize) -> std::io::Result<String> {
    let mut raw = vec![0u8; n];
    getrandom::fill(&mut raw)
        .map_err(|e| std::io::Error::other(format!("no OS randomness: {e}")))?;
    Ok(anchor_policy::audit::hex(&raw))
}

impl State {
    fn new(src: Sources) -> State {
        let path = src.audit.path().to_path_buf();
        State {
            feed: Mutex::new(Feed {
                tailer: Tailer::new(&path),
                model: model::Model::default(),
                error: None,
            }),
            src,
            workspace: Mutex::new(None),
            running: AtomicBool::new(false),
            diffs: Mutex::new(BTreeMap::new()),
            confirms: Mutex::new(BTreeMap::new()),
        }
    }
}

/// Start the dashboard on `127.0.0.1:port` (0: any free port).
pub fn start(src: Sources, port: u16) -> std::io::Result<Ui> {
    serve(Some(Arc::new(State::new(src))), None, port)
}

/// Start the desktop app's server (`anchor app`): no project until the
/// user opens one.
pub fn start_app(app: Arc<app::App>, port: u16) -> std::io::Result<Ui> {
    serve(None, Some(app), port)
}

fn serve(
    project: Option<Arc<State>>,
    app: Option<Arc<app::App>>,
    port: u16,
) -> std::io::Result<Ui> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(Server {
        token: random_hex(32)?,
        port,
        project: RwLock::new(project),
        app,
    });
    let cfg = ServerConfig {
        // Long polls hold a worker each.
        workers: 12,
        max_body_bytes: 64 << 10,
        ..ServerConfig::default()
    };
    let server = anchor_hub::http::serve_on(listener, state.clone(), cfg)?;
    Ok(Ui {
        state,
        server: Some(server),
    })
}

impl Ui {
    pub fn port(&self) -> u16 {
        self.state.port
    }

    pub fn token(&self) -> &str {
        &self.state.token
    }

    /// The URL to open; the token travels in the fragment.
    pub fn url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/#t={}",
            self.state.port, self.state.token
        )
    }

    /// Whether a team run is in progress (rollback is refused meanwhile).
    pub fn set_running(&self, running: bool) {
        if let Some(p) = self.state.current() {
            p.running.store(running, Ordering::SeqCst);
        }
    }

    /// Hand the main workspace to the dashboard (enables rollback).
    pub fn attach_workspace(&self, m: Mediator) {
        if let Some(p) = self.state.current() {
            *p.workspace.lock().unwrap_or_else(|p| p.into_inner()) = Some(m);
        }
    }

    pub fn detach_workspace(&self) -> Option<Mediator> {
        self.state
            .current()?
            .workspace
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Block until the server stops.
    pub fn wait(mut self) {
        if let Some(s) = self.server.take() {
            s.wait();
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        if let Some(app) = &self.state.app {
            app.shutdown();
        }
        if let Some(p) = self.state.current()
            && let Some(b) = &p.src.broker
        {
            b.close();
        }
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

// -- request plumbing ---------------------------------------------------------

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

fn harden(mut r: Response) -> Response {
    for (n, v) in [
        ("Content-Security-Policy", CSP),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        ("Cross-Origin-Opener-Policy", "same-origin"),
        ("Cross-Origin-Resource-Policy", "same-origin"),
    ] {
        r.headers.push((n, v.to_string()));
    }
    r
}

/// Constant-time equality.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hi = hexval(*b.get(i + 1)?)?;
                let lo = hexval(*b.get(i + 2)?)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    let s = String::from_utf8(out).ok()?;
    (!s.chars().any(char::is_control)).then_some(s)
}

/// Strict query parsing: well-formed percent-encoding, no control
/// characters, no repeated keys.
fn parse_query(q: &str) -> Option<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (pct_decode(k)?, pct_decode(v)?);
        if m.insert(k, v).is_some() {
            return None;
        }
    }
    Some(m)
}

fn bad(msg: &str) -> Response {
    Response::error(400, msg)
}

fn ok(v: Value) -> Response {
    Response::json(200, &v)
}

impl Service for Server {
    fn handle(&self, req: &Request) -> Response {
        harden(self.route(req))
    }
}

impl Server {
    fn current(&self) -> Option<Arc<State>> {
        self.project
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set_current(&self, p: Option<Arc<State>>) -> Option<Arc<State>> {
        std::mem::replace(
            &mut *self.project.write().unwrap_or_else(|p| p.into_inner()),
            p,
        )
    }

    fn get(&self, api: &str, q: &BTreeMap<String, String>) -> Response {
        if let (Some(app), Some(rest)) = (&self.app, api.strip_prefix("app/")) {
            return app.get(rest, q);
        }
        match self.current() {
            Some(p) => p.get(api, q),
            // The app before a project is open.
            None if api == "overview" => ok(json!({ "mode": "app", "project": false })),
            None => Response::error(409, "open a project first"),
        }
    }

    fn post(&self, api: &str, body: &Value) -> Response {
        if let (Some(app), Some(rest)) = (&self.app, api.strip_prefix("app/")) {
            return app.post(self, rest, body);
        }
        match self.current() {
            Some(p) => p.post(api, body),
            None => Response::error(409, "open a project first"),
        }
    }

    fn origin_ok(&self, req: &Request) -> bool {
        let ours = |v: &str, scheme: &str| {
            v == format!("{scheme}127.0.0.1:{}", self.port)
                || v == format!("{scheme}localhost:{}", self.port)
        };
        let host_ok = req.header("host").is_some_and(|h| ours(h, ""));
        // A request carrying Origin must come from this very origin.
        let origin_ok = req
            .headers
            .iter()
            .filter(|(n, _)| n == "origin")
            .all(|(_, v)| ours(v, "http://"));
        // Browsers mark cross-site requests; refuse them outright.
        let site_ok = req
            .headers
            .iter()
            .filter(|(n, _)| n == "sec-fetch-site")
            .all(|(_, v)| v == "same-origin" || v == "none");
        host_ok && origin_ok && site_ok
    }

    fn authed(&self, req: &Request) -> bool {
        req.header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| same(t.trim().as_bytes(), self.token.as_bytes()))
    }

    fn route(&self, req: &Request) -> Response {
        if !self.origin_ok(req) {
            return Response::error(403, "cross-origin or unexpected Host");
        }
        let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        if !path.starts_with('/') {
            return bad("bad path");
        }
        let Some(q) = parse_query(query) else {
            return bad("malformed query string");
        };
        if let Some(api) = path.strip_prefix("/api/") {
            if !self.authed(req) {
                let mut r = Response::error(401, "a valid bearer token is required");
                r.headers.push(("WWW-Authenticate", "Bearer".into()));
                return r;
            }
            return match req.method.as_str() {
                "GET" => self.get(api, &q),
                "POST" => {
                    if req
                        .header("content-type")
                        .map(|c| c.split(';').next().unwrap_or("").trim())
                        != Some("application/json")
                    {
                        return Response::error(415, "application/json required");
                    }
                    match serde_json::from_slice::<Value>(&req.body) {
                        Ok(body) if body.is_object() => self.post(api, &body),
                        _ => bad("the body must be a JSON object"),
                    }
                }
                _ => Response::error(405, "method not allowed"),
            };
        }
        if req.method != "GET" {
            return Response::error(405, "method not allowed");
        }
        let name = if path == "/" { "/index.html" } else { path };
        match assets::ASSETS.iter().find(|(p, _, _)| *p == name) {
            Some((_, mime, bytes)) => Response {
                status: 200,
                content_type: mime,
                headers: Vec::new(),
                body: bytes.to_vec(),
            },
            None => Response::error(404, "not found"),
        }
    }
}

impl State {
    // -- the model -------------------------------------------------------------

    /// Fold new audit entries in. Returns the number of events.
    fn refresh(&self) -> usize {
        let mut f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        if f.error.is_none() {
            if f.model.events.is_empty()
                && let Some(h) = &self.src.hub
                && let Ok(agents) = h.agents()
            {
                f.model.learn_agents(&agents);
            }
            match f.tailer.poll() {
                Ok(entries) => {
                    for e in &entries {
                        f.model.apply(e);
                    }
                }
                Err(e) => f.error = Some(e.to_string()),
            }
        }
        f.model.events.len()
    }

    fn with_model<T>(&self, g: impl FnOnce(&Feed) -> T) -> T {
        self.refresh();
        let f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        g(&f)
    }

    fn names(&self) -> BTreeMap<i64, String> {
        self.src
            .hub
            .as_ref()
            .and_then(|h| h.agents().ok())
            .map(|a| a.into_iter().map(|a| (a.id, a.display_name)).collect())
            .unwrap_or_default()
    }

    fn pending(&self) -> Vec<Value> {
        match &self.src.broker {
            Some(b) => broker::pending_json(b),
            // Inspecting afterwards: what the log says was left unanswered.
            None => self.with_model(|f| f.model.pending.values().cloned().collect()),
        }
    }

    fn chain(&self, f: &Feed) -> Value {
        json!({ "verified_entries": f.tailer.verified(), "error": f.error })
    }

    fn merges(&self) -> Vec<MergeEntry> {
        self.src
            .hub
            .as_ref()
            .and_then(|h| h.merges().ok())
            .unwrap_or_default()
    }

    fn can_rollback(&self) -> Result<(), &'static str> {
        if self.running.load(Ordering::SeqCst) {
            return Err("a run is in progress; roll back after it finishes");
        }
        if self
            .workspace
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none()
        {
            return Err("this dashboard does not hold the workspace (another process does)");
        }
        Ok(())
    }

    // -- GET -------------------------------------------------------------------

    fn get(&self, api: &str, q: &BTreeMap<String, String>) -> Response {
        let num = |k: &str| q.get(k).and_then(|v| v.parse::<i64>().ok());
        match api {
            "overview" => self.overview(),
            "events" => self.events(num("after").unwrap_or(-1), q.contains_key("wait")),
            "claims" => self.claims(),
            "messages" => self.messages(num("after").unwrap_or(0)),
            "decisions" => self.decisions(),
            "tasks" => self.hub_list(|h| h.view_tasks().map(|v| json!(v))),
            "merges" => self.merge_list(),
            "merge" => match num("seq") {
                Some(seq) => self.merge_detail(seq),
                None => bad("seq required"),
            },
            "evidence" => match q.get("path") {
                Some(p) => self.evidence(p),
                None => bad("path required"),
            },
            "cost" => self.with_model(|f| {
                ok(json!({ "cost": f.model.cost, "formula": model::formula(), "team": f.model.team }))
            }),
            "snapshots" => self.snapshots(),
            "snapshot-diff" => match (q.get("a"), q.get("b"), q.get("path")) {
                (Some(a), Some(b), Some(p)) => self.snapshot_diff(a, b, p),
                _ => bad("a, b and path required"),
            },
            _ => Response::error(404, "no such endpoint"),
        }
    }

    fn overview(&self) -> Response {
        let pending = self.pending();
        let merges = self.merges();
        let agents = self
            .src
            .hub
            .as_ref()
            .and_then(|h| h.agents().ok())
            .unwrap_or_default();
        self.with_model(|f| {
            let failures = f
                .model
                .events
                .iter()
                .filter(|e| e.level == model::Level::Fail)
                .count();
            ok(json!({
                "mode": self.src.mode,
                "running": self.running.load(Ordering::SeqCst),
                "workspace": self.src.workspace,
                "chain": self.chain(f),
                "team": f.model.team,
                "roster": f.model.roster(&agents, &merges, &pending),
                "pending": pending,
                "cost_total": f.model.cost.total,
                "cost_discrepancies": f.model.cost.discrepancies.len(),
                "failures": failures,
                "events": f.model.events.len(),
                "last_seq": f.model.events.last().map(|e| e.seq),
                "store": { "available": self.src.store.is_some(), "note": self.src.store_note },
                "rollback": self.can_rollback().err(),
                "hub": self.src.hub.is_some(),
            }))
        })
    }

    /// Events after `after`; with `wait`, hold the request up to 8 s for
    /// new ones (long poll; the dashboard's live feed).
    fn events(&self, after: i64, wait: bool) -> Response {
        let deadline = Instant::now() + Duration::from_secs(8);
        let pending_before = self.src.broker.as_ref().map(|b| b.pending().len());
        loop {
            self.refresh();
            let f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
            let start = f.model.events.partition_point(|e| (e.seq as i64) <= after);
            let fresh = &f.model.events[start..];
            let pending_changed =
                self.src.broker.as_ref().map(|b| b.pending().len()) != pending_before;
            if !fresh.is_empty()
                || !wait
                || Instant::now() >= deadline
                || pending_changed
                || f.error.is_some()
            {
                let page: Vec<&model::Event> = fresh.iter().take(2000).collect();
                return ok(json!({
                    "events": page,
                    "more": fresh.len() > page.len(),
                    "chain": self.chain(&f),
                }));
            }
            drop(f);
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn hub_list(&self, g: impl FnOnce(&Hub) -> anchor_hub::Result<Value>) -> Response {
        match &self.src.hub {
            None => ok(json!([])),
            Some(h) => match g(h) {
                Ok(v) => ok(v),
                Err(e) => Response::error(500, &e.to_string()),
            },
        }
    }

    fn claims(&self) -> Response {
        let Some(hub) = &self.src.hub else {
            return ok(
                json!({ "claims": [], "files": list_files(&self.src.workspace), "conflicts": [] }),
            );
        };
        let names = self.names();
        let now = hub.now_ms();
        let claims = match hub.view_claims() {
            Ok(c) => c,
            Err(e) => return Response::error(500, &e.to_string()),
        };
        let claims: Vec<Value> = claims
            .into_iter()
            .map(|c| {
                let mut v = json!(c);
                v["agent"] = json!(names.get(&c.agent_id));
                v["lease_expired"] = json!(c.lease_expires_at <= now);
                v
            })
            .collect();
        // Conflicts: refused claim requests and merges rejected for claim
        // or path conflicts.
        let conflicts = self.with_model(|f| {
            f.model
                .events
                .iter()
                .filter_map(|e| match e.kind.as_str() {
                    "hub_denied" if e.data["method"] == "request_claim" => Some(json!({
                        "seq": e.seq, "agent": e.agent, "path": e.data["args"]["path"],
                        "reason": e.data["error"], "kind": "claim_refused",
                    })),
                    "merge_rejected"
                        if matches!(e.data["feedback"]["reason"].as_str(), Some("conflict" | "claims")) =>
                    {
                        Some(json!({
                            "seq": e.seq, "agent": e.agent, "merge": e.data["seq"],
                            "paths": e.data["feedback"]["paths"], "reason": e.data["feedback"]["reason"],
                            "detail": e.data["feedback"], "kind": "merge_conflict",
                        }))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        ok(
            json!({ "claims": claims, "files": list_files(&self.src.workspace), "conflicts": conflicts, "now_ms": now }),
        )
    }

    fn messages(&self, after: i64) -> Response {
        let names = self.names();
        self.hub_list(|h| {
            Ok(json!(
                h.view_messages(after, 500)?
                    .into_iter()
                    .map(|m| {
                        let mut v = json!(m);
                        v["from"] = json!(
                            m.from_agent_id
                                .map(|i| names
                                    .get(&i)
                                    .cloned()
                                    .unwrap_or_else(|| format!("agent #{i}")))
                                .unwrap_or_else(|| "user / backend".into())
                        );
                        v["to"] = json!(match (&m.to_agent_id, &m.room_id) {
                            (Some(i), _) => names
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("agent #{i}")),
                            (None, Some(r)) => format!("room {r}"),
                            _ => "?".into(),
                        });
                        v
                    })
                    .collect::<Vec<_>>()
            ))
        })
    }

    fn decisions(&self) -> Response {
        let names = self.names();
        self.hub_list(|h| {
            Ok(json!(
                h.view_decisions()?
                    .into_iter()
                    .map(|d| {
                        let mut v = json!(d);
                        v["agent"] = json!(d.agent_id.and_then(|i| names.get(&i).cloned()));
                        v
                    })
                    .collect::<Vec<_>>()
            ))
        })
    }

    fn merge_list(&self) -> Response {
        let names = self.names();
        let merges = self.merges();
        // Expected merge time: queue position × median gate time so far.
        let mut durations: Vec<u64> = self.with_model(|f| {
            let mut enq: BTreeMap<i64, u64> = BTreeMap::new();
            let mut out = Vec::new();
            for e in &f.model.events {
                match e.kind.as_str() {
                    "hub_merge_update" if e.data["args"]["status"] == "queued" => {}
                    "hub_merge_enqueue" => {
                        if let Some(id) = e.data["rows"]
                            .as_array()
                            .and_then(|r| r.first())
                            .and_then(|r| r["id"].as_i64())
                        {
                            enq.insert(id, e.ts_ms);
                        }
                    }
                    "merge_applied" | "merge_rejected" => {
                        if let Some(t0) = e.data["seq"].as_i64().and_then(|s| enq.get(&s)) {
                            out.push(e.ts_ms.saturating_sub(*t0));
                        }
                    }
                    _ => {}
                }
            }
            out
        });
        durations.sort_unstable();
        let median = durations.get(durations.len() / 2).copied();
        let mut position = 0u64;
        let rows: Vec<Value> = merges
            .iter()
            .map(|m| {
                let mut v = json!(m);
                v["agent"] = json!(m.agent_id.and_then(|i| names.get(&i).cloned()));
                let waiting = matches!(m.status, MergeStatus::Queued | MergeStatus::Applying);
                if waiting {
                    position += 1;
                }
                v["expected_ms"] = json!(if waiting {
                    median.map(|d| d * position)
                } else {
                    None
                });
                v["evidence"] = m.feedback["verification"]["evidence"]
                    .as_str()
                    .or(m.feedback["detail"]["evidence"].as_str())
                    .map_or(Value::Null, |g| json!(g));
                let codes: Vec<&str> = m.feedback["detail"]["findings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f["code"].as_str())
                    .collect();
                v["findings"] = json!(codes);
                v["caught_later"] = json!(codes.contains(&"fails_on_main"));
                v
            })
            .collect();
        ok(json!({ "merges": rows, "median_gate_ms": median }))
    }

    fn merge_detail(&self, seq: i64) -> Response {
        let Some(hub) = &self.src.hub else {
            return Response::error(404, "no Hub");
        };
        let Some(m) = self.merges().into_iter().find(|m| m.seq == seq) else {
            return Response::error(404, "no such merge");
        };
        let group = m.feedback["verification"]["evidence"]
            .as_str()
            .or(m.feedback["detail"]["evidence"].as_str())
            .map(str::to_string);
        let mut artifacts = Vec::new();
        let mut review_input = Value::Null;
        let mut decision = Value::Null;
        if let Some(g) = &group
            && let Ok(list) = hub.list_artifacts(Caller::User, Some(&format!("evidence:{g}/")))
        {
            for a in list {
                if a.path.ends_with("/review-input.json") || a.path.ends_with("/decision.json") {
                    let v = hub
                        .read_evidence(Caller::User, &a)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                        .unwrap_or(Value::Null);
                    if a.path.ends_with("/decision.json") {
                        decision = v;
                    } else {
                        review_input = v;
                    }
                }
                artifacts.push(json!(a));
            }
        }
        // The diff side: hunks from the review input, or the change list.
        let changes = review_input["changes"].clone();
        let mut mv = json!(m);
        mv["agent"] = json!(m.agent_id.and_then(|i| self.names().get(&i).cloned()));
        ok(json!({
            "merge": mv,
            "evidence_group": group,
            "artifacts": artifacts,
            "decision": decision,
            "claims": review_input["claims"],
            "changes": changes,
            "tests": review_input["tests"],
            "commands": review_input["commands"],
            "coverage": review_input["coverage"],
        }))
    }

    fn evidence(&self, path: &str) -> Response {
        let Some(hub) = &self.src.hub else {
            return Response::error(404, "no Hub");
        };
        if !path.starts_with("evidence:") {
            return bad("not an evidence path");
        }
        let list = match hub.list_artifacts(Caller::User, Some(path)) {
            Ok(l) => l,
            Err(e) => return Response::error(500, &e.to_string()),
        };
        let Some(a) = list.into_iter().find(|a| a.path == path) else {
            return Response::error(404, "no such evidence");
        };
        match hub.read_evidence(Caller::User, &a) {
            Ok(b) => {
                let cap = 1 << 20;
                let text = String::from_utf8_lossy(&b[..b.len().min(cap)]).into_owned();
                ok(json!({ "artifact": a, "text": text, "truncated": b.len() > cap }))
            }
            Err(e) => Response::error(500, &e.to_string()),
        }
    }

    fn snapshots(&self) -> Response {
        let Some(store) = &self.src.store else {
            return ok(json!({ "available": false, "note": self.src.store_note, "snapshots": [] }));
        };
        let names = self.names();
        let merge_agents: BTreeMap<i64, String> = self
            .merges()
            .into_iter()
            .filter_map(|m| Some((m.seq, names.get(&m.agent_id?)?.clone())))
            .collect();
        let st = lock_store(store);
        let mut cache = self.diffs.lock().unwrap_or_else(|p| p.into_inner());
        match snapshots::timeline(&st, &names, &merge_agents, &mut cache) {
            Ok(rows) => ok(
                json!({ "available": true, "snapshots": rows, "rollback": self.can_rollback().err() }),
            ),
            Err(e) => Response::error(500, &e),
        }
    }

    fn snapshot_diff(&self, a: &str, b: &str, path: &str) -> Response {
        let Some(store) = &self.src.store else {
            return Response::error(409, "the snapshot store is not available");
        };
        let st = lock_store(store);
        let r = snapshots::resolve(&st, a)
            .and_then(|a| Ok((a, snapshots::resolve(&st, b)?)))
            .and_then(|(a, b)| snapshots::file_diff(&st, &a, &b, path));
        match r {
            Ok(text) => ok(json!({ "diff": text })),
            Err(e) => bad(&e),
        }
    }

    // -- POST (actions) --------------------------------------------------------

    fn audit_action(&self, action: &str, args: &Value) -> Result<(), Response> {
        self.src
            .audit
            .append(
                "ui_action",
                json!({ "action": action, "args": args, "via": "ui" }),
            )
            .map(|_| ())
            .map_err(|e| Response::error(500, &format!("audit log: {e}")))
    }

    fn post(&self, api: &str, body: &Value) -> Response {
        let field = |k: &str| &body[k];
        let allowed = |keys: &[&str]| {
            body.as_object()
                .is_some_and(|m| m.keys().all(|k| keys.contains(&k.as_str())))
        };
        match api {
            "approvals/answer" => {
                if !allowed(&["id", "choice"]) {
                    return bad("unexpected fields");
                }
                let (Some(id), Some(choice)) = (field("id").as_u64(), field("choice").as_str())
                else {
                    return bad("id and choice required");
                };
                let Some(b) = &self.src.broker else {
                    return Response::error(409, "this dashboard does not answer approvals");
                };
                if let Err(r) = self.audit_action("answer_approval", body) {
                    return r;
                }
                match b.answer(id, choice) {
                    Ok(()) => ok(json!({ "ok": true })),
                    Err(e) => Response::error(409, &e),
                }
            }
            "messages/decide" => {
                if !allowed(&["id", "approve"]) {
                    return bad("unexpected fields");
                }
                let (Some(id), Some(approve)) = (field("id").as_i64(), field("approve").as_bool())
                else {
                    return bad("id and approve required");
                };
                let Some(h) = &self.src.hub else {
                    return Response::error(409, "no Hub");
                };
                if let Err(r) = self.audit_action("decide_message", body) {
                    return r;
                }
                let r = if approve {
                    h.approve_message(Caller::User, id)
                } else {
                    h.reject_message(Caller::User, id)
                };
                match r {
                    Ok(m) => ok(json!(m)),
                    Err(e) => Response::error(409, &e.to_string()),
                }
            }
            "decisions/decide" => {
                if !allowed(&["id", "accept", "reason"]) {
                    return bad("unexpected fields");
                }
                let (Some(id), Some(accept)) = (field("id").as_i64(), field("accept").as_bool())
                else {
                    return bad("id and accept required");
                };
                let reason = field("reason").as_str().unwrap_or("");
                if reason.len() > 2000 {
                    return bad("reason too long");
                }
                let Some(h) = &self.src.hub else {
                    return Response::error(409, "no Hub");
                };
                if let Err(r) = self.audit_action("decide", body) {
                    return r;
                }
                match h.decide(Caller::User, id, accept, reason) {
                    Ok(d) => ok(json!(d)),
                    Err(e) => Response::error(409, &e.to_string()),
                }
            }
            "rollback/prepare" => {
                if !allowed(&["ref"]) {
                    return bad("unexpected fields");
                }
                let Some(r) = field("ref").as_str() else {
                    return bad("ref required");
                };
                self.rollback_prepare(r)
            }
            "rollback/confirm" => {
                if !allowed(&["nonce"]) {
                    return bad("unexpected fields");
                }
                let Some(n) = field("nonce").as_str() else {
                    return bad("nonce required");
                };
                self.rollback_confirm(n)
            }
            _ => Response::error(404, "no such action"),
        }
    }

    /// Step 1 of a rollback: what restoring `r` would change, and a
    /// one-time nonce (valid 5 minutes) that step 2 must present.
    fn rollback_prepare(&self, r: &str) -> Response {
        if let Err(e) = self.can_rollback() {
            return Response::error(409, e);
        }
        if r.starts_with("r-") || r == "HEAD" {
            return Response::error(409, "only main-workspace snapshots can be restored here");
        }
        let mut ws = self.workspace.lock().unwrap_or_else(|p| p.into_inner());
        let Some(m) = ws.as_mut() else {
            return Response::error(409, "the workspace is not available");
        };
        let id = {
            let st = m.snapshots();
            match snapshots::resolve(&st, r) {
                Ok(id) => id,
                Err(e) => return bad(&e),
            }
        };
        let preview = match m.preview_restore(&id) {
            Ok(p) => p.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            Err(e) => return Response::error(500, &e),
        };
        let nonce = match random_hex(16) {
            Ok(n) => n,
            Err(e) => return Response::error(500, &e.to_string()),
        };
        let mut c = self.confirms.lock().unwrap_or_else(|p| p.into_inner());
        c.retain(|_, (_, _, t)| t.elapsed() < Duration::from_secs(300));
        c.insert(nonce.clone(), (id, r.to_string(), Instant::now()));
        ok(json!({ "nonce": nonce, "ref": r, "id": id.hex(), "changes": preview }))
    }

    fn rollback_confirm(&self, nonce: &str) -> Response {
        if let Err(e) = self.can_rollback() {
            return Response::error(409, e);
        }
        let entry = self
            .confirms
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(nonce);
        let Some((id, r, t)) = entry.filter(|(_, _, t)| t.elapsed() < Duration::from_secs(300))
        else {
            return Response::error(409, "unknown or expired confirmation; prepare again");
        };
        let _ = t;
        if let Err(resp) = self.audit_action("rollback", &json!({ "ref": r, "id": id.hex() })) {
            return resp;
        }
        let mut ws = self.workspace.lock().unwrap_or_else(|p| p.into_inner());
        let Some(m) = ws.as_mut() else {
            return Response::error(409, "the workspace is not available");
        };
        match m.rollback_to_snapshot(&id) {
            Ok(ops) => ok(json!({ "ok": true, "operations": ops })),
            Err(e) => Response::error(500, &e),
        }
    }
}

/// The workspace's files for the claim map (bounded; never follows
/// symlinks; skips VCS and cache directories).
fn list_files(root: &Path) -> Vec<String> {
    const MAX: usize = 5000;
    const SKIP: &[&str] = &[
        ".git",
        ".hg",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
    ];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            let p = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                if depth < 16 && !SKIP.contains(&name.as_str()) {
                    stack.push((e.path(), p, depth + 1));
                }
            } else if ft.is_file() {
                out.push(p);
                if out.len() >= MAX {
                    out.sort();
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_parsed_strictly() {
        let q = parse_query("a=1&path=src%2Fx.py&b=").unwrap();
        assert_eq!(q["path"], "src/x.py");
        assert_eq!(q["b"], "");
        assert!(parse_query("a=1&a=2").is_none());
        assert!(parse_query("a=%zz").is_none());
        assert!(parse_query("a=%0a").is_none());
    }

    #[test]
    fn token_comparison() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
