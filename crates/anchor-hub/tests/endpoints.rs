//! MCP and A2A endpoints over a real loopback socket: protocol conformance,
//! hostile HTTP input, and Threat 8 (a rogue peer cannot escalate through
//! the Hub).

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use anchor_hub::http::{ServerConfig, ServerHandle, serve};
use anchor_hub::*;
use serde_json::{Value, json};

struct Fx {
    _dir: tempfile::TempDir,
    hub: Arc<Hub>,
    server: ServerHandle,
    alpha: (i64, String),
    beta: (i64, String),
    ws: std::path::PathBuf,
    audit: std::path::PathBuf,
}

fn fx_with(cfg: ServerConfig) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(anchor_policy::AuditLog::open(&audit_path).unwrap());
    let hub = Arc::new(
        Hub::open(
            &dir.path().join("hub.sqlite"),
            &ws,
            audit,
            Arc::new(SystemClock),
            HubConfig::default(),
        )
        .unwrap(),
    );
    let reg = |name: &str, within: &[&str]| {
        let (a, t) = hub
            .register_agent(
                Caller::User,
                NewAgent {
                    display_name: name.into(),
                    model_id: format!("{name}-model"),
                    role: "executor".into(),
                    family: format!("{name}-family"),
                    capabilities: json!({}),
                    permissions: Permissions::collaborator(within),
                },
            )
            .unwrap();
        (a.id, t)
    };
    let alpha = reg("alpha", &["src/"]);
    let beta = reg("beta", &["."]);
    let server = serve(hub.clone(), 0, cfg).unwrap();
    Fx {
        _dir: dir,
        hub,
        server,
        alpha,
        beta,
        ws,
        audit: audit_path,
    }
}

fn fx() -> Fx {
    fx_with(ServerConfig::default())
}

/// Send raw bytes, return (status, body).
fn raw(port: u16, bytes: &[u8]) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(bytes).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    let (status, b) = raw(port, req.as_bytes());
    (status, serde_json::from_str(&b).unwrap_or(Value::Null))
}

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "1" },
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

fn mcp(port: u16, token: &str, method: &str, mut params: Value) -> (u16, Value) {
    params["_meta"] = meta();
    let name = params["name"].as_str().map(str::to_string);
    let auth = format!("Bearer {token}");
    let mut h = vec![
        ("Authorization", auth.as_str()),
        ("Mcp-Method", method),
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Accept", "application/json, text/event-stream"),
    ];
    if let Some(n) = &name {
        h.push(("Mcp-Name", n.as_str()));
    }
    post(
        port,
        "/mcp",
        &h,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }),
    )
}

fn call(port: u16, token: &str, tool: &str, args: Value) -> Value {
    let (status, r) = mcp(
        port,
        token,
        "tools/call",
        json!({ "name": tool, "arguments": args }),
    );
    assert_eq!(status, 200, "{r}");
    r["result"].clone()
}

fn a2a(port: u16, token: &str, method: &str, params: Value) -> Value {
    let auth = format!("Bearer {token}");
    let (status, r) = post(
        port,
        "/a2a",
        &[("Authorization", &auth), ("A2A-Version", "1.0")],
        &json!({ "jsonrpc": "2.0", "id": "r1", "method": method, "params": params }),
    );
    assert_eq!(status, 200);
    r
}

fn hub_rows(f: &Fx) -> usize {
    f.hub.verify().unwrap().rows
}

#[test]
fn mcp_discover_list_and_call_conform_to_2026_07_28() {
    let f = fx();
    let port = f.server.port();
    let tok = &f.alpha.1;

    // The official example request from schema/2026-07-28/examples
    // (DiscoverRequest/server-discover-request.json), sent verbatim.
    let official = json!({
        "jsonrpc": "2.0",
        "id": "discover-1",
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "ExampleClient", "version": "1.0.0" },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let auth = format!("Bearer {tok}");
    let (status, r) = post(
        port,
        "/mcp",
        &[
            ("Authorization", &auth),
            ("Mcp-Method", "server/discover"),
            ("MCP-Protocol-Version", "2026-07-28"),
        ],
        &official,
    );
    assert_eq!(status, 200);
    assert_eq!(r["id"], "discover-1");
    let res = &r["result"];
    // DiscoverResult: CacheableResult + supportedVersions + capabilities.
    assert_eq!(res["resultType"], "complete");
    assert_eq!(res["supportedVersions"], json!(["2026-07-28"]));
    assert!(res["capabilities"]["tools"].is_object());
    assert!(res["ttlMs"].is_u64());
    assert!(matches!(
        res["cacheScope"].as_str(),
        Some("public" | "private")
    ));
    assert_eq!(
        res["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "anchor-hub"
    );

    let (status, r) = mcp(port, tok, "tools/list", json!({}));
    assert_eq!(status, 200);
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"hub_request_claim") && names.contains(&"hub_send_message"));
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object");
        assert!(t["description"].is_string());
    }
    // Deterministic order (spec: SHOULD, for caching).
    let (_, r2) = mcp(port, tok, "tools/list", json!({}));
    assert_eq!(r["result"]["tools"], r2["result"]["tools"]);

    let res = call(
        port,
        tok,
        "hub_request_claim",
        json!({ "path": "src/lib.rs", "claim_type": "edit" }),
    );
    assert_eq!(res["resultType"], "complete");
    assert_eq!(res["isError"], false);
    assert_eq!(res["content"][0]["type"], "text");
    assert_eq!(res["structuredContent"]["path"], "src/lib.rs");
    assert_eq!(res["structuredContent"]["agent_id"], f.alpha.0);

    // A tool failure is a result with isError (the model sees it).
    let res = call(
        port,
        &f.beta.1,
        "hub_request_claim",
        json!({ "path": "src/", "claim_type": "edit" }),
    );
    assert_eq!(res["isError"], true);
    assert_eq!(res["structuredContent"]["error"], "conflict");
}

#[test]
fn mcp_protocol_errors() {
    let f = fx();
    let port = f.server.port();
    let auth = format!("Bearer {}", f.alpha.1);
    let body = |method: &str, version: &str| {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {} } } })
    };
    // Unsupported version: 400 + -32022 with supported/requested.
    let (s, r) = post(
        port,
        "/mcp",
        &[
            ("Authorization", &auth),
            ("Mcp-Method", "tools/list"),
            ("MCP-Protocol-Version", "2025-11-25"),
        ],
        &body("tools/list", "2025-11-25"),
    );
    assert_eq!((s, r["error"]["code"].as_i64()), (400, Some(-32022)));
    assert_eq!(r["error"]["data"]["supported"], json!(["2026-07-28"]));
    assert_eq!(r["error"]["data"]["requested"], "2025-11-25");
    // Header/body mismatch: -32020.
    for (method_h, version_h) in [("tools/call", "2026-07-28"), ("tools/list", "2025-11-25")] {
        let (s, r) = post(
            port,
            "/mcp",
            &[
                ("Authorization", &auth),
                ("Mcp-Method", method_h),
                ("MCP-Protocol-Version", version_h),
            ],
            &body("tools/list", "2026-07-28"),
        );
        assert_eq!(
            (s, r["error"]["code"].as_i64()),
            (400, Some(-32020)),
            "{method_h} {version_h}"
        );
    }
    // Mcp-Name must match params.name.
    let mut b = body("tools/call", "2026-07-28");
    b["params"]["name"] = json!("hub_list_tasks");
    let (s, r) = post(
        port,
        "/mcp",
        &[
            ("Authorization", &auth),
            ("Mcp-Method", "tools/call"),
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Name", "hub_list_agents"),
        ],
        &b,
    );
    assert_eq!((s, r["error"]["code"].as_i64()), (400, Some(-32020)));
    // Unknown method: 404 + -32601. Removed methods are unknown too.
    for m in ["initialize", "ping", "resources/subscribe", "nope"] {
        let (s, r) = post(
            port,
            "/mcp",
            &[
                ("Authorization", &auth),
                ("Mcp-Method", m),
                ("MCP-Protocol-Version", "2026-07-28"),
            ],
            &body(m, "2026-07-28"),
        );
        assert_eq!((s, r["error"]["code"].as_i64()), (404, Some(-32601)), "{m}");
    }
    // Unknown tool is a protocol error.
    let (s, r) = mcp(
        port,
        &f.alpha.1,
        "tools/call",
        json!({ "name": "rm_rf", "arguments": {} }),
    );
    assert_eq!((s, r["error"]["code"].as_i64()), (400, Some(-32602)));
}

#[test]
fn a2a_card_send_and_tasks() {
    let f = fx();
    let port = f.server.port();
    let (s, card) = raw(
        port,
        format!("GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n")
            .as_bytes(),
    );
    assert_eq!(s, 200);
    let card: Value = serde_json::from_str(&card).unwrap();
    for k in [
        "name",
        "description",
        "supportedInterfaces",
        "version",
        "capabilities",
        "defaultInputModes",
        "defaultOutputModes",
        "skills",
    ] {
        assert!(card.get(k).is_some(), "card lacks required {k}");
    }
    let iface = &card["supportedInterfaces"][0];
    assert_eq!(iface["protocolBinding"], "JSONRPC");
    assert_eq!(iface["protocolVersion"], "1.0");
    assert_eq!(iface["url"], format!("http://127.0.0.1:{port}/a2a"));
    assert_eq!(card["capabilities"]["streaming"], false);

    let r = a2a(
        port,
        &f.alpha.1,
        "SendMessage",
        json!({ "message": {
            "messageId": "m-1", "contextId": format!("agent:{}", f.beta.0),
            "role": "ROLE_USER", "parts": [{ "text": "please review src/lib.rs" }],
            "metadata": { "anchorMessageType": "review" }
        }}),
    );
    assert_eq!(r["result"]["message"]["role"], "ROLE_AGENT");
    assert_eq!(r["result"]["message"]["parts"][0]["text"], "delivered");
    let inbox = f
        .hub
        .query_messages(Caller::Agent(f.beta.0), MessageQuery::default())
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].from_agent_id, Some(f.alpha.0));
    assert_eq!(inbox[0].message_type, MessageType::Review);
    assert_eq!(
        inbox[0].content["a2a"]["parts"][0]["text"],
        "please review src/lib.rs"
    );

    let t = f
        .hub
        .create_task(Caller::User, "write tests", &[], Some(f.beta.0))
        .unwrap();
    let r = a2a(
        port,
        &f.beta.1,
        "GetTask",
        json!({ "id": t.id.to_string() }),
    );
    assert_eq!(r["result"]["status"]["state"], "TASK_STATE_SUBMITTED");
    let r = a2a(port, &f.beta.1, "ListTasks", json!({}));
    assert_eq!(r["result"]["totalSize"], 1);
    let r = a2a(port, &f.beta.1, "GetTask", json!({ "id": "999" }));
    assert_eq!(r["error"]["code"], -32001);
    for m in [
        "SendStreamingMessage",
        "CancelTask",
        "SubscribeToTask",
        "GetExtendedAgentCard",
    ] {
        assert_eq!(
            a2a(port, &f.beta.1, m, json!({}))["error"]["code"],
            -32004,
            "{m}"
        );
    }
    assert_eq!(
        a2a(port, &f.beta.1, "Bogus", json!({}))["error"]["code"],
        -32601
    );
    // Version negotiation: missing header means 0.3, which is refused.
    let auth = format!("Bearer {}", f.beta.1);
    let (_, r) = post(
        port,
        "/a2a",
        &[("Authorization", &auth)],
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {} }),
    );
    assert_eq!(r["error"]["code"], -32009);
}

/// Threat 8: a peer cannot escalate through the Hub. Every attempt is
/// refused, changes nothing, and is audited.
#[test]
fn rogue_agent_cannot_bypass_the_hub() {
    let f = fx();
    let port = f.server.port();
    let rogue = &f.alpha.1; // may only claim under src/
    let rows_before = hub_rows(&f);

    // No token, a bad token, a retired agent's token.
    for auth in [None, Some("Bearer anc_nope"), Some("Basic Zm9vOmJhcg==")] {
        let mut h = vec![
            ("Mcp-Method", "tools/list"),
            ("MCP-Protocol-Version", "2026-07-28"),
        ];
        if let Some(a) = auth {
            h.push(("Authorization", a));
        }
        let (s, _) = post(port, "/mcp", &h, &json!({}));
        assert_eq!(s, 401, "{auth:?}");
        let (s, _) = post(port, "/a2a", &h, &json!({}));
        assert_eq!(s, 401);
    }
    // DNS rebinding: a foreign Host or any Origin.
    let auth = format!("Bearer {rogue}");
    for (n, v) in [("Origin", "http://evil.example"), ("Origin", "null")] {
        let (s, _) = post(
            port,
            "/mcp",
            &[("Authorization", &auth), (n, v)],
            &json!({}),
        );
        assert_eq!(s, 403);
    }
    let (s, _) = raw(
        port,
        format!("POST /mcp HTTP/1.1\r\nHost: evil.example:{port}\r\nAuthorization: {auth}\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes(),
    );
    assert_eq!(s, 403);

    // Escalation attempts through tools.
    let attempts = [
        (
            "hub_request_claim",
            json!({ "path": "tests/x.rs", "claim_type": "edit" }),
        ),
        (
            "hub_request_claim",
            json!({ "path": ".", "claim_type": "edit" }),
        ),
        (
            "hub_request_claim",
            json!({ "path": "../outside", "claim_type": "edit" }),
        ),
        (
            "hub_request_claim",
            json!({ "path": "src/a.rs", "claim_type": "edit", "agent_id": f.beta.0 }),
        ),
        (
            "hub_send_message",
            json!({ "to_agent": f.beta.0, "message_type": "task", "content": "x", "from_agent_id": f.beta.0 }),
        ),
        (
            "hub_send_message",
            json!({ "to_agent": f.alpha.0, "message_type": "task", "content": "self" }),
        ),
        (
            "hub_assign_task",
            json!({ "task_id": 999, "agent_id": f.beta.0 }),
        ),
    ];
    for (tool, args) in attempts {
        let res = call(port, rogue, tool, args.clone());
        assert_eq!(res["isError"], true, "{tool} {args}");
    }
    // User-only operations are not tools at all.
    for tool in [
        "register_agent",
        "approve_message",
        "accept_decision",
        "hub_approve_message",
    ] {
        let (s, r) = mcp(
            port,
            rogue,
            "tools/call",
            json!({ "name": tool, "arguments": {} }),
        );
        assert_eq!(
            (s, r["error"]["code"].as_i64()),
            (400, Some(-32602)),
            "{tool}"
        );
    }
    // Another agent's claim cannot be released.
    let theirs = f
        .hub
        .request_claim(
            Caller::Agent(f.beta.0),
            "docs/x.md",
            ClaimType::Edit,
            false,
            None,
        )
        .unwrap();
    let res = call(
        port,
        rogue,
        "hub_release_claim",
        json!({ "claim_id": theirs.id }),
    );
    assert_eq!(res["structuredContent"]["error"], "forbidden");
    // Another agent's direct messages are invisible.
    f.hub
        .send_message(
            Caller::User,
            Recipient::Agent(f.beta.0),
            MessageType::Task,
            json!("secret"),
        )
        .unwrap();
    let res = call(port, rogue, "hub_query_messages", json!({}));
    assert_eq!(res["structuredContent"], json!([]));
    // A2A: a spoofed sender in metadata is ignored; identity is the token.
    let r = a2a(
        port,
        rogue,
        "SendMessage",
        json!({ "message": {
            "messageId": "m", "contextId": format!("agent:{}", f.beta.0), "role": "ROLE_USER",
            "parts": [{ "text": "hi" }], "metadata": { "from": f.beta.0, "fromAgentId": f.beta.0 }
        }, "metadata": { "agentId": f.beta.0 }}),
    );
    let mid = r["result"]["message"]["metadata"]["anchorMessageId"]
        .as_i64()
        .unwrap();
    let all = f
        .hub
        .query_messages(Caller::User, MessageQuery::default())
        .unwrap();
    assert_eq!(
        all.iter().find(|m| m.id == mid).unwrap().from_agent_id,
        Some(f.alpha.0)
    );

    // Nothing above changed state except the beta claim, the user's
    // message and the A2A message; every refusal is audited; the DB still
    // verifies against the log.
    assert_eq!(hub_rows(&f), rows_before + 3);
    let entries = anchor_policy::AuditLog::read_entries(&f.audit).unwrap();
    let denied = entries.iter().filter(|e| e.kind == "hub_denied").count();
    let unauth = entries
        .iter()
        .filter(|e| e.kind == "hub_unauthenticated")
        .count();
    assert!(denied >= 8, "{denied}");
    assert_eq!(unauth, 6);
    assert!(f.hub.verify().unwrap().ok());

    // A retired agent's token stops working at once.
    f.hub.retire_agent(Caller::User, f.alpha.0).unwrap();
    let (s, _) = mcp(port, rogue, "tools/list", json!({}));
    assert_eq!(s, 401);
}

#[test]
fn hostile_http_is_refused() {
    // Long enough that a loaded runner reads a request sent in one write
    // in time (CI run 37620917349 got 408 at 500 ms); the slowloris case
    // below still sees its 408 well within its 5 s.
    let f = fx_with(ServerConfig {
        request_deadline: Duration::from_secs(2),
        ..Default::default()
    });
    let port = f.server.port();
    let host = format!("Host: 127.0.0.1:{port}\r\n");
    let cases: Vec<(String, u16)> = vec![
        (
            format!("POST /mcp HTTP/1.1\r\n{host}Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"),
            411,
        ),
        (format!("POST /mcp HTTP/1.1\r\n{host}\r\n"), 411),
        (
            format!("POST /mcp HTTP/1.1\r\n{host}Content-Length: 99999999\r\n\r\n"),
            413,
        ),
        (
            format!("POST /mcp HTTP/1.1\r\n{host}Content-Length: 1\r\nContent-Length: 1\r\n\r\nx"),
            400,
        ),
        (
            format!("POST /mcp HTTP/1.1\r\n{host}Content-Length: x\r\n\r\n"),
            400,
        ),
        (format!("GET /mcp HTTP/1.1\r\n{host}\r\n"), 405),
        (format!("GET /nothing HTTP/1.1\r\n{host}\r\n"), 404),
        (format!("BREW /mcp HTTP/9\r\n{host}\r\n"), 400),
        (
            format!(
                "POST /mcp HTTP/1.1\r\n{host}X-{}: y\r\n\r\n",
                "a".repeat(20_000)
            ),
            431,
        ),
        (
            format!(
                "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil\r\n\r\n"
            ),
            403,
        ),
    ];
    for (req, want) in cases {
        let (got, _) = raw(port, req.as_bytes());
        assert_eq!(got, want, "{:?}", &req[..req.len().min(80)]);
    }
    // Malformed JSON with valid auth.
    let auth = format!("Bearer {}", f.beta.1);
    let req = format!(
        "POST /mcp HTTP/1.1\r\n{host}Authorization: {auth}\r\nContent-Length: 5\r\n\r\n{{nope"
    );
    let (s, b) = raw(port, req.as_bytes());
    assert_eq!(s, 400);
    assert!(b.contains("-32700"));
    // Slowloris: a request that never finishes is cut off at the deadline.
    let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    slow.write_all(format!("POST /mcp HTTP/1.1\r\n{host}").as_bytes())
        .unwrap();
    let t = std::time::Instant::now();
    let mut out = String::new();
    let _ = slow.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 408"), "{out}");
    assert!(t.elapsed() < Duration::from_secs(3));
    // The server still answers afterwards.
    let (s, _) = raw(
        port,
        format!("GET /.well-known/agent-card.json HTTP/1.1\r\n{host}\r\n").as_bytes(),
    );
    assert_eq!(s, 200);
    let _ = (&f.ws, &f.hub);
}

/// Review finding H-1: idle connections hold readers, not workers, so
/// more of them than workers and backlog together do not lock agents out.
#[test]
fn idle_connections_do_not_starve_requests() {
    let f = fx();
    let port = f.server.port();
    let idle: Vec<TcpStream> = (0..60)
        .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let t = std::time::Instant::now();
    let (s, _) = mcp(port, &f.beta.1, "tools/list", json!({}));
    assert_eq!(s, 200);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    drop(idle);
    let _ = (&f.ws, &f.hub);
}

#[test]
fn artifacts_are_hashed_by_the_hub() {
    let f = fx();
    let port = f.server.port();
    std::fs::create_dir(f.ws.join("src")).unwrap();
    std::fs::write(f.ws.join("src/out.txt"), "evidence").unwrap();
    let res = call(
        port,
        &f.beta.1,
        "hub_register_artifact",
        json!({ "kind": "log", "path": "src/out.txt", "evidence": { "note": "x" } }),
    );
    assert_eq!(
        res["structuredContent"]["sha256"],
        anchor_policy::audit::sha256_hex(b"evidence")
    );
    // A caller-supplied hash is not an accepted argument.
    let res = call(
        port,
        &f.beta.1,
        "hub_register_artifact",
        json!({ "kind": "log", "path": "src/out.txt", "sha256": "00" }),
    );
    assert_eq!(res["isError"], true);
    let res = call(
        port,
        &f.beta.1,
        "hub_register_artifact",
        json!({ "kind": "log", "path": "../x" }),
    );
    assert_eq!(res["isError"], true);
}
