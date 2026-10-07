//! Phase 6 done-when tests: the collaboration dashboard (spec §12, §14).
//!
//! * The server's hardening: token, Host, Origin, cross-site requests,
//!   security headers, no path escape.
//! * A scripted two-model collaboration (`examples/ui/team.toml`) driven
//!   *through the dashboard's API*: every approval is answered over HTTP,
//!   then the API is checked against the audit log and the Hub: every
//!   inter-agent interaction is visible and attributed, failures are shown
//!   with their evidence, and the cost meter equals tokens × the prices in
//!   the user's roles files, exactly.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

use anchor_backend::interact::Scripted;
use anchor_backend::open_workspace_in;
#[cfg(unix)]
use anchor_backend::session;
#[cfg(unix)]
use anchor_backend::team::{self, Team, TeamOptions};
use anchor_backend::ui::{self, Sources};
#[cfg(unix)]
use anchor_backend::ui::{Broker, UiInteraction};
#[cfg(unix)]
use anchor_policy::AuditLog;
use anchor_protocol::ResumeChoice;
use serde_json::{Value, json};

static SERIAL: Mutex<()> = Mutex::new(());

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ui")
}

#[cfg(unix)]
fn agent_bin() -> PathBuf {
    let p = session::default_agent_bin();
    if !p.exists() {
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "-q",
                "-p",
                "anchor-runtime",
                "--bin",
                "anchor-agent",
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }
    p
}

#[cfg(unix)]
fn python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), t).unwrap();
        }
    }
}

/// One raw HTTP/1.1 exchange: `(status, headers, body)`.
fn http(port: u16, raw: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

struct Client {
    port: u16,
    token: String,
}

impl Client {
    fn get(&self, path: &str) -> Value {
        let (st, _, body) = http(
            self.port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
                self.port, self.token
            ),
        );
        assert_eq!(st, 200, "GET {path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn post(&self, path: &str, v: &Value) -> (u16, Value) {
        let body = v.to_string();
        let (st, _, resp) = http(
            self.port,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\n\
                 Authorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                self.port,
                self.port,
                self.token,
                body.len()
            ),
        );
        (st, serde_json::from_str(&resp).unwrap_or(Value::Null))
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    ws: PathBuf,
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let ws = base.join("ws");
    copy_dir(&example().join("workspace"), &ws);
    Fx {
        _dir: dir,
        base,
        ws,
    }
}

#[test]
fn dashboard_is_token_gated_same_origin_and_hardened() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let f = fixture();
    let opened = open_workspace_in(
        &f.ws,
        &f.base.join("state"),
        None,
        Box::new(Scripted::new(false, ResumeChoice::FreshSession)),
        vec![],
    )
    .unwrap();
    let audit = opened.mediator.engine().audit_arc();
    let ui = ui::start(
        Sources {
            workspace: opened.mediator.workspace().root().to_path_buf(),
            audit: audit.clone(),
            hub: None,
            store: Some(opened.mediator.shared_store()),
            store_note: None,
            broker: None,
            mode: "inspect",
        },
        0,
    )
    .unwrap();
    let port = ui.port();
    let token = ui.token().to_string();
    assert!(
        ui.url().ends_with(&format!("/#t={token}")),
        "token travels in the fragment"
    );
    assert_eq!(token.len(), 64);
    let get = |path: &str, extra: &str| {
        http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}\r\n"),
        )
    };
    let auth = format!("Authorization: Bearer {token}\r\n");

    // No token, a wrong token, a token of the wrong length: refused.
    assert_eq!(get("/api/overview", "").0, 401);
    assert_eq!(
        get("/api/overview", "Authorization: Bearer nope\r\n").0,
        401
    );
    let flip = if token.ends_with('0') { '1' } else { '0' };
    let wrong = format!("Authorization: Bearer {}{flip}\r\n", &token[..63]);
    assert_eq!(get("/api/overview", &wrong).0, 401);
    assert_eq!(get("/api/overview", &auth).0, 200);

    // DNS rebinding: a foreign Host is refused, even with the token.
    let (st, _, _) = http(
        port,
        &format!("GET /api/overview HTTP/1.1\r\nHost: evil.example:{port}\r\n{auth}\r\n"),
    );
    assert_eq!(st, 403);
    // A cross-origin page: refused even with the token.
    assert_eq!(
        get(
            "/api/overview",
            &format!("{auth}Origin: http://evil.example\r\n")
        )
        .0,
        403
    );
    assert_eq!(get("/", "Sec-Fetch-Site: cross-site\r\n").0, 403);
    assert_eq!(
        get(
            "/api/overview",
            &format!("{auth}Origin: http://127.0.0.1:{port}\r\n")
        )
        .0,
        200
    );

    // Static files: served with the security headers, without the token.
    let (st, head, body) = get("/", "");
    assert_eq!(st, 200);
    assert!(body.contains("<html"), "{body}");
    let head = head.to_ascii_lowercase();
    for h in [
        "content-security-policy: default-src 'none'; script-src 'self'",
        "x-frame-options: deny",
        "referrer-policy: no-referrer",
        "x-content-type-options: nosniff",
    ] {
        assert!(head.contains(h), "missing {h} in {head}");
    }
    // Nothing outside the embedded table is reachable.
    for p in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/Cargo.toml",
        "/index.html/..",
    ] {
        assert_eq!(get(p, "").0, 404, "{p}");
    }
    // Writes need JSON and the token; unknown fields are refused.
    let c = Client { port, token };
    let (st, _) = c.post("/api/rollback/prepare", &json!({ "ref": "HEAD", "x": 1 }));
    assert_eq!(st, 400);
    let (st, _, _) = http(
        port,
        &format!(
            "POST /api/approvals/answer HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert_eq!(st, 401);
    // The user's views write nothing to the audit log.
    let before = audit.head();
    for p in [
        "/api/overview",
        "/api/events",
        "/api/claims",
        "/api/snapshots",
        "/api/cost",
    ] {
        c.get(p);
    }
    assert_eq!(audit.head(), before);
}

#[cfg(unix)]
fn script_tokens(path: &Path) -> (u64, u64) {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .fold((0, 0), |(i, o), s| {
            (
                i + s["prompt_tokens"].as_u64().unwrap(),
                o + s["completion_tokens"].as_u64().unwrap(),
            )
        })
}

#[cfg(unix)]
fn price(roles: &str, role: &str) -> (f64, f64) {
    let t: toml::Table = std::fs::read_to_string(example().join(roles))
        .unwrap()
        .parse()
        .unwrap();
    let r = &t["roles"][role];
    (
        r["price_in_per_mtok"].as_float().unwrap(),
        r["price_out_per_mtok"].as_float().unwrap(),
    )
}

#[cfg(unix)]
fn usd(tokens: (u64, u64), p: (f64, f64)) -> f64 {
    (tokens.0 as f64 * p.0 + tokens.1 as f64 * p.1) / 1_000_000.0
}

#[cfg(unix)]
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

/// The two-model collaboration, watched and steered through the API.
#[cfg(unix)]
#[test]
fn two_model_collaboration_through_the_dashboard() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !python() {
        eprintln!("python3 not found: skipped");
        return;
    }
    let f = fixture();
    let state = f.base.join("state");
    let broker = Broker::new();
    let interact = team::Shared::wrap(Box::new(UiInteraction::new(broker.clone())));
    let mut opened = open_workspace_in(
        &f.ws,
        &state,
        None,
        Box::new(team::Shared::new("merge", &interact)),
        vec![],
    )
    .unwrap();
    let audit = opened.mediator.engine().audit_arc();
    broker.set_audit(audit.clone());
    let root = opened.mediator.workspace().root().to_path_buf();
    let hub = team::open_hub(&opened.paths, &root, audit.clone()).unwrap();
    let dash = ui::start(
        Sources {
            workspace: root.clone(),
            audit: audit.clone(),
            hub: Some(hub.clone()),
            store: Some(opened.mediator.shared_store()),
            store_note: None,
            broker: Some(broker.clone()),
            mode: "live",
        },
        0,
    )
    .unwrap();
    let c = Client {
        port: dash.port(),
        token: dash.token().to_string(),
    };
    dash.set_running(true);

    // The user, in the dashboard: answer every gate as it appears, and
    // try a rollback mid-run (refused: the run holds the workspace).
    let done = AtomicBool::new(false);
    let team = Team::load(&example().join("team.toml")).unwrap();
    let (report, answered, kinds, refused_mid_run) = std::thread::scope(|s| {
        let user = s.spawn(|| {
            let mut answered = 0;
            let mut kinds = std::collections::BTreeSet::new();
            let mut refused = None;
            let mut after = -1i64;
            while !done.load(Ordering::SeqCst) {
                // The live feed (long poll) wakes us on new entries.
                let ev = c.get(&format!("/api/events?after={after}&wait=1"));
                if let Some(last) = ev["events"].as_array().and_then(|e| e.last()) {
                    after = last["seq"].as_i64().unwrap();
                }
                let o = c.get("/api/overview");
                for p in o["pending"].as_array().unwrap() {
                    kinds.insert(p["kind"].as_str().unwrap().to_string());
                    let blocked = o["roster"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["status"] == "blocked");
                    assert!(
                        blocked || p["kind"] == "merge",
                        "a pending tool approval shows its agent blocked: {o}"
                    );
                    let (st, _) = c.post(
                        "/api/approvals/answer",
                        &json!({ "id": p["id"], "choice": "approve" }),
                    );
                    assert_eq!(st, 200);
                    answered += 1;
                }
                if refused.is_none() {
                    refused = Some(c.post("/api/rollback/prepare", &json!({ "ref": "HEAD" })).0);
                }
            }
            (answered, kinds, refused)
        });
        let report = team::run(
            &mut opened,
            &team,
            TeamOptions {
                agent_bin: agent_bin(),
                state_base: state.clone(),
                auto_merge: false,
                checkpoint_threshold: 0.8,
                gate: Box::new(anchor_backend::queue::NoGate),
            },
            interact.clone(),
        );
        done.store(true, Ordering::SeqCst);
        let (a, k, r) = user.join().unwrap();
        (report.unwrap(), a, k, r)
    });
    dash.set_running(false);
    assert_eq!(
        refused_mid_run,
        Some(409),
        "no rollback while a run holds the workspace"
    );
    assert!(answered >= 3, "answered {answered}");
    assert!(
        kinds.contains("merge") && kinds.contains("tool"),
        "{kinds:?}"
    );
    for r in &report.agents {
        assert!(
            r.merge.as_ref().unwrap().is_merged(),
            "{}: {:?}",
            r.name,
            r.attempts
        );
    }

    // -- every interaction is visible ----------------------------------------
    let entries = AuditLog::read_entries(audit.path()).unwrap();
    let mut events = Vec::new();
    let mut after = -1i64;
    loop {
        let page = c.get(&format!("/api/events?after={after}"));
        let ev = page["events"].as_array().unwrap().clone();
        if ev.is_empty() {
            break;
        }
        after = ev.last().unwrap()["seq"].as_i64().unwrap();
        events.extend(ev);
    }
    assert_eq!(
        events
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        "every audit entry appears in the dashboard's feed"
    );
    for e in &events {
        assert!(!e["text"].as_str().unwrap().is_empty(), "{e}");
        // Every entry an agent's session caused is attributed to it.
        if e["session_id"].is_string() {
            assert!(e["agent"].is_string(), "unattributed: {e}");
        }
        // So is every Hub call an agent made.
        if e["data"]["caller"]["kind"] == "agent" {
            assert!(e["agent"].is_string(), "unattributed hub call: {e}");
        }
    }
    let kinds_seen: std::collections::BTreeSet<&str> =
        events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    for k in [
        "team_started",
        "session_started",
        "turn_started",
        "usage",
        "mutation",
        "hub_request_claim",
        "hub_denied",
        "hub_send_message",
        "hub_propose_decision",
        "approval_pending",
        "approval_resolved",
        "prompt_response",
        "merge_rejected",
        "repair_started",
        "merge_verified",
        "merge_applied",
        "review_verdicts",
        "session_finished",
    ] {
        assert!(kinds_seen.contains(k), "no {k} event");
    }
    // Answers given in the dashboard are recorded as such.
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "approval_resolved" && e["data"]["via"] == "ui")
    );
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "prompt_response" && e["data"]["responder"] == "interactive")
    );

    // Hub state: messages (held one included), claims, decisions, merges.
    let msgs = c.get("/api/messages");
    let msgs = msgs.as_array().unwrap();
    assert_eq!(msgs.len(), hub.view_messages(0, 1000).unwrap().len());
    let held = msgs
        .iter()
        .find(|m| m["state"] == "held")
        .expect("beta's message is held");
    assert_eq!(held["from"], "beta");
    assert_eq!(held["to"], "alpha");
    assert!(
        msgs.iter().any(|m| m["from"] == "alpha"
            && m["content"]["text"].as_str().unwrap().contains("edge-case"))
    );
    // Merge feedback reaches the agent as a review message, visible too.
    assert!(
        msgs.iter()
            .any(|m| m["message_type"] == "review" && m["content"]["status"] == "rejected")
    );
    let claims = c.get("/api/claims");
    assert_eq!(
        claims["claims"].as_array().unwrap().len(),
        hub.view_claims().unwrap().len()
    );
    let conflict = &claims["conflicts"].as_array().unwrap()[0];
    assert_eq!(
        (conflict["agent"].as_str(), conflict["path"].as_str()),
        (Some("beta"), Some("calc.py"))
    );
    assert!(
        claims["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "calc.py")
    );
    let decisions = c.get("/api/decisions");
    assert_eq!(decisions[0]["agent"], "alpha");
    assert_eq!(decisions[0]["status"], "proposed");

    // -- failures are never hidden -------------------------------------------
    let merges = c.get("/api/merges");
    let m1 = &merges["merges"][0];
    assert_eq!(
        (m1["status"].as_str(), m1["agent"].as_str()),
        (Some("rejected"), Some("alpha"))
    );
    assert!(
        m1["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "test_failed"),
        "{m1}"
    );
    let detail = c.get("/api/merge?seq=1");
    assert!(
        detail["artifacts"].as_array().unwrap().len() >= 3,
        "{detail}"
    );
    assert!(
        detail["tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == "tests.test_clamp.C.test_high" && t["outcome"] == "failed"),
        "{detail}"
    );
    assert!(
        detail["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|ch| ch["path"] == "calc.py")
    );
    let log = detail["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["path"].as_str().unwrap().ends_with("/test.log"))
        .unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let ev = c.get(&format!("/api/evidence?path={}", log.replace(':', "%3A")));
    assert!(
        ev["text"]
            .as_str()
            .unwrap()
            .contains("AssertionError: 0 != 10")
    );
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "merge_rejected" && e["level"] == "fail")
    );
    let m2 = &merges["merges"][1];
    assert_eq!(m2["status"], "merged");
    assert!(m2["evidence"].is_string());

    // -- the roster ------------------------------------------------------------
    let o = c.get("/api/overview");
    let roster = o["roster"].as_array().unwrap();
    let row = |n: &str| roster.iter().find(|r| r["name"] == n).unwrap();
    assert_eq!(row("alpha")["status"], "done");
    assert_eq!(row("alpha")["family"], "demo-family-a");
    assert_eq!(row("beta")["family"], "demo-family-b");
    assert_eq!(row("alpha")["sessions"], 2);
    assert_eq!(row("reviewer")["status"], "done");
    assert_eq!(row("reviewer")["family"], "demo-family-c");
    assert_eq!(row("reviewer")["model_id"], "demo-reviewer-c");
    // The executor's claims are attributed to it.
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "claims_submitted" && e["agent"] == "alpha")
    );
    assert_eq!(o["chain"]["error"], Value::Null);
    assert!(o["pending"].as_array().unwrap().is_empty());

    // -- the cost meter is accurate --------------------------------------------
    let cost = c.get("/api/cost");
    let reviews = events
        .iter()
        .filter(|e| e["kind"] == "session_started" && e["data"]["review"] == true)
        .count() as u64;
    assert_eq!(reviews, 2, "merges 2 and 3 reached review");
    let (a1, a2) = (
        script_tokens(&example().join("alpha.json")),
        script_tokens(&example().join("alpha-fix.json")),
    );
    let alpha = (a1.0 + a2.0, a1.1 + a2.1);
    let beta = script_tokens(&example().join("beta.json"));
    // A review session ends with its verdicts: it uses the script's first
    // step only.
    let rv: Value =
        serde_json::from_str(&std::fs::read_to_string(example().join("reviewer.json")).unwrap())
            .unwrap();
    let r1 = (
        rv["steps"][0]["prompt_tokens"].as_u64().unwrap(),
        rv["steps"][0]["completion_tokens"].as_u64().unwrap(),
    );
    let reviewer = (r1.0 * reviews, r1.1 * reviews);
    let want = [
        (
            "alpha",
            alpha,
            usd(alpha, price("roles-alpha.toml", "executor")),
        ),
        (
            "beta",
            beta,
            usd(beta, price("roles-beta.toml", "executor")),
        ),
        (
            "reviewer",
            reviewer,
            usd(reviewer, price("roles-reviewer.toml", "reviewer")),
        ),
    ];
    let mut total = 0.0;
    for (name, tokens, spend) in want {
        let t = &cost["cost"]["per_agent"][name];
        assert_eq!(
            (
                t["tokens_in"].as_u64().unwrap(),
                t["tokens_out"].as_u64().unwrap()
            ),
            tokens,
            "{name}"
        );
        assert!(
            close(t["spend_usd"].as_f64().unwrap(), spend),
            "{name}: {t} vs {spend}"
        );
        assert!(
            close(
                t["computed_usd"].as_f64().unwrap(),
                t["reported_usd"].as_f64().unwrap()
            ),
            "{name}"
        );
        assert_eq!(t["unpriced_calls"], 0, "{name}");
        // And it matches what the roster shows.
        assert!(close(
            row(name)["cost"]["spend_usd"].as_f64().unwrap(),
            spend
        ));
        total += spend;
    }
    assert!(close(
        cost["cost"]["total"]["spend_usd"].as_f64().unwrap(),
        total
    ));
    assert!(close(o["cost_total"]["spend_usd"].as_f64().unwrap(), total));
    assert_eq!(
        cost["cost"]["discrepancies"],
        json!([]),
        "reported, recomputed and session totals agree"
    );
    // Per session: equal to each session's own summary.
    for e in events.iter().filter(|e| e["kind"] == "session_finished") {
        let s = &cost["cost"]["per_session"][e["session_id"].as_str().unwrap()];
        assert_eq!(s["tokens_in"], e["data"]["tokens_in"]);
        assert!(close(
            s["reported_usd"].as_f64().unwrap(),
            e["data"]["cost_usd"].as_f64().unwrap()
        ));
    }

    // -- intervening at a gate: the held message, the decision ------------------
    let (st, m) = c.post(
        "/api/messages/decide",
        &json!({ "id": held["id"], "approve": true }),
    );
    assert_eq!(st, 200);
    assert_eq!(m["state"], "delivered");
    let (st, d) = c.post(
        "/api/decisions/decide",
        &json!({ "id": decisions[0]["id"], "accept": true, "reason": "agreed" }),
    );
    assert_eq!(st, 200, "{d}");
    assert_eq!(d["status"], "accepted");

    // -- one-click rollback (after the run) --------------------------------------
    assert!(
        std::fs::read_to_string(root.join("calc.py"))
            .unwrap()
            .contains("return hi")
    );
    dash.attach_workspace(opened.mediator);
    let snaps = c.get("/api/snapshots");
    let rows = snaps["snapshots"].as_array().unwrap();
    // Each merge snapshot names the agent whose work it holds, and what
    // changed. (Agents' own turn snapshots live while their isolated
    // workspace does; every snapshot stays in the log, attributed.)
    assert!(
        rows.iter().any(|r| r["kind"] == "merge"
            && r["agent"] == "alpha"
            && r["changes"]
                .as_array()
                .is_some_and(|c| c.iter().any(|x| x["path"] == "calc.py"))),
        "{snaps}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "snapshot" && e["agent"] == "alpha")
    );
    let pre2 = rows
        .iter()
        .find(|r| r["kind"] == "merge-pre" && r["merge_seq"] == 2)
        .unwrap()["ref"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, prep) = c.post("/api/rollback/prepare", &json!({ "ref": pre2 }));
    assert_eq!(st, 200, "{prep}");
    assert!(!prep["changes"].as_array().unwrap().is_empty());
    let (st, done) = c.post("/api/rollback/confirm", &json!({ "nonce": prep["nonce"] }));
    assert_eq!(st, 200, "{done}");
    assert_eq!(
        std::fs::read_to_string(root.join("calc.py")).unwrap(),
        "def clamp(x, lo, hi):\n    return x\n"
    );
    // A confirmation works once.
    assert_eq!(
        c.post("/api/rollback/confirm", &json!({ "nonce": prep["nonce"] }))
            .0,
        409
    );
    // Every action taken in the dashboard is in the audit log.
    let actions: Vec<String> = AuditLog::read_entries(audit.path())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "ui_action")
        .map(|e| e.data["action"].as_str().unwrap().to_string())
        .collect();
    for a in ["answer_approval", "decide_message", "decide", "rollback"] {
        assert!(
            actions.iter().any(|x| x == a),
            "{a} not audited: {actions:?}"
        );
    }
    assert!(
        AuditLog::read_entries(audit.path())
            .unwrap()
            .iter()
            .any(|e| e.kind == "rollback")
    );
    AuditLog::verify(audit.path()).unwrap();
}
