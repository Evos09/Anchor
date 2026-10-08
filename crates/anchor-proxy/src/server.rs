//! The free-tier proxy's HTTP surface (Update #1 §4). It listens on the
//! loopback behind a TLS-terminating reverse proxy and serves:
//!
//! * `POST /v1/anchor/register`: a free account for a device, no signup;
//! * `GET /v1/anchor/usage`: the three cap meters;
//! * `GET /v1/models`: the free-tier models;
//! * `POST /v1/chat/completions`: the request, forwarded within the caps.
//!
//! A request's content is read in memory to estimate it and is forwarded;
//! it is never written, logged or kept. The only record is
//! [`LogRecord`](crate::log::LogRecord).

use std::collections::{BTreeMap, VecDeque};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anchor_hub::http::{Request, Response, Service};
use anchor_inference::{ModelEntry, Path, Registry, estimate};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::caps::{Cap, UserCaps};
use crate::config::ProxyConfig;
use crate::ledger::{Ledger, Refusal};
use crate::log::LogRecord;
use crate::pool::{KeyState, Outcome, Pool, PoolRefusal};
use crate::store::{Account, Store};
use crate::upstream::Upstream;

/// What a user sees when the pool cannot serve them (§5.6): a choice,
/// not an error.
pub const POOL_FULL: &str = "Free tier is at capacity right now. Two options: \
1. Add your own API key for unlimited use. 2. Run a local model with Ollama. \
Or wait: the pool recovers as usage ages out.";

/// Registrations accepted per minute, across all callers (in memory: no
/// address is stored).
const REGISTRATIONS_PER_MINUTE: usize = 30;

pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Box::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

struct Inner {
    ledger: Ledger,
    pool: Pool,
    store: Store,
    registrations: VecDeque<u64>,
}

pub struct Proxy {
    cfg: ProxyConfig,
    registry: Registry,
    /// provider → (base URL, key). Keys never leave this map.
    accounts: BTreeMap<String, (String, String)>,
    upstream: Arc<dyn Upstream>,
    clock: Clock,
    port: u16,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("providers", &self.accounts.keys())
            .finish()
    }
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    // A failure of the OS RNG is not survivable for a token issuer.
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn err(status: u16, kind: &str, message: &str, extra: Value) -> Response {
    let mut e = json!({ "type": kind, "message": message });
    if let (Some(o), Some(x)) = (e.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    Response::json(status, &json!({ "error": e }))
}

fn when(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => format!("at {ms} (ms since 1970, UTC)"),
        None => "when your next window opens".into(),
    }
}

fn cap_refusal(r: &Refusal) -> Response {
    let msg = match r.cap {
        Cap::FiveHour => "You've reached the free tier's 5-hour limit.",
        Cap::Weekly => "You've reached the free tier's weekly limit.",
        Cap::Monthly => "You've reached the free tier's monthly limit.",
    };
    err(
        429,
        "anchor_cap",
        &format!(
            "{msg} It frees up {}. Add your own API key or run a local model to keep going.",
            when(r.resets_at_ms)
        ),
        json!({ "cap": r.cap, "resets_at_ms": r.resets_at_ms, "needed": r.needed, "remaining": r.remaining }),
    )
}

fn pool_refusal(p: &PoolRefusal) -> Response {
    err(429, "anchor_pool", POOL_FULL, json!({ "reason": p }))
}

impl Proxy {
    /// Build the proxy from a validated config; `keys` maps each account's
    /// `key_env` to its value.
    pub fn new(
        cfg: ProxyConfig,
        registry: Registry,
        base: &FsPath,
        store: Store,
        keys: &BTreeMap<String, String>,
        upstream: Arc<dyn Upstream>,
        clock: Clock,
    ) -> Result<Proxy, String> {
        cfg.validate(&registry, base).map_err(|e| e.to_string())?;
        let mut accounts = BTreeMap::new();
        let mut key_states = BTreeMap::new();
        for a in &cfg.accounts {
            let key = keys
                .get(&a.key_env)
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    format!("the key for {:?} (${}) is not set", a.provider, a.key_env)
                })?;
            let base_url = registry
                .provider(&a.provider)
                .map(|p| p.base_url.clone())
                .ok_or_else(|| format!("provider {:?} is not in the registry", a.provider))?;
            accounts.insert(a.provider.clone(), (base_url, key.clone()));
            let mut ks = store
                .load_key(&a.provider)
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| KeyState::new(&a.provider, a.limits));
            ks.limits = a.limits;
            key_states.insert(a.provider.clone(), ks);
        }
        let mut pool = Pool::new(cfg.pool.clone(), key_states);
        if let Some(p) = store.load_pool().map_err(|e| e.to_string())? {
            pool.restore(&p);
        }
        let mut ledger = Ledger::new(cfg.caps.clone(), cfg.ledger.clone());
        for (u, c) in store.all_caps().map_err(|e| e.to_string())? {
            ledger.add_user(&u, c);
        }
        let port = cfg.port;
        Ok(Proxy {
            cfg,
            registry,
            accounts,
            upstream,
            clock,
            port,
            inner: Mutex::new(Inner {
                ledger,
                pool,
                store,
                registrations: VecDeque::new(),
            }),
        })
    }

    /// For tests that bind port 0: the port the server actually got.
    pub fn set_port(&mut self, port: u16) {
        self.port = port;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn user(&self, req: &Request) -> Result<String, Response> {
        let token = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let unauthorized = || {
            err(
                401,
                "anchor_auth",
                "a free-tier token is required",
                json!({}),
            )
        };
        let token = token.ok_or_else(unauthorized)?;
        let g = self.lock();
        match g.store.account_by_token(&sha256_hex(token)) {
            Ok(Some(a)) => Ok(a.user_id),
            Ok(None) => Err(unauthorized()),
            Err(e) => Err(err(500, "anchor_store", &e.to_string(), json!({}))),
        }
    }

    fn register(&self, req: &Request) -> Response {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let Some(device) = body.get("device").and_then(Value::as_str) else {
            return err(400, "anchor_bad_request", "device required", json!({}));
        };
        if !(32..=128).contains(&device.len()) || !device.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return err(
                400,
                "anchor_bad_request",
                "device must be 32-128 letters and digits",
                json!({}),
            );
        }
        let now = (self.clock)();
        let mut g = self.lock();
        while g.registrations.front().is_some_and(|t| t + 60_000 <= now) {
            g.registrations.pop_front();
        }
        if g.registrations.len() >= REGISTRATIONS_PER_MINUTE {
            return err(
                429,
                "anchor_busy",
                "too many sign-ups right now; try again in a minute",
                json!({}),
            );
        }
        g.registrations.push_back(now);
        let device_hash = sha256_hex(device);
        let token = random_hex(32);
        let token_hash = sha256_hex(&token);
        let result = match g.store.account_by_device(&device_hash) {
            // The same device gets its account back (a new token): caps
            // cannot be reset by registering again.
            Ok(Some(a)) => g.store.rotate_token(&a.user_id, &token_hash).map(|_| a),
            Ok(None) => {
                let a = Account {
                    user_id: random_hex(16),
                    created_ms: now,
                    caps: UserCaps::new(now),
                };
                g.store
                    .insert_account(&a, &token_hash, &device_hash)
                    .map(|_| a)
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(a) => {
                if g.ledger.user(&a.user_id).is_none() {
                    g.ledger.add_user(&a.user_id, a.caps.clone());
                }
                Response::json(
                    200,
                    &json!({ "token": token, "user_id": a.user_id, "created_ms": a.created_ms }),
                )
            }
            Err(e) => err(500, "anchor_store", &e.to_string(), json!({})),
        }
    }

    fn usage(&self, user: &str) -> Response {
        let now = (self.clock)();
        let g = self.lock();
        let Some(s) = g.ledger.status(user, now) else {
            return err(404, "anchor_auth", "unknown account", json!({}));
        };
        Response::json(
            200,
            &json!({
                "now_ms": now,
                "caps": s,
                "pool": {
                    "providers": self.accounts.len(),
                    "saturated": g.pool.saturated(),
                },
            }),
        )
    }

    fn served(&self) -> impl Iterator<Item = &ModelEntry> {
        self.registry
            .free_tier()
            .filter(|m| self.accounts.contains_key(&m.provider))
    }

    fn models(&self) -> Response {
        let data: Vec<Value> = self
            .served()
            .map(|m| {
                json!({
                    "id": m.id, "object": "model", "owned_by": m.provider,
                    "display_name": m.display_name, "family": m.family,
                    "context_window": m.context_window, "license": m.license_id,
                })
            })
            .collect();
        Response::json(200, &json!({ "object": "list", "data": data }))
    }

    fn chat(&self, user: &str, req: &Request) -> Response {
        let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
            return err(400, "anchor_bad_request", "the body is not JSON", json!({}));
        };
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            return err(
                400,
                "anchor_bad_request",
                "this endpoint returns whole responses; send stream: false",
                json!({}),
            );
        }
        let Some(model) = body.get("model").and_then(Value::as_str) else {
            return err(400, "anchor_bad_request", "model required", json!({}));
        };
        let candidates: Vec<&ModelEntry> = self
            .registry
            .free_tier_equivalents(model)
            .into_iter()
            .filter(|m| self.accounts.contains_key(&m.provider))
            .collect();
        if candidates.is_empty() {
            return err(
                404,
                "anchor_model",
                &format!("{model} is not on the free tier"),
                json!({}),
            );
        }
        let input_est = estimate::request_input_tokens(&body);
        let asked = body.get("max_tokens").and_then(Value::as_u64);
        let weight = candidates
            .iter()
            .map(|m| m.free_tier_weight)
            .fold(0.0, f64::max);
        let started = (self.clock)();

        // Pre-flight and reserve against the user's caps, then the pool.
        let mut r = {
            let mut g = self.lock();
            let r = match g.ledger.reserve(user, started, input_est, asked, weight) {
                Ok(r) => r,
                Err(refusal) => return cap_refusal(&refusal),
            };
            if let Err(p) = g.pool.admit(user, started, r.held) {
                g.ledger.release(&r);
                return pool_refusal(&p);
            }
            r
        };

        let providers: Vec<String> = candidates.iter().map(|m| m.provider.clone()).collect();
        let mut last_error = PoolRefusal::NoKeyAvailable;
        for _ in 0..providers.len().max(1) * 2 {
            let now = (self.clock)();
            let chosen = {
                let mut g = self.lock();
                match g
                    .pool
                    .select(&providers, now, input_est + r.max_output_tokens)
                {
                    Ok(p) => p,
                    Err(p) => {
                        last_error = p;
                        break;
                    }
                }
            };
            let Some(entry) = candidates.iter().find(|m| m.provider == chosen) else {
                break;
            };
            let Some((base_url, key)) = self.accounts.get(&chosen) else {
                break;
            };
            let mut fwd = body.clone();
            fwd["model"] = json!(entry.provider_model_id);
            fwd["stream"] = json!(true);
            fwd["stream_options"] = json!({ "include_usage": true });
            fwd["max_tokens"] = json!(r.max_output_tokens);
            let cut_raw = (r.output_cut_at as f64 / entry.free_tier_weight.max(f64::MIN_POSITIVE))
                .ceil() as u64;
            let t = Instant::now();
            let res = self.upstream.chat(base_url, key, &fwd, cut_raw);
            drop(fwd);
            let latency_ms = t.elapsed().as_millis() as u64;
            let now = (self.clock)();
            let mut g = self.lock();
            let g = &mut *g;
            g.pool.learn(&chosen, &res.headers);
            let record = |input: u64, output: u64, status: u16| LogRecord {
                user_id: user.to_string(),
                timestamp_ms: now,
                provider: chosen.clone(),
                model: entry.id.clone(),
                input_tokens: input,
                output_tokens: output,
                http_status: status,
                latency_ms,
                path: Path::A,
            };
            let Some(done) = res.completion.filter(|_| res.status == 200) else {
                g.pool.record(
                    &chosen,
                    user,
                    now,
                    &Outcome::Failed {
                        status: res.status,
                        retry_after_secs: res.retry_after_secs,
                        quota: res.quota,
                    },
                    0,
                );
                let _ = g.store.log(&record(0, 0, res.status));
                if let Some(k) = g.pool.keys.get(&chosen) {
                    let _ = g.store.save_key(&chosen, k);
                }
                last_error = PoolRefusal::NoKeyAvailable;
                continue;
            };
            // Reconcile on the provider's count when it gave one.
            let (input, output) = done.usage.unwrap_or((input_est, done.counted_output));
            r.weight = entry.free_tier_weight;
            let charged = g.ledger.settle(&r, now, input, output);
            g.pool.record(
                &chosen,
                user,
                now,
                &Outcome::Ok {
                    tokens: input + output,
                },
                charged,
            );
            let _ = g.store.log(&record(input, output, 200));
            if let Some(c) = g.ledger.user(user) {
                let _ = g.store.save_caps(user, c);
            }
            if let Some(k) = g.pool.keys.get(&chosen) {
                let _ = g.store.save_key(&chosen, k);
            }
            let _ = g.store.save_pool(&g.pool.snapshot());
            let mut resp = Response::json(200, &done.to_completion(model));
            if done.truncated {
                resp.headers.push(("X-Anchor-Truncated", "1".into()));
            }
            if r.wrap_up {
                resp.headers.push(("X-Anchor-Wrap-Up", "1".into()));
            }
            return resp;
        }
        let mut g = self.lock();
        g.ledger.release(&r);
        let _ = g.store.save_pool(&g.pool.snapshot());
        pool_refusal(&last_error)
    }
}

impl Service for Proxy {
    fn handle(&self, req: &Request) -> Response {
        let host_ok = req.header("host").is_some_and(|h| {
            h == self.cfg.public_host
                || h == format!("127.0.0.1:{}", self.port)
                || h == format!("localhost:{}", self.port)
        });
        if !host_ok {
            return err(403, "anchor_host", "unexpected Host", json!({}));
        }
        // Not for browsers: the app calls it from its backend.
        if req.header("origin").is_some() {
            return err(
                403,
                "anchor_origin",
                "cross-origin requests are refused",
                json!({}),
            );
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/anchor/register") => self.register(req),
            ("GET", "/v1/anchor/usage") => match self.user(req) {
                Ok(u) => self.usage(&u),
                Err(r) => r,
            },
            ("GET", "/v1/models") => match self.user(req) {
                Ok(_) => self.models(),
                Err(r) => r,
            },
            ("POST", "/v1/chat/completions") => match self.user(req) {
                Ok(u) => self.chat(&u, req),
                Err(r) => r,
            },
            _ => err(404, "anchor_not_found", "not found", json!({})),
        }
    }
}
