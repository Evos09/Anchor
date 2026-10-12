//! The desktop app's backend (`anchor app`, ADR-0028), driven through its
//! HTTP API exactly as the page drives it: open a project, run a task,
//! answer its approvals, undo a turn, stop a task.
//!
//! The model is scripted (`examples/app/`): it reads `hello.py`, then asks
//! to edit it and to write `README.md`, two approvals.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anchor_backend::session;
use anchor_backend::ui::app::{App, AppConfig};
use anchor_backend::ui::{self, Ui};
use serde_json::{Value, json};

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/app")
}

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

/// One raw HTTP/1.1 exchange: `(status, body)`.
fn http(port: u16, raw: &str) -> (u16, String) {
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
    (status, body.to_string())
}

/// Percent-encode a query value (paths carry `:`, `\` and spaces).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

struct Client {
    port: u16,
    token: String,
}

impl Client {
    fn get_raw(&self, path: &str) -> (u16, Value) {
        let (st, body) = http(
            self.port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
                self.port, self.token
            ),
        );
        (st, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    fn get(&self, path: &str) -> Value {
        let (st, v) = self.get_raw(path);
        assert_eq!(st, 200, "GET {path}: {v}");
        v
    }

    fn post(&self, path: &str, v: &Value) -> (u16, Value) {
        let body = v.to_string();
        let (st, resp) = http(
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

    fn ok(&self, path: &str, v: &Value) -> Value {
        let (st, r) = self.post(path, v);
        assert_eq!(st, 200, "POST {path} {v}: {r}");
        r
    }

    fn running(&self) -> bool {
        self.get("/api/app/state")["running"].as_bool().unwrap()
    }

    fn pending(&self) -> Vec<Value> {
        self.get("/api/overview")["pending"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Answer every approval with `choice` until the task ends; returns
    /// how many were answered.
    fn answer_all(&self, choice: &str) -> usize {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut n = 0;
        loop {
            for p in self.pending() {
                let (st, _) = self.post(
                    "/api/approvals/answer",
                    &json!({ "id": p["id"], "choice": choice }),
                );
                if st == 200 {
                    n += 1;
                }
            }
            if !self.running() {
                return n;
            }
            assert!(
                Instant::now() < deadline,
                "the task did not finish; the log so far:\n{}",
                self.story()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the running task to ask for an approval. Fails at once,
    /// with the log, if the task ends without asking.
    fn wait_pending(&self) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let p = self.pending();
            if !p.is_empty() {
                return p;
            }
            if !self.running() && self.pending().is_empty() {
                panic!(
                    "the task ended without asking for an approval; the log:\n{}",
                    self.story()
                );
            }
            assert!(
                Instant::now() < deadline,
                "no approval was asked (running: {}); the log so far:\n{}",
                self.running(),
                self.story()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The last events, one line each, for a failure message.
    fn story(&self) -> String {
        let ev = self.events();
        ev[ev.len().saturating_sub(40)..]
            .iter()
            .map(|e| format!("  [{}] {} {}", e["level"], e["kind"], e["text"]))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn events(&self) -> Vec<Value> {
        self.get("/api/events?after=-1")["events"]
            .as_array()
            .cloned()
            .unwrap()
    }

    fn finished(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["kind"] == "session_finished")
            .collect()
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    ws: PathBuf,
    _ui: Ui,
    c: Client,
}

fn start() -> Fx {
    start_with(true)
}

/// `explicit`: the mock models file was given with `--roles`; without
/// it, tasks are routed (Update #1) and the file is ignored.
fn start_with(explicit: bool) -> Fx {
    start_cfg(
        explicit,
        example().join("roles.mock.toml"),
        Some(std::fs::read_to_string(example().join("script.json")).unwrap()),
    )
}

fn start_cfg(explicit: bool, roles_path: PathBuf, mock_script: Option<String>) -> Fx {
    start_full(
        explicit,
        roles_path,
        mock_script,
        Vec::new(),
        Vec::new(),
        None,
    )
}

/// With scripted reviewers (one per review) and builders for rounds 2+.
fn start_full(
    explicit: bool,
    roles_path: PathBuf,
    mock_script: Option<String>,
    review: Vec<String>,
    repair: Vec<String>,
    team: Option<String>,
) -> Fx {
    start_custom(
        explicit,
        roles_path,
        mock_script,
        review,
        repair,
        team,
        |_| {},
    )
}

fn start_custom(
    explicit: bool,
    roles_path: PathBuf,
    mock_script: Option<String>,
    review: Vec<String>,
    repair: Vec<String>,
    team: Option<String>,
    tweak: impl FnOnce(&mut AppConfig),
) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = anchor_fs::normalize_verbatim(&std::fs::canonicalize(dir.path()).unwrap());
    let ws = base.join("project");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::copy(example().join("workspace/hello.py"), ws.join("hello.py")).unwrap();
    let mut cfg = AppConfig {
        state_base: base.join("state"),
        roles_path,
        mock_script,
        review_mock_scripts: review,
        repair_mock_scripts: repair,
        team_mock_script: team,
        exec_read_only: anchor_backend::default_exec_read_only(),
        agent_bin: agent_bin(),
        policy: None,
        roles_explicit: explicit,
        inference: anchor_backend::inference::Inference::new(
            Arc::new(anchor_backend::inference::keys::MemoryKeys::default()),
            None,
        )
        .unwrap(),
        app_version: None,
        update_url: anchor_backend::ui::app::UPDATE_URL.into(),
    };
    tweak(&mut cfg);
    let app: Arc<App> = App::new(cfg);
    let ui = ui::start_app(app, 0).unwrap();
    let c = Client {
        port: ui.port(),
        token: ui.token().to_string(),
    };
    Fx {
        _dir: dir,
        base,
        ws,
        _ui: ui,
        c,
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap().replace("\r\n", "\n")
}

/// The whole loop: pick the folder, run a task, approve, see the result,
/// undo the turn.
#[test]
fn a_task_runs_from_the_app_with_approvals_and_undo() {
    let fx = start();
    let c = &fx.c;
    assert_eq!(
        c.get("/api/overview"),
        json!({ "mode": "app", "project": false })
    );
    assert_eq!(c.get("/api/app/state")["project"], Value::Null);
    assert_eq!(c.get_raw("/api/claims").0, 409);

    // The folder picker: directory names under the base, nothing else.
    let dirs = c.get(&format!(
        "/api/app/dirs?path={}",
        enc(&fx.base.to_string_lossy())
    ));
    let names: Vec<&str> = dirs["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert!(names.contains(&"project"), "{dirs}");
    assert!(c.get("/api/app/dirs")["dirs"].as_array().unwrap().len() >= 2);

    let r = c.ok("/api/app/open", &json!({ "path": fx.ws }));
    assert_eq!(Path::new(r["project"].as_str().unwrap()), fx.ws);
    assert_eq!(c.get("/api/overview")["mode"], "app");

    c.ok(
        "/api/app/task",
        &json!({ "text": "Add a README and greet anchor" }),
    );
    // One task at a time; the project stays put meanwhile.
    assert_eq!(c.post("/api/app/task", &json!({ "text": "again" })).0, 409);
    assert_eq!(c.post("/api/app/open", &json!({ "path": fx.ws })).0, 409);
    let prompt = &c.wait_pending()[0];
    assert!(
        prompt["target"].as_str().unwrap().ends_with("hello.py"),
        "{prompt}"
    );
    assert_eq!(c.answer_all("approve"), 2);

    assert_eq!(read(&fx.ws.join("hello.py")), "print('hello, anchor')\n");
    assert!(read(&fx.ws.join("README.md")).starts_with("# hello"));
    let fin = c.finished();
    assert_eq!(fin.len(), 1);
    assert_eq!(
        fin[0]["data"]["result"],
        "Added a README and made the greeting mention anchor."
    );

    // Undo the turn that changed the files: restore the snapshot taken
    // before its first change.
    let snaps = c.get("/api/snapshots");
    let turn = snaps["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "turn")
        .unwrap_or_else(|| panic!("no turn snapshot: {snaps}"));
    let prep = c.ok("/api/rollback/prepare", &json!({ "ref": turn["ref"] }));
    assert!(!prep["changes"].as_array().unwrap().is_empty(), "{prep}");
    c.ok("/api/rollback/confirm", &json!({ "nonce": prep["nonce"] }));
    assert_eq!(read(&fx.ws.join("hello.py")), "print('hello')\n");
    assert!(!fx.ws.join("README.md").exists());

    // The next launch remembers the project.
    let recent = c.get("/api/app/state")["recent"].clone();
    assert_eq!(Path::new(recent[0].as_str().unwrap()), fx.ws);
    assert!(fx.base.join("state/app.json").is_file());
}

/// Stop while the agent waits on an approval: the approval is refused,
/// nothing is written, the task ends as cancelled, and the next task runs.
#[test]
fn stop_refuses_the_pending_approval_and_ends_the_task() {
    let fx = start();
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    c.ok("/api/app/task", &json!({ "text": "Add a README" }));
    c.wait_pending();
    c.ok("/api/app/cancel", &json!({}));
    let deadline = Instant::now() + Duration::from_secs(60);
    while c.running() {
        assert!(Instant::now() < deadline, "the task did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(read(&fx.ws.join("hello.py")), "print('hello')\n");
    assert!(!fx.ws.join("README.md").exists());
    let fin = c.finished();
    assert_eq!(fin.len(), 1);
    assert_eq!(fin[0]["data"]["failed"], session::CANCELLED, "{}", fin[0]);
    assert_eq!(c.post("/api/app/cancel", &json!({})).0, 409);

    // Stopping did not leave the approvals refusing: a new task asks again.
    c.ok("/api/app/task", &json!({ "text": "Add a README" }));
    assert_eq!(c.answer_all("approve"), 2);
    assert!(fx.ws.join("README.md").is_file());
}

/// The new endpoints are behind the same gates as the rest, and refuse
/// malformed requests.
#[test]
fn the_app_endpoints_are_gated() {
    let fx = start();
    let c = &fx.c;
    let port = c.port;
    let (st, _) = http(
        port,
        &format!("GET /api/app/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(st, 401);
    let (st, _) = http(
        port,
        &format!(
            "GET /api/app/dirs HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example\r\n\
             Authorization: Bearer {}\r\n\r\n",
            c.token
        ),
    );
    assert_eq!(st, 403);
    let (st, _) = http(
        port,
        &format!(
            "GET /api/app/state HTTP/1.1\r\nHost: attacker.example:{port}\r\nAuthorization: Bearer {}\r\n\r\n",
            c.token
        ),
    );
    assert_eq!(st, 403);

    assert_eq!(c.post("/api/app/task", &json!({ "text": "x" })).0, 409);
    assert_eq!(
        c.post("/api/app/open", &json!({ "path": "relative" })).0,
        400
    );
    let file = fx.ws.join("hello.py");
    assert_eq!(c.post("/api/app/open", &json!({ "path": file })).0, 400);
    assert_eq!(c.get_raw("/api/app/dirs?path=relative").0, 400);
    assert_eq!(
        c.post("/api/app/open", &json!({ "path": fx.ws, "extra": 1 }))
            .0,
        400
    );
    assert_eq!(c.post("/api/app/cancel", &json!({})).0, 409);

    // The models file is checked before it is used.
    let bad = fx.base.join("bad.toml");
    std::fs::write(
        &bad,
        "[providers.a]\nfamily = \"same\"\n[roles.executor]\nprovider = \"a\"\n\
         [roles.reviewer]\nprovider = \"a\"\n",
    )
    .unwrap();
    let (st, r) = c.post("/api/app/models", &json!({ "path": bad }));
    assert_eq!(st, 400, "{r}");
    let good = example().join("roles.mock.toml");
    c.ok("/api/app/models", &json!({ "path": good }));
    assert_eq!(c.get("/api/app/state")["models"]["ok"], true);

    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    assert_eq!(c.post("/api/app/task", &json!({ "text": "  " })).0, 400);
    assert_eq!(
        c.post("/api/app/task", &json!({ "text": "x".repeat(20_001) }))
            .0,
        400
    );
}

/// The desktop shell keeps `anchor app`'s stdin open; if the shell dies
/// without stopping it, the pipe closes and the backend must not linger.
#[test]
fn the_app_exits_when_the_shell_goes_away() {
    let dir = tempfile::tempdir().unwrap();
    let url = dir.path().join("url");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_anchor"))
        .args(["app", "--exit-with-stdin", "--ui-url-file"])
        .arg(&url)
        .env("ANCHOR_STATE_DIR", dir.path().join("state"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !url.exists() {
        assert!(Instant::now() < deadline, "the app did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Still serving while the pipe is open.
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none());
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "the app outlived its shell");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Update #1 §2.5: with nothing to run on, a task gets a prompt with the
/// ways forward, not an error, and no session starts.
#[test]
fn a_routed_task_with_no_model_gets_a_prompt() {
    let fx = start_with(false);
    let c = &fx.c;
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert_eq!(
        c.post("/api/app/inference", &json!({ "ollama_port": closed }))
            .0,
        200
    );
    let (s, _) = c.post("/api/app/open", &json!({ "path": fx.ws }));
    assert_eq!(s, 200);
    let inf = c.get("/api/app/inference");
    assert_eq!(inf["source"], "routed");
    assert_eq!(inf["local"]["running"], false);
    assert_eq!(inf["free"]["offered"], false);
    let (s, v) = c.post(
        "/api/app/task",
        &json!({ "text": "/local add a docstring" }),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["prompt"]["reason"]["kind"], "path_unavailable");
    assert_eq!(v["prompt"]["reason"]["path"], "C");
    let (s, v) = c.post("/api/app/task", &json!({ "text": "add a docstring" }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["prompt"]["reason"]["kind"], "nothing_available");
    assert_eq!(c.get("/api/app/state")["running"], false);
    // A pin is per project and saved.
    assert_eq!(c.post("/api/app/pin", &json!({ "path": "C" })).0, 200);
    assert_eq!(c.get("/api/app/state")["inference"]["pin"], "C");
    assert_eq!(c.post("/api/app/pin", &json!({ "path": "D" })).0, 400);
}

/// A model server on the loopback interface (Ollama's case). On Windows
/// such a runtime runs outside its AppContainer, with a cleared
/// environment: without SystemRoot it could not even open a socket
/// ("os error 10106", reported from a user's machine with Ollama).
#[test]
fn a_loopback_model_server_runs_a_task() {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = json!({
                "choices": [{ "index": 0, "finish_reason": "stop",
                              "message": { "role": "assistant", "content": "Nothing to change." } }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 3 },
            })
            .to_string();
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let roles = dir.path().join("roles.toml");
    let mut t = String::new();
    for (name, family) in [("local_a", "fam-a"), ("local_b", "fam-b")] {
        t.push_str(&format!(
            "[providers.{name}]\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nfamily = \"{family}\"\n\n"
        ));
    }
    for (role, provider) in [
        ("planner", "local_a"),
        ("executor", "local_a"),
        ("tester", "local_a"),
        ("reviewer", "local_b"),
    ] {
        t.push_str(&format!(
            "[roles.{role}]\nprovider = \"{provider}\"\nmodel_id = \"m-{provider}\"\ncontext_window = 32000\n\n"
        ));
    }
    std::fs::write(&roles, t).unwrap();
    let fx = start_cfg(true, roles, None);
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    c.ok(
        "/api/app/task",
        &json!({ "text": "Is anything to change?" }),
    );
    c.answer_all("deny");
    let fin = c.finished();
    assert_eq!(fin.len(), 1, "{}", c.story());
    assert!(fin[0]["data"]["failed"].is_null(), "{}", c.story());
    assert_eq!(
        fin[0]["data"]["result"],
        "Nothing to change.",
        "{}",
        c.story()
    );
    assert!(served.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

fn script(name: &str) -> String {
    std::fs::read_to_string(example().join(name)).unwrap()
}

/// Builder + checker with scripted models: the builder, a reviewer that
/// refutes, the builder again, a reviewer that approves.
fn start_pair(roles: &str) -> Fx {
    start_full(
        true,
        example().join(roles),
        Some(script("script.json")),
        vec![script("review-refute.json"), script("review-approve.json")],
        vec![script("repair.json")],
        None,
    )
}

fn of_kind(c: &Client, kind: &str) -> Vec<Value> {
    c.events()
        .into_iter()
        .filter(|e| e["kind"] == kind)
        .collect()
}

/// ADR-0039: the reviewer (another family) refutes the first change, its
/// note goes back to the builder, the builder fixes it, and the reviewer
/// approves. Every change still went through the user's approvals.
#[test]
fn the_reviewer_sends_the_builder_back_until_it_approves() {
    let fx = start_pair("roles.mock.toml");
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    let r = c.ok(
        "/api/app/task",
        &json!({ "text": "Add a README and greet anchor" }),
    );
    assert_eq!(r["collab"], "pair", "{r}");
    // Two writes in round 1, one in round 2.
    assert_eq!(c.answer_all("approve"), 3, "{}", c.story());

    let reviews = of_kind(c, "pair_review");
    assert_eq!(reviews.len(), 2, "{}", c.story());
    let (first, second) = (&reviews[0]["data"], &reviews[1]["data"]);
    assert_eq!(first["round"], 1);
    assert_eq!(first["approved"], false);
    assert_eq!(first["reviewer"]["family"], "demo-family-b");
    assert!(
        first["verdicts"][0]["note"]
            .as_str()
            .unwrap()
            .contains("Python 3")
    );
    assert_eq!(second["round"], 2);
    assert_eq!(second["approved"], true);
    assert_eq!(second["final"], true);
    assert_eq!(of_kind(c, "pair_reply").len(), 1);
    assert!(read(&fx.ws.join("README.md")).contains("needs Python 3"));
    assert_eq!(read(&fx.ws.join("hello.py")), "print('hello, anchor')\n");
    // The reviewer ran as its own agent, with no workspace tools.
    let reviewer_sessions = c
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "session_started" && e["agent"] == "reviewer")
        .count();
    assert_eq!(reviewer_sessions, 2, "{}", c.story());
}

/// The family rule (spec §8.2(a)): a models file whose reviewer shares
/// the builder's family is refused before anything runs (and the pair
/// loop's reviewer re-checks the families it actually ran on).
#[test]
fn the_reviewer_never_runs_on_the_builders_family() {
    let dir = tempfile::tempdir().unwrap();
    let roles = dir.path().join("same-family.toml");
    // Line endings as checked out (CRLF on Windows) do not matter here.
    let text = std::fs::read_to_string(example().join("roles.mock.toml"))
        .unwrap()
        .replace("\r\n", "\n")
        .replace(
            "[roles.reviewer]\nprovider = \"mock_b\"",
            "[roles.reviewer]\nprovider = \"mock_a\"",
        );
    assert!(text.contains("[roles.reviewer]\nprovider = \"mock_a\""));
    std::fs::write(&roles, text).unwrap();
    let fx = start_full(
        true,
        roles,
        Some(script("script.json")),
        vec![script("review-approve.json")],
        vec![],
        None,
    );
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    let (st, r) = c.post("/api/app/task", &json!({ "text": "Add a README" }));
    assert_eq!(st, 409, "{r}");
    assert!(r["error"].as_str().unwrap().contains("family"), "{r}");
    assert!(!c.running());
    assert!(!fx.ws.join("README.md").exists());
    assert!(
        !c.events()
            .iter()
            .any(|e| e["kind"] == "session_started" && e["agent"] == "reviewer")
    );
}

/// "One AI" in the settings (or `mode` on the task) runs no reviewer.
#[test]
fn one_ai_mode_skips_review() {
    let fx = start_pair("roles.mock.toml");
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    c.ok("/api/app/inference", &json!({ "collab": "single" }));
    assert_eq!(c.get("/api/app/state")["inference"]["collab"], "single");
    let r = c.ok("/api/app/task", &json!({ "text": "Add a README" }));
    assert_eq!(r["collab"], "single");
    assert_eq!(c.answer_all("approve"), 2);
    assert!(of_kind(c, "pair_review").is_empty());
    assert_eq!(c.finished().len(), 1);
    // The settings refuse out-of-range rounds and unknown modes.
    assert_eq!(
        c.post("/api/app/inference", &json!({ "pair_rounds": 0 })).0,
        400
    );
    assert_eq!(
        c.post("/api/app/inference", &json!({ "pair_rounds": 6 })).0,
        400
    );
    assert_eq!(
        c.post("/api/app/inference", &json!({ "collab": "trio" })).0,
        400
    );
    assert_eq!(
        c.post("/api/app/task", &json!({ "text": "x", "mode": "trio" }))
            .0,
        400
    );
}

/// Two builders (ADR-0039): both work at once in isolated copies, and
/// nothing reaches the project until the user confirms each merge.
#[test]
fn two_builders_run_in_the_app_and_merges_need_the_user() {
    // Builder A replays script.json, builder B builder-b.json.
    let fx = start_full(
        true,
        example().join("roles.mock.toml"),
        Some(script("script.json")),
        vec![],
        vec![],
        Some(script("builder-b.json")),
    );
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    let r = c.ok(
        "/api/app/task",
        &json!({ "text": "/team Add a README and notes" }),
    );
    assert_eq!(r["collab"], "team", "{r}");
    let mut merges = 0;
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        for p in c.pending() {
            if p["kind"] == "merge" {
                merges += 1;
                // Before the user confirms, the project is untouched.
                if merges == 1 {
                    assert_eq!(read(&fx.ws.join("hello.py")), "print('hello')\n");
                    assert!(!fx.ws.join("NOTES.md").exists());
                }
            }
            if c.post(
                "/api/approvals/answer",
                &json!({ "id": p["id"], "choice": "approve" }),
            )
            .0 == 200
            {
                answered += 1;
            }
        }
        if !c.running() {
            break;
        }
        assert!(Instant::now() < deadline, "{}", c.story());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(merges, 2, "{}", c.story());
    assert!(answered >= 5, "3 writes and 2 merges: {answered}");
    assert_eq!(read(&fx.ws.join("hello.py")), "print('hello, anchor')\n");
    assert!(fx.ws.join("README.md").is_file());
    assert!(fx.ws.join("NOTES.md").is_file());
    let done = of_kind(c, "team_finished");
    assert_eq!(done.len(), 1, "{}", c.story());
    let agents = done[0]["data"]["agents"].as_array().unwrap();
    assert!(agents.iter().all(|a| a["merged"] == true), "{}", done[0]);
    // A second two-builder task can start (Hub names are per task).
    c.ok(
        "/api/app/task",
        &json!({ "text": "Tidy up", "mode": "team" }),
    );
    c.answer_all("deny");
}

/// Stop ends a builder + checker task while the reviewer is pending
/// behind an approval, and nothing is written.
#[test]
fn stop_ends_a_pair_task() {
    let fx = start_pair("roles.mock.toml");
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    c.ok("/api/app/task", &json!({ "text": "Add a README" }));
    c.wait_pending();
    c.ok("/api/app/cancel", &json!({}));
    let deadline = Instant::now() + Duration::from_secs(60);
    while c.running() {
        assert!(Instant::now() < deadline, "the task did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!fx.ws.join("README.md").exists());
    assert!(of_kind(c, "pair_review").is_empty(), "{}", c.story());
}

/// A fake Ollama: `/api/version`, `/api/tags` (the models pulled so far)
/// and a streaming `/api/pull`. Reads each whole request before
/// answering (Windows resets a connection closed with unread bytes).
fn fake_ollama() -> u16 {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let pulled = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { continue };
            let pulled = pulled.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut first = String::new();
                let _ = r.read_line(&mut first);
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let reply = |s: &mut TcpStream, ctype: &str, out: &str| {
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    );
                };
                if first.starts_with("GET /api/version") {
                    reply(&mut s, "application/json", r#"{"version":"0.12.3"}"#);
                } else if first.starts_with("GET /api/tags") {
                    let models: Vec<Value> = pulled
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|m| json!({ "name": m, "size": 1, "details": {} }))
                        .collect();
                    reply(
                        &mut s,
                        "application/json",
                        &json!({ "models": models }).to_string(),
                    );
                } else if first.starts_with("POST /api/pull") {
                    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let model = v["model"].as_str().unwrap_or("").to_string();
                    let mut out = String::new();
                    for line in [
                        json!({ "status": "pulling manifest" }),
                        json!({ "status": "pulling layer", "total": 1000, "completed": 500 }),
                        json!({ "status": "pulling layer", "total": 1000, "completed": 1000 }),
                        json!({ "status": "success" }),
                    ] {
                        out.push_str(&line.to_string());
                        out.push('\n');
                    }
                    pulled.lock().unwrap().push(model);
                    reply(&mut s, "application/x-ndjson", &out);
                } else {
                    reply(&mut s, "application/json", "{}");
                }
                let _ = s.flush();
                let _ = s.shutdown(std::net::Shutdown::Write);
            });
        }
    });
    port
}

/// 0.12.0: the recommended local models download from the app, through
/// Ollama, and then show as ready. Only registry models can be pulled.
#[test]
fn models_download_from_the_app() {
    let fx = start_with(false);
    let c = &fx.c;
    let port = fake_ollama();
    c.ok("/api/app/inference", &json!({ "ollama_port": port }));
    let inf = c.get("/api/app/inference");
    assert_eq!(inf["local"]["running"], true, "{inf}");
    let rec: Vec<String> = inf["local"]["recommended"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(rec, ["qwen2.5-coder:7b", "llama3.1:8b"]);
    assert_eq!(
        c.post("/api/app/pull", &json!({ "model": "someone/else:latest" }))
            .0,
        400
    );
    for m in &rec {
        c.ok("/api/app/pull", &json!({ "model": m }));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pulls = c.get("/api/app/pulls")["pulls"].as_array().unwrap().clone();
        if pulls.len() == 2 && pulls.iter().all(|p| p["done"] == true) {
            for p in &pulls {
                assert!(p["error"].is_null(), "{p}");
                assert_eq!(p["status"], "success");
                assert_eq!(p["completed"], 1000);
            }
            break;
        }
        assert!(Instant::now() < deadline, "{pulls:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let ready: Vec<String> = c.get("/api/app/inference")["local"]["ready"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ready, rec);
    assert_eq!(
        c.post("/api/app/pull_cancel", &json!({ "model": "llama3.1:8b" }))
            .0,
        409
    );
    // No Ollama, no download.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    c.ok("/api/app/inference", &json!({ "ollama_port": closed }));
    let (st, r) = c.post("/api/app/pull", &json!({ "model": "qwen2.5-coder:7b" }));
    assert_eq!(st, 409, "{r}");
}

/// 0.12.0: the desktop app learns of a newer release from GitHub (here a
/// fake), asks at most every few hours, and not at all when turned off.
#[test]
fn the_app_offers_a_newer_release() {
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
            }
            assert!(first.starts_with("GET /repos/x/releases/latest"), "{first}");
            count.fetch_add(1, Ordering::SeqCst);
            let out = json!({
                "tag_name": "v9.1.0", "draft": false, "prerelease": false,
                "html_url": "https://github.com/Evos09/Anchor/releases/tag/v9.1.0",
                "body": "Faster everything.",
            })
            .to_string();
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
            let _ = s.flush();
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
    });
    let fx = start_custom(
        true,
        example().join("roles.mock.toml"),
        None,
        vec![],
        vec![],
        None,
        |cfg| {
            cfg.app_version = Some("0.12.0".into());
            cfg.update_url = format!("http://127.0.0.1:{port}/repos/x");
        },
    );
    let c = &fx.c;
    assert_eq!(c.get("/api/app/state")["version"], "0.12.0");
    let u = c.get("/api/app/update");
    assert_eq!(u["current"], "0.12.0");
    assert_eq!(u["latest"], "9.1.0", "{u}");
    assert_eq!(u["notes"], "Faster everything.");
    // Cached: GitHub is asked once.
    assert_eq!(c.get("/api/app/update")["latest"], "9.1.0");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // Turned off: no request at all.
    c.ok("/api/app/inference", &json!({ "check_updates": false }));
    assert_eq!(c.get("/api/app/update")["off"], true);
    assert_eq!(c.get("/api/app/state")["inference"]["check_updates"], false);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // Only the allowed pages open.
    assert_eq!(
        c.post(
            "/api/app/open_url",
            &json!({ "url": "https://example.com/" })
        )
        .0,
        403
    );
}

const PY: &str = if cfg!(windows) { "python" } else { "python3" };

fn python() -> bool {
    std::process::Command::new(PY)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A project test: a README, when there is one, must say Python 3 is
/// needed. It passes before the task (no README), fails after the
/// builder's first round, and passes after its second.
const README_TEST: &str = "import os\nimport unittest\n\n\nclass ReadmeTest(unittest.TestCase):\n    def test_readme_names_python_3(self):\n        if os.path.exists('README.md'):\n            with open('README.md') as f:\n                self.assertIn('Python 3', f.read())\n\n\nif __name__ == '__main__':\n    unittest.main()\n";

/// 0.12.0: the checker runs the project's tests. The reviewer accepts the
/// first change, but the tests fail, so the change goes back to the
/// builder with the test output; its fix passes and is approved.
#[test]
fn the_checker_sees_the_tests_fail_and_the_builder_fixes_them() {
    if !python() {
        eprintln!("skipped: no {PY}");
        return;
    }
    let fx = start_full(
        true,
        example().join("roles.mock.toml"),
        Some(script("script.json")),
        vec![script("review-approve.json")],
        vec![script("repair.json")],
        None,
    );
    let c = &fx.c;
    std::fs::write(fx.ws.join("test_readme.py"), README_TEST).unwrap();
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    let t = c.get("/api/app/tests");
    assert!(t["setup"].is_null(), "{t}");
    assert!(t["suggested"].is_array(), "a Python project: {t}");
    assert_eq!(
        c.post("/api/app/tests", &json!({ "argv": [] })).0,
        400,
        "an empty command"
    );
    c.ok(
        "/api/app/tests",
        &json!({ "argv": [PY, "-m", "unittest", "discover", "-v"], "timeout_secs": 180 }),
    );
    assert_eq!(c.get("/api/app/tests")["setup"]["argv"][0], PY);
    c.ok(
        "/api/app/task",
        &json!({ "text": "Add a README and greet anchor" }),
    );
    assert_eq!(c.answer_all("approve"), 3, "{}", c.story());

    let runs = of_kind(c, "pair_tests");
    assert_eq!(runs.len(), 3, "baseline and two rounds: {}", c.story());
    let d = |i: usize| &runs[i]["data"];
    assert_eq!(
        (d(0)["round"].clone(), d(0)["ok"].clone()),
        (json!(0), json!(true)),
        "{}",
        c.story()
    );
    assert_eq!(d(1)["round"], 1);
    assert_eq!(d(1)["ok"], false, "{}", d(1));
    assert_eq!(d(1)["holds"], false);
    assert_eq!(d(1)["failed"], 1, "{}", d(1));
    assert_eq!(d(2)["ok"], true, "{}", d(2));
    let reviews = of_kind(c, "pair_review");
    assert_eq!(reviews.len(), 2, "{}", c.story());
    assert_eq!(reviews[0]["data"]["reviewer_ok"], true);
    assert_eq!(reviews[0]["data"]["tests_ok"], false);
    assert_eq!(reviews[0]["data"]["approved"], false);
    assert_eq!(reviews[1]["data"]["approved"], true);
    assert!(read(&fx.ws.join("README.md")).contains("Python 3"));
    // The test command and its result are in the audit log.
    let log = serde_json::to_string(&c.events()).unwrap();
    assert!(log.contains("set_tests"), "{}", c.story());
}

/// Test runs use an export of the project, never the project itself: a
/// test command that writes a file leaves the user's folder unchanged.
#[test]
fn tests_never_run_in_the_live_project() {
    if !python() {
        eprintln!("skipped: no {PY}");
        return;
    }
    let fx = start();
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    c.ok("/api/app/inference", &json!({ "collab": "single" }));
    c.ok(
        "/api/app/tests",
        &json!({ "argv": [PY, "-c", "open('touched.txt', 'w').write('x'); print('ran')"], "timeout_secs": 180 }),
    );
    c.ok("/api/app/task", &json!({ "text": "Add a README" }));
    assert_eq!(c.answer_all("approve"), 2, "{}", c.story());
    let runs = of_kind(c, "pair_tests");
    assert_eq!(runs.len(), 1, "{}", c.story());
    assert_eq!(runs[0]["data"]["ok"], true, "{}", runs[0]);
    assert_eq!(runs[0]["data"]["info"], true);
    assert!(runs[0]["data"]["output"].as_str().unwrap().contains("ran"));
    assert!(!fx.ws.join("touched.txt").exists());
    assert!(fx.ws.join("README.md").exists());
    // Clearing the command, and declining the suggestion, are remembered.
    c.ok("/api/app/tests", &json!({ "argv": null, "decline": true }));
    let t = c.get("/api/app/tests");
    assert!(t["setup"].is_null());
    assert_eq!(t["declined"], true);
}

/// 0.12.0: a reply to a finished task continues the builder's own
/// conversation (its checkpoint, with the user's message as the next
/// turn); when the checkpoint is gone it starts afresh with a summary.
#[test]
fn a_follow_up_continues_the_same_task() {
    let fx = start_cfg(
        true,
        example().join("roles.mock.toml"),
        Some(
            r#"{"steps":[{"content":"Answered.","prompt_tokens":100,"completion_tokens":5}]}"#
                .into(),
        ),
    );
    let c = &fx.c;
    c.ok("/api/app/open", &json!({ "path": fx.ws }));
    let (st, r) = c.post(
        "/api/app/task",
        &json!({ "text": "Too early", "follow_up": true }),
    );
    assert_eq!(st, 409, "nothing to reply to yet: {r}");
    c.ok(
        "/api/app/task",
        &json!({ "text": "What does hello.py print?" }),
    );
    c.answer_all("approve");
    let r = c.ok(
        "/api/app/task",
        &json!({ "text": "And in French?", "follow_up": true }),
    );
    assert_eq!(r["follow_up"], true, "{r}");
    c.answer_all("approve");

    let ev = c.events();
    let follow = ev
        .iter()
        .find(|e| e["kind"] == "ui_action" && e["data"]["action"] == "follow_up_task")
        .unwrap_or_else(|| panic!("{}", c.story()));
    assert_eq!(follow["data"]["args"]["resumed"], true, "{follow}");
    assert_eq!(follow["data"]["args"]["text"], "And in French?");
    let started: Vec<&Value> = ev
        .iter()
        .filter(|e| e["kind"] == "session_started")
        .collect();
    assert_eq!(started.len(), 2, "{}", c.story());
    assert!(started[0]["data"]["resumed_from"].is_null());
    assert!(
        started[1]["data"]["resumed_from"].is_string(),
        "{}",
        started[1]
    );
    assert_eq!(started[1]["data"]["follow_up"], true);
    // The follow-up's conversation holds the first task and the reply.
    let second = started[1]["data"]["session_id"].as_str().unwrap();
    let saved: Vec<&Value> = ev
        .iter()
        .filter(|e| e["kind"] == "checkpoint_saved" && e["session_id"] == second)
        .collect();
    let path = saved.last().unwrap_or_else(|| panic!("{}", c.story()))["data"]["path"]
        .as_str()
        .unwrap()
        .to_string();
    let cp = std::fs::read_to_string(&path).unwrap();
    assert!(cp.contains("What does hello.py print?"), "{cp}");
    assert!(cp.contains("And in French?"), "{cp}");
    assert_eq!(c.finished().len(), 2);

    // The last task is remembered without secrets, for after a restart.
    let saved_app: Value =
        serde_json::from_slice(&std::fs::read(fx.base.join("state/app.json")).unwrap()).unwrap();
    let last = saved_app["last"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert!(
        last["text"].as_str().unwrap().contains("And in French?"),
        "{last}"
    );
    assert_eq!(last["result"], "Answered.");

    // Checkpoints gone (pruned): the reply starts afresh with a summary.
    for e in ev.iter().filter(|e| e["kind"] == "checkpoint_saved") {
        let _ = std::fs::remove_file(e["data"]["path"].as_str().unwrap());
    }
    c.ok(
        "/api/app/task",
        &json!({ "text": "One more thing", "follow_up": true }),
    );
    c.answer_all("approve");
    let ev = c.events();
    let follows: Vec<&Value> = ev
        .iter()
        .filter(|e| e["kind"] == "ui_action" && e["data"]["action"] == "follow_up_task")
        .collect();
    assert_eq!(follows.len(), 2);
    assert_eq!(
        follows[1]["data"]["args"]["resumed"], false,
        "{}",
        follows[1]
    );
    assert_eq!(c.finished().len(), 3, "{}", c.story());
}
