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
            assert!(Instant::now() < deadline, "the task did not finish");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_pending(&self) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let p = self.pending();
            if !p.is_empty() {
                return p;
            }
            assert!(Instant::now() < deadline, "no approval was asked");
            std::thread::sleep(Duration::from_millis(50));
        }
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
    let dir = tempfile::tempdir().unwrap();
    let base = anchor_fs::normalize_verbatim(&std::fs::canonicalize(dir.path()).unwrap());
    let ws = base.join("project");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::copy(example().join("workspace/hello.py"), ws.join("hello.py")).unwrap();
    let app: Arc<App> = App::new(AppConfig {
        state_base: base.join("state"),
        roles_path: example().join("roles.mock.toml"),
        mock_script: Some(std::fs::read_to_string(example().join("script.json")).unwrap()),
        exec_read_only: anchor_backend::default_exec_read_only(),
        agent_bin: agent_bin(),
        policy: None,
    });
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
