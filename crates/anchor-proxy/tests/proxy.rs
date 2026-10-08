//! The free-tier proxy end to end (Update #1 §10, "PROXY" and "POOL
//! MANAGER"): requests go through [`Proxy`] to a fake provider over real
//! HTTP, so the streaming, counting and cut-off paths are the real ones.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anchor_hub::http::{Request, Response, Service};
use anchor_inference::Registry;
use anchor_proxy::config::ProxyConfig;
use anchor_proxy::server::{POOL_FULL, Proxy};
use anchor_proxy::store::Store;
use anchor_proxy::upstream::HttpUpstream;
use serde_json::{Value, json};

/// 2026-10-08T00:00:00Z.
const T0: u64 = 1_791_417_600_000;
const HOUR: u64 = 3_600_000;

/// What the fake provider answers: status, extra headers, body.
type Behaviour = Box<dyn Fn(&Value) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

struct FakeProvider {
    port: u16,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    behaviour: Arc<Mutex<Behaviour>>,
}

fn sse(content: &str, usage: Option<(u64, u64)>) -> String {
    let mut s = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"role":"assistant","content":content},"finish_reason":"stop"}]})
    );
    if let Some((p, c)) = usage {
        s += &format!(
            "data: {}\n\n",
            json!({"choices":[],"usage":{"prompt_tokens":p,"completion_tokens":c}})
        );
    }
    s + "data: [DONE]\n\n"
}

impl FakeProvider {
    fn start() -> FakeProvider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ok: Behaviour = Box::new(|_| (200, vec![], sse("Hello", Some((50, 10)))));
        let behaviour = Arc::new(Mutex::new(ok));
        let (s2, b2) = (seen.clone(), behaviour.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                s2.lock().unwrap().push((path, v.clone()));
                let (status, headers, out) = (b2.lock().unwrap())(&v);
                let mut resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
                    out.len()
                );
                for (k, v) in headers {
                    resp += &format!("{k}: {v}\r\n");
                }
                resp += "\r\n";
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(out.as_bytes());
            }
        });
        FakeProvider {
            port,
            seen,
            behaviour,
        }
    }

    fn set(&self, b: Behaviour) {
        *self.behaviour.lock().unwrap() = b;
    }

    fn requests(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

struct Fx {
    dir: tempfile::TempDir,
    proxy: Proxy,
    clock: Arc<AtomicU64>,
    alpha: FakeProvider,
    beta: FakeProvider,
}

fn registry(alpha: u16, beta: u16) -> String {
    let model = |id: &str, provider: &str, weight: f64| {
        format!(
            r#"
[[model]]
id = "{id}"
display_name = "Open Coder"
provider = "{provider}"
provider_model_id = "{provider}/open-coder"
family = "open"
same_as = "open-coder"
context_window = 32768
license_id = "Apache-2.0"
license_commercial_ok = true
license_revenue_gate = false
free_tier_weight = {weight}
free_tier_eligible = true
available_on_paths = ["A"]
verified_at = "2026-10-08"
verified_by = "test"
"#
        )
    };
    // Loopback http is allowed for local providers; the fake providers
    // stand in for https ones.
    format!(
        r#"
[[provider]]
id = "alpha"
display_name = "Alpha"
base_url = "http://127.0.0.1:{alpha}/v1"
paths = ["A"]

[[provider]]
id = "beta"
display_name = "Beta"
base_url = "http://127.0.0.1:{beta}/v1"
paths = ["A"]
{}{}"#,
        model("open-coder@alpha", "alpha", 1.0),
        model("open-coder@beta", "beta", 2.0)
    )
}

fn setup(daily_cap: u64) -> Fx {
    let (alpha, beta) = (FakeProvider::start(), FakeProvider::start());
    let dir = tempfile::tempdir().unwrap();
    for p in ["alpha", "beta"] {
        std::fs::write(
            dir.path().join(format!("{p}.md")),
            "# Provider\n\n- Verdict: INCLUDE\n- Terms verified: 2026-10-08\n",
        )
        .unwrap();
    }
    let reg = Registry::from_toml(&registry(alpha.port, beta.port)).unwrap();
    let cfg = ProxyConfig::from_toml(&format!(
        r#"
port = 9
public_host = "free.example"
registry = "models.toml"
database = "proxy.db"
[pool]
chain = ["alpha", "beta"]
daily_cap = {daily_cap}
[[account]]
provider = "alpha"
key_env = "KA"
adr = "alpha.md"
[[account]]
provider = "beta"
key_env = "KB"
adr = "beta.md"
"#
    ))
    .unwrap();
    let keys = BTreeMap::from([
        ("KA".to_string(), "ka-secret".to_string()),
        ("KB".to_string(), "kb-secret".to_string()),
    ]);
    let clock = Arc::new(AtomicU64::new(T0));
    let c2 = clock.clone();
    let store = Store::open(&dir.path().join("proxy.db")).unwrap();
    let proxy = Proxy::new(
        cfg,
        reg,
        dir.path(),
        store,
        &keys,
        Arc::new(HttpUpstream::new(Duration::from_secs(10))),
        Box::new(move || c2.load(Ordering::SeqCst)),
    )
    .unwrap();
    Fx {
        dir,
        proxy,
        clock,
        alpha,
        beta,
    }
}

fn call(
    p: &Proxy,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (Response, Value) {
    let mut headers = vec![("host".to_string(), "free.example".to_string())];
    if let Some(t) = token {
        headers.push(("authorization".to_string(), format!("Bearer {t}")));
    }
    let r = p.handle(&Request {
        method: method.into(),
        path: path.into(),
        headers,
        body: if body.is_null() {
            vec![]
        } else {
            body.to_string().into_bytes()
        },
    });
    let v = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    (r, v)
}

fn register(p: &Proxy, device: &str) -> String {
    let (r, v) = call(
        p,
        "POST",
        "/v1/anchor/register",
        None,
        json!({ "device": device }),
    );
    assert_eq!(r.status, 200, "{v}");
    v["token"].as_str().unwrap().to_string()
}

fn chat(p: &Proxy, token: &str, text: &str) -> (Response, Value) {
    call(
        p,
        "POST",
        "/v1/chat/completions",
        Some(token),
        json!({ "model": "open-coder@alpha", "messages": [{ "role": "user", "content": text }] }),
    )
}

fn usage(p: &Proxy, token: &str) -> Value {
    call(p, "GET", "/v1/anchor/usage", Some(token), Value::Null).1
}

const DEVICE: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn a_request_is_forwarded_and_settled_on_the_providers_count() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    let (r, v) = chat(&fx.proxy, &t, "explain this file");
    assert_eq!(r.status, 200, "{v}");
    assert_eq!(v["choices"][0]["message"]["content"], "Hello");
    assert_eq!(v["model"], "open-coder@alpha");
    let reqs = fx.alpha.requests();
    assert_eq!(reqs.len(), 1);
    let (path, sent) = &reqs[0];
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(sent["model"], "alpha/open-coder");
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert!(sent["max_tokens"].as_u64().unwrap() > 0);
    // Reconciled: the hold is gone, the provider's 50 + 10 are charged.
    assert_eq!(usage(&fx.proxy, &t)["caps"]["five_hour"]["used"], 60);
    // The models list names the free models served.
    let (_, m) = call(&fx.proxy, "GET", "/v1/models", Some(&t), Value::Null);
    assert_eq!(m["data"].as_array().unwrap().len(), 2);
}

#[test]
fn pre_flight_refuses_a_request_beyond_the_budget_without_forwarding() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    let huge = "x".repeat(240_000); // ≈ 60,000 tokens > 50,000 in the window
    let (r, v) = chat(&fx.proxy, &t, &huge);
    assert_eq!(r.status, 429);
    assert_eq!(v["error"]["type"], "anchor_cap");
    assert_eq!(v["error"]["cap"], "five_hour");
    assert!(fx.alpha.requests().is_empty() && fx.beta.requests().is_empty());
}

#[test]
fn the_stream_is_cut_when_output_passes_the_reservation() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    // A provider that ignores max_tokens and keeps writing.
    fx.alpha
        .set(Box::new(|_| (200, vec![], sse(&"y".repeat(200_000), None))));
    let (r, v) = call(
        &fx.proxy,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        json!({ "model": "open-coder@alpha", "max_tokens": 1000,
                "messages": [{ "role": "user", "content": "hi" }] }),
    );
    assert_eq!(r.status, 200, "{v}");
    assert!(
        r.headers
            .iter()
            .any(|(k, v)| *k == "X-Anchor-Truncated" && v == "1")
    );
    assert_eq!(v["choices"][0]["finish_reason"], "length");
}

#[test]
fn a_rate_limited_provider_cools_down_and_the_pool_falls_over() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    fx.alpha.set(Box::new(|_| {
        (
            429,
            vec![("Retry-After".into(), "30".into())],
            r#"{"error":"rate limit"}"#.into(),
        )
    }));
    let (r, v) = chat(&fx.proxy, &t, "hi");
    assert_eq!(r.status, 200, "{v}");
    assert_eq!(fx.alpha.requests().len(), 1);
    assert_eq!(fx.beta.requests().len(), 1);
    assert_eq!(fx.beta.requests()[0].1["model"], "beta/open-coder");
    // Beta's weight is 2: the same 60 tokens cost 120.
    assert_eq!(usage(&fx.proxy, &t)["caps"]["five_hour"]["used"], 120);
    // Alpha is skipped while it cools down...
    chat(&fx.proxy, &t, "again");
    assert_eq!(fx.alpha.requests().len(), 1);
    // ...and tried again once Retry-After has passed.
    fx.alpha
        .set(Box::new(|_| (200, vec![], sse("ok", Some((5, 5))))));
    fx.clock.fetch_add(31_000, Ordering::SeqCst);
    chat(&fx.proxy, &t, "later");
    assert_eq!(fx.alpha.requests().len(), 2);
}

#[test]
fn a_refused_key_is_disabled_and_exhaustion_offers_a_way_on() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    fx.alpha.set(Box::new(|_| {
        (401, vec![], r#"{"error":"invalid api key"}"#.into())
    }));
    fx.beta.set(Box::new(|_| (503, vec![], "{}".into())));
    let (r, v) = chat(&fx.proxy, &t, "hi");
    // Not an error page: the BYOK / Ollama choice.
    assert_eq!(r.status, 429, "{v}");
    assert_eq!(v["error"]["type"], "anchor_pool");
    assert_eq!(v["error"]["message"], POOL_FULL);
    assert!(POOL_FULL.contains("Add your own API key") && POOL_FULL.contains("Ollama"));
    // The failed attempt held nothing.
    assert_eq!(usage(&fx.proxy, &t)["caps"]["five_hour"]["used"], 0);
    // Alpha stays disabled a day later; beta is back after its backoff.
    fx.beta
        .set(Box::new(|_| (200, vec![], sse("ok", Some((1, 1))))));
    fx.clock.fetch_add(24 * HOUR, Ordering::SeqCst);
    let before = fx.alpha.requests().len();
    assert_eq!(chat(&fx.proxy, &t, "hi").0.status, 200);
    assert_eq!(fx.alpha.requests().len(), before);
}

#[test]
fn the_pool_cap_holds_whatever_the_users_own_caps_say() {
    // A small pool: A's request (charged 8,500) leaves too little for B's
    // reservation (about 16,400 at the dearer provider's weight of 2).
    let fx = setup(20_000);
    fx.alpha
        .set(Box::new(|_| (200, vec![], sse("ok", Some((8_000, 500))))));
    let a = register(&fx.proxy, DEVICE);
    let (r, v) = chat(&fx.proxy, &a, "hi");
    assert_eq!(r.status, 200, "{v}");
    let b = register(&fx.proxy, "fedcba9876543210fedcba9876543210");
    let (r, v) = chat(&fx.proxy, &b, "hi");
    assert_eq!(r.status, 429, "{v}");
    assert_eq!(v["error"]["type"], "anchor_pool");
    assert_eq!(v["error"]["reason"]["kind"], "pool_cap_reached");
    // B's own caps are untouched.
    assert_eq!(usage(&fx.proxy, &b)["caps"]["five_hour"]["used"], 0);
}

#[test]
fn request_content_is_never_written_to_disk() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    let marker = "MARKER-7f3c9e-never-on-disk";
    fx.alpha.set(Box::new(move |_| {
        (
            200,
            vec![],
            sse("reply-MARKER-reply-7f3c9e-never-on-disk", Some((5, 5))),
        )
    }));
    assert_eq!(
        chat(&fx.proxy, &t, &format!("secret code {marker} here"))
            .0
            .status,
        200
    );
    // A failing request too (its error path writes a log line).
    fx.alpha.set(Box::new(|_| (500, vec![], "{}".into())));
    fx.beta.set(Box::new(|_| (500, vec![], "{}".into())));
    let _ = chat(&fx.proxy, &t, &format!("again {marker}"));
    drop(fx.proxy);
    let mut scanned = 0;
    for e in std::fs::read_dir(fx.dir.path()).unwrap() {
        let p = e.unwrap().path();
        let bytes = std::fs::read(&p).unwrap();
        scanned += bytes.len();
        for needle in ["MARKER-7f3c9e", "secret code", "reply-MARKER"] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{needle} found in {}",
                p.display()
            );
        }
    }
    assert!(scanned > 0);
    // The metadata did get written.
    let store = Store::open(&fx.dir.path().join("proxy.db")).unwrap();
    assert!(store.log_count().unwrap() >= 2);
}

#[test]
fn registering_again_on_a_device_keeps_its_account_and_caps() {
    let fx = setup(10_000_000);
    let t1 = register(&fx.proxy, DEVICE);
    chat(&fx.proxy, &t1, "hi");
    let t2 = register(&fx.proxy, DEVICE);
    assert_ne!(t1, t2);
    // The old token stops working; the usage stayed.
    assert_eq!(
        call(&fx.proxy, "GET", "/v1/anchor/usage", Some(&t1), Value::Null)
            .0
            .status,
        401
    );
    assert_eq!(usage(&fx.proxy, &t2)["caps"]["five_hour"]["used"], 60);
}

#[test]
fn what_the_client_says_about_time_changes_nothing() {
    let fx = setup(10_000_000);
    let t = register(&fx.proxy, DEVICE);
    chat(&fx.proxy, &t, "hi");
    let plain = usage(&fx.proxy, &t);
    // A client in another time zone, or with its clock set ahead.
    let r = fx.proxy.handle(&Request {
        method: "GET".into(),
        path: "/v1/anchor/usage".into(),
        headers: vec![
            ("host".into(), "free.example".into()),
            ("authorization".into(), format!("Bearer {t}")),
            ("date".into(), "Mon, 01 Jan 2035 00:00:00 GMT".into()),
            ("x-timezone".into(), "Pacific/Kiritimati".into()),
        ],
        body: vec![],
    });
    let shifted: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(plain["caps"], shifted["caps"]);
    // Resets are UTC: the month turns at 2026-11-01T00:00Z.
    assert_eq!(
        plain["caps"]["monthly"]["resets_at_ms"],
        1_793_491_200_000u64
    );
}

#[test]
fn the_proxy_refuses_to_start_without_terms_adrs_or_with_two_accounts() {
    let fx = setup(10_000_000);
    let reg = Registry::from_toml(&registry(fx.alpha.port, fx.beta.port)).unwrap();
    let base = |accounts: &str| {
        ProxyConfig::from_toml(&format!(
            "port = 9\npublic_host = \"h\"\nregistry = \"m\"\ndatabase = \"d\"\n[pool]\nchain = [\"alpha\"]\ndaily_cap = 1\n{accounts}"
        ))
        .unwrap()
    };
    let keys = BTreeMap::from([
        ("KA".to_string(), "k".to_string()),
        ("KA2".to_string(), "k".to_string()),
    ]);
    let try_start = |cfg: ProxyConfig, dir: &Path| {
        Proxy::new(
            cfg,
            reg.clone(),
            dir,
            Store::memory().unwrap(),
            &keys,
            Arc::new(HttpUpstream::new(Duration::from_secs(1))),
            Box::new(|| T0),
        )
    };
    let one = "[[account]]\nprovider = \"alpha\"\nkey_env = \"KA\"\nadr = \"alpha.md\"\n";
    // No ADR in an empty directory.
    let empty = tempfile::tempdir().unwrap();
    let e = try_start(base(one), empty.path()).unwrap_err();
    assert!(e.contains("terms ADR"), "{e}");
    // Two accounts on one provider.
    let two = format!("{one}{}", one.replace("\"KA\"", "\"KA2\""));
    let e = try_start(base(&two), fx.dir.path()).unwrap_err();
    assert!(e.contains("exactly one account per provider"), "{e}");
    // The ADR present and cleared: it starts.
    assert!(try_start(base(one), fx.dir.path()).is_ok());
}
