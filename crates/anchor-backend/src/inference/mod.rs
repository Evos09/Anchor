//! The desktop app's three inference paths (Update #1 §2, ADR-0029):
//!
//! * **A, the free tier**: Anchor's capped pool, through `anchor-proxy`;
//! * **B, your own key**: straight from this computer to the provider,
//!   the key kept in the system keychain ([`keys`]);
//! * **C, local**: Ollama on this computer ([`net::detect_ollama`]).
//!
//! Per task, the router (`anchor_inference::router`) picks a path from
//! the user's preference, a `/local` `/byok` `/free` override, the
//! project's pin and what is available; [`plan`] then writes the
//! runtime's models file for that task. When nothing can serve a task the
//! answer is a prompt for the user, never an error.

pub mod keys;
pub mod net;
pub mod plan;

use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anchor_inference::router::{self, Request};
use anchor_inference::{
    Availability, Decision, FreeState, Path, Preference, PromptReason, Registry,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use keys::KeyStore;
use plan::{Candidate, LOCAL_CONTEXT_CAP, Plan};

/// The registry the app ships with (a copy of `config/models.toml`).
pub const REGISTRY_TOML: &str = include_str!("../../assets/models.toml");

/// Context assumed for a BYOK model the registry does not list.
pub const UNLISTED_CONTEXT: u64 = 32_768;

pub fn bundled_registry() -> Result<Registry, String> {
    Registry::from_toml(REGISTRY_TOML).map_err(|e| e.to_string())
}

/// Where a task's models come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Chosen per task by the router.
    #[default]
    Routed,
    /// A models (roles) file the user picked (the Phase 9 behaviour).
    File,
}

/// A BYOK provider's settings. The key itself is in the keychain.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ByokChoice {
    /// The model to run, from the provider's own list.
    pub model: String,
}

fn default_port() -> u16 {
    net::OLLAMA_PORT
}

/// What the app remembers about inference (in `app.json`; no secrets).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub source: Source,
    pub preference: Preference,
    /// A custom fallback order, replacing the preference's.
    pub order: Option<Vec<Path>>,
    /// Projects pinned to one path (e.g. a private repo to local only).
    pub pins: BTreeMap<PathBuf, Path>,
    #[serde(default = "default_port")]
    pub ollama_port: u16,
    /// Providers with a key in the keychain, and the model chosen.
    pub byok: BTreeMap<String, ByokChoice>,
    /// The executor model to prefer on each path.
    pub preferred: BTreeMap<Path, String>,
    /// Families the user gave for local models the registry does not list.
    pub local_families: BTreeMap<String, String>,
    /// This install's random free-tier device id (not a secret; ADR-0037).
    pub free_device: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            source: Source::Routed,
            preference: Preference::Auto,
            order: None,
            pins: BTreeMap::new(),
            ollama_port: net::OLLAMA_PORT,
            byok: BTreeMap::new(),
            preferred: BTreeMap::new(),
            local_families: BTreeMap::new(),
            free_device: None,
        }
    }
}

/// The app's inference services.
#[derive(Debug, Clone)]
pub struct Inference {
    pub registry: Registry,
    pub keys: Arc<dyn KeyStore>,
    /// The free tier's base URL; `None` while no free tier is offered.
    pub free_url: Option<String>,
    pub local_timeout: Duration,
    pub net_timeout: Duration,
}

/// What a task runs on, or what to ask the user.
#[derive(Debug)]
pub enum Routed {
    Run {
        plan: Box<Plan>,
        skipped: Vec<(Path, String)>,
        /// The task text without its `/local` `/byok` `/free` prefix.
        text: String,
    },
    Prompt(Value),
}

struct Local {
    ollama: Result<net::Ollama, String>,
    ready: Vec<Candidate>,
    /// Installed models Anchor cannot use until the user names a family.
    needs_family: Vec<net::LocalModel>,
}

struct Free {
    state: FreeState,
    started: bool,
    usage: Option<Value>,
    token: Option<String>,
}

/// "2026-10-08 14:00 UTC".
pub fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn cap_name(cap: &str) -> &str {
    match cap {
        "five_hour" => "5-hour",
        "weekly" => "weekly",
        "monthly" => "monthly",
        other => other,
    }
}

impl Inference {
    pub fn new(keys: Arc<dyn KeyStore>, free_url: Option<String>) -> Result<Inference, String> {
        Ok(Inference {
            registry: bundled_registry()?,
            keys,
            free_url: free_url.filter(|u| !u.is_empty()),
            local_timeout: Duration::from_millis(1500),
            net_timeout: Duration::from_secs(10),
        })
    }

    fn local(&self, s: &Settings) -> Local {
        let ollama = net::detect_ollama(s.ollama_port, self.local_timeout);
        let base_url = format!("http://127.0.0.1:{}/v1", s.ollama_port);
        let (mut ready, mut needs_family) = (Vec::new(), Vec::new());
        if let Ok(o) = &ollama {
            // Registry order first: it lists the preferred models first.
            let mut known: Vec<(usize, Candidate)> = Vec::new();
            for m in &o.models {
                let entry = self
                    .registry
                    .models
                    .iter()
                    .enumerate()
                    .find(|(_, e)| e.provider == "ollama" && e.provider_model_id == m.name);
                let family = entry
                    .map(|(_, e)| e.family.clone())
                    .or_else(|| s.local_families.get(&m.name).cloned());
                let Some(family) = family else {
                    needs_family.push(m.clone());
                    continue;
                };
                let rank = entry.map_or(usize::MAX, |(i, _)| i);
                known.push((
                    rank,
                    Candidate {
                        path: Path::C,
                        label: entry.map_or(m.name.clone(), |(_, e)| e.display_name.clone()),
                        base_url: base_url.clone(),
                        model_id: m.name.clone(),
                        family,
                        context_window: entry.map_or(LOCAL_CONTEXT_CAP, |(_, e)| {
                            e.context_window.min(LOCAL_CONTEXT_CAP)
                        }),
                        price_in: 0.0,
                        price_out: 0.0,
                        key: None,
                    },
                ));
            }
            known.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.model_id.cmp(&b.1.model_id)));
            ready = known.into_iter().map(|(_, c)| c).collect();
        }
        Local {
            ollama,
            ready,
            needs_family,
        }
    }

    /// BYOK candidates, with their keys (read from the keychain now).
    fn byok(&self, s: &Settings) -> (Vec<Candidate>, Vec<(String, String)>) {
        let (mut out, mut problems) = (Vec::new(), Vec::new());
        for (provider, choice) in &s.byok {
            let Some(p) = self
                .registry
                .provider(provider)
                .filter(|p| p.paths.contains(&Path::B))
            else {
                continue;
            };
            if choice.model.is_empty() {
                problems.push((provider.clone(), "pick a model".to_string()));
                continue;
            }
            let key = match self.keys.get(&keys::byok_entry(provider)) {
                Ok(Some(k)) => k,
                Ok(None) => {
                    problems.push((
                        provider.clone(),
                        "the key is no longer in the keychain".into(),
                    ));
                    continue;
                }
                Err(e) => {
                    problems.push((provider.clone(), e));
                    continue;
                }
            };
            let entry = self.registry.by_provider_model(provider, &choice.model);
            out.push(Candidate {
                path: Path::B,
                label: format!("{} {}", p.display_name, choice.model),
                base_url: p.base_url.clone(),
                model_id: choice.model.clone(),
                family: entry.map_or(p.family.clone(), |e| e.family.clone()),
                context_window: entry.map_or(UNLISTED_CONTEXT, |e| e.context_window),
                price_in: entry.map_or(0.0, |e| e.input_price),
                price_out: entry.map_or(0.0, |e| e.output_price),
                key: Some(key),
            });
        }
        (out, problems)
    }

    fn free(&self) -> Free {
        let unavailable = |reason: &str, started: bool| Free {
            state: FreeState::Unavailable {
                reason: reason.to_string(),
            },
            started,
            usage: None,
            token: None,
        };
        let Some(url) = &self.free_url else {
            return unavailable(
                "the free tier is not offered yet: no provider's terms allow it (see Settings → Models)",
                false,
            );
        };
        let token = match self.keys.get(keys::FREE_TOKEN) {
            Ok(Some(t)) => t,
            Ok(None) => return unavailable("not started yet: choose Start free", false),
            Err(e) => return unavailable(&e, false),
        };
        match net::free_usage(url, &token, self.net_timeout) {
            Ok(usage) => {
                let blocked = usage.pointer("/caps/blocked_by").and_then(Value::as_str);
                let state = match blocked {
                    Some(cap) => {
                        let resets = usage
                            .pointer(&format!("/caps/{cap}/resets_at_ms"))
                            .and_then(Value::as_u64)
                            .map(|ms| format!("; it resets at {}", utc(ms)))
                            .unwrap_or_default();
                        FreeState::Exhausted {
                            reason: format!("your {} free limit is used up{resets}", cap_name(cap)),
                        }
                    }
                    None => FreeState::Available,
                };
                Free {
                    state,
                    started: true,
                    usage: Some(usage),
                    token: Some(token),
                }
            }
            Err(e) if e.status == 401 || e.status == 404 => unavailable(
                "the free tier no longer knows this install: choose Start free again",
                false,
            ),
            Err(e) => unavailable(&e.message, true),
        }
    }

    fn free_candidates(&self, token: &str) -> Vec<Candidate> {
        let Some(url) = &self.free_url else {
            return Vec::new();
        };
        let Ok(v) = net::free_models(url, token, self.net_timeout) else {
            return Vec::new();
        };
        let mut seen = std::collections::BTreeSet::new();
        v.get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                let family = m.get("family")?.as_str()?.to_string();
                seen.insert(id.clone()).then(|| Candidate {
                    path: Path::A,
                    label: m
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_string(),
                    base_url: format!("{}/v1", url.trim_end_matches('/')),
                    model_id: id,
                    family,
                    context_window: m
                        .get("context_window")
                        .and_then(Value::as_u64)
                        .unwrap_or(UNLISTED_CONTEXT),
                    price_in: 0.0,
                    price_out: 0.0,
                    key: Some(token.to_string()),
                })
            })
            .collect()
    }

    /// Everything the settings page and the meters show.
    pub fn status(&self, s: &Settings, project: Option<&FsPath>) -> Value {
        let local = self.local(s);
        let (byok, problems) = self.byok(s);
        let free = self.free();
        let providers: Vec<Value> = self
            .registry
            .providers
            .iter()
            .filter(|p| p.paths.contains(&Path::B))
            .map(|p| {
                let choice = s.byok.get(&p.id);
                json!({
                    "id": p.id, "name": p.display_name, "key_url": p.key_url,
                    "has_key": choice.is_some(),
                    "model": choice.map(|c| &c.model),
                    "ready": byok.iter().any(|c| c.base_url == p.base_url),
                    "problem": problems.iter().find(|(id, _)| id == &p.id).map(|(_, w)| w),
                })
            })
            .collect();
        let license = |name: &str| {
            self.registry.by_provider_model("ollama", name).map(|e| {
                json!({ "id": e.license_id, "commercial": e.license_commercial_ok,
                                 "gated": e.license_revenue_gate, "notes": e.license_notes })
            })
        };
        json!({
            "source": s.source,
            "preference": s.preference,
            "order": s.order,
            "pin": project.and_then(|p| s.pins.get(p)),
            "preferred": s.preferred,
            "local": {
                "port": s.ollama_port,
                "running": local.ollama.is_ok(),
                "version": local.ollama.as_ref().ok().map(|o| &o.version),
                "error": local.ollama.as_ref().err(),
                "ready": local.ready.iter().map(|c| json!({
                    "model": c.model_id, "label": c.label, "family": c.family,
                    "license": license(&c.model_id),
                })).collect::<Vec<_>>(),
                "needs_family": local.needs_family.iter().map(|m| json!({
                    "model": m.name, "hint": m.ollama_family, "size": m.parameter_size,
                })).collect::<Vec<_>>(),
            },
            "byok": providers,
            "free": {
                "offered": self.free_url.is_some(),
                "started": free.started,
                "state": free.state,
                "usage": free.usage,
            },
        })
    }

    /// Route one task and write its models file.
    pub fn route(&self, s: &Settings, project: &FsPath, text: &str) -> Routed {
        let (override_, rest) = router::parse_override(text);
        let rest = rest.trim().to_string();
        let local = self.local(s);
        let (byok, _) = self.byok(s);
        let free = self.free();
        let free_cands = match (&free.state, &free.token) {
            (FreeState::Available, Some(t)) => self.free_candidates(t),
            _ => Vec::new(),
        };
        // A free tier serving no model cannot run a task.
        let free_state = if free.state == FreeState::Available && free_cands.is_empty() {
            FreeState::Unavailable {
                reason: "the free tier serves no model right now".into(),
            }
        } else {
            free.state.clone()
        };
        let avail = Availability {
            local: !local.ready.is_empty(),
            byok: !byok.is_empty(),
            free: free_state,
        };
        let pin = s.pins.get(project).copied();
        let req = Request {
            preference: s.preference,
            order: s.order.clone(),
            override_,
            pin,
            complexity: router::classify(&rest),
        };
        let on = |p: Path| match p {
            Path::A => free_cands.clone(),
            Path::B => byok.clone(),
            Path::C => local.ready.clone(),
        };
        match router::route(&req, &avail) {
            Decision::Use { path, skipped } => {
                // The reviewer may come from another available path only
                // when the user has not confined this task to one.
                let confined =
                    pin.is_some() || override_.is_some() || s.preference == Preference::FreeOnly;
                let others: Vec<Candidate> = if confined {
                    Vec::new()
                } else {
                    let order = match &s.order {
                        Some(o) if !o.is_empty() => o.clone(),
                        _ => router::default_order(s.preference, req.complexity),
                    };
                    order
                        .into_iter()
                        .chain([Path::C, Path::B, Path::A])
                        .filter(|p| *p != path)
                        .fold(Vec::<Path>::new(), |mut v, p| {
                            if !v.contains(&p) {
                                v.push(p);
                            }
                            v
                        })
                        .into_iter()
                        .flat_map(on)
                        .collect()
                };
                match plan::choose(
                    path,
                    &on(path),
                    s.preferred.get(&path).map(String::as_str),
                    &others,
                ) {
                    Ok((exec, reviewer)) => Routed::Run {
                        plan: Box::new(plan::roles(path, &exec, &reviewer)),
                        skipped,
                        text: rest,
                    },
                    Err(why) => {
                        Routed::Prompt(prompt(&PromptReason::PathUnavailable { path, why }, &avail))
                    }
                }
            }
            Decision::Prompt(reason) => Routed::Prompt(prompt(&reason, &avail)),
        }
    }

    /// Check a key with its provider, then keep it in the keychain.
    pub fn add_key(&self, provider: &str, key: &str) -> Result<Vec<String>, String> {
        let p = self
            .registry
            .provider(provider)
            .filter(|p| p.paths.contains(&Path::B))
            .ok_or("unknown provider")?;
        let key = key.trim();
        if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) {
            return Err("that does not look like an API key".into());
        }
        let models = net::verify_key(&p.base_url, key, self.net_timeout)?;
        self.keys.set(&keys::byok_entry(provider), key)?;
        Ok(models)
    }

    /// Wipe a key from the keychain (and check it is gone).
    pub fn remove_key(&self, provider: &str) -> Result<(), String> {
        let name = keys::byok_entry(provider);
        self.keys.delete(&name)?;
        match self.keys.get(&name)? {
            None => Ok(()),
            Some(_) => Err("the keychain still holds the key; remove it there by hand".into()),
        }
    }

    /// The models a stored key can use (for changing the model later).
    pub fn key_models(&self, provider: &str) -> Result<Vec<String>, String> {
        let p = self.registry.provider(provider).ok_or("unknown provider")?;
        let key = self
            .keys
            .get(&keys::byok_entry(provider))?
            .ok_or("no key is stored for this provider")?;
        net::verify_key(&p.base_url, &key, self.net_timeout)
    }

    /// Start the free tier: register this install (no signup).
    pub fn start_free(&self, device: &str) -> Result<(), String> {
        let url = self
            .free_url
            .as_ref()
            .ok_or("the free tier is not offered yet")?;
        let token = net::free_register(url, device, self.net_timeout).map_err(|e| e.message)?;
        self.keys.set(keys::FREE_TOKEN, &token)
    }
}

/// The page's prompt for a task nothing can run.
fn prompt(reason: &PromptReason, avail: &Availability) -> Value {
    let free_line = match &avail.free {
        FreeState::Available => None,
        FreeState::Exhausted { reason } | FreeState::Unavailable { reason } => Some(reason.clone()),
    };
    let message = match reason {
        PromptReason::NothingAvailable { .. } => {
            "No model is ready to run this task yet.".to_string()
        }
        PromptReason::PathUnavailable { path, why } => {
            format!(
                "This task asks for {}, which is not ready: {why}.",
                path.describe()
            )
        }
    };
    json!({
        "reason": reason,
        "message": message,
        "options": [
            { "path": "C", "title": "Run locally — nothing leaves your machine",
              "detail": "Install Ollama and a model; Anchor finds it by itself.",
              "ready": avail.local },
            { "path": "B", "title": "Add your own API key",
              "detail": "Unlimited use, billed by your provider. The key stays in your system keychain.",
              "ready": avail.byok },
            { "path": "A", "title": "Free tier",
              "detail": free_line.clone().unwrap_or_else(|| "Available.".into()),
              "ready": free_line.is_none() },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_registry_is_valid_and_offers_nothing_free_yet() {
        let r = bundled_registry().unwrap();
        // No provider has cleared the terms gate (docs/adr/providers/).
        assert_eq!(r.free_tier().count(), 0);
        assert!(r.on_path(Path::C).count() > 5);
        // Every BYOK provider says which family its models are.
        assert!(
            r.providers
                .iter()
                .filter(|p| p.paths.contains(&Path::B))
                .all(|p| !p.family.is_empty())
        );
    }

    #[test]
    fn times_are_shown_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_793_491_200_000), "2026-11-01 00:00 UTC");
    }

    #[test]
    fn nothing_available_is_a_prompt_with_the_three_ways_forward() {
        let inf = Inference {
            local_timeout: Duration::from_millis(300),
            ..Inference::new(Arc::new(keys::MemoryKeys::default()), None).unwrap()
        };
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let s = Settings {
            ollama_port: closed,
            ..Settings::default()
        };
        let Routed::Prompt(p) = inf.route(&s, FsPath::new("/p"), "fix the typo") else {
            panic!("expected a prompt");
        };
        assert_eq!(p["reason"]["kind"], "nothing_available");
        assert_eq!(p["options"].as_array().unwrap().len(), 3);
        assert!(
            p["options"][2]["detail"]
                .as_str()
                .unwrap()
                .contains("not offered yet")
        );
        // An override for a missing path names it.
        let Routed::Prompt(p) = inf.route(&s, FsPath::new("/p"), "/byok fix the typo") else {
            panic!("expected a prompt");
        };
        assert_eq!(p["reason"]["kind"], "path_unavailable");
        assert_eq!(p["reason"]["path"], "B");
    }

    #[test]
    fn settings_round_trip_through_json_with_defaults() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.ollama_port, 11434);
        let mut s = Settings::default();
        s.pins.insert(PathBuf::from("/repo"), Path::C);
        s.preferred.insert(Path::C, "qwen2.5-coder:7b".into());
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
