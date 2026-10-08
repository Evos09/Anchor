//! The pool manager (Update #1 §5, ADR-0032): one vendor key per
//! provider, each tracked on four sliding windows (requests and tokens per
//! minute and per day). It picks the healthiest key for a request, cools
//! keys down on failure, falls over to the next provider in the chain,
//! and shares the pool fairly between users when it runs short.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::caps::{DAY_MS, HOUR_MS};

const MINUTE_MS: u64 = 60_000;

/// A provider's documented free-tier limits (`None`: not limited, or not
/// published).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub rpm: Option<u64>,
    #[serde(default)]
    pub rpd: Option<u64>,
    #[serde(default)]
    pub tpm: Option<u64>,
    #[serde(default)]
    pub tpd: Option<u64>,
}

/// Why a key is cooling down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cooldown {
    RateLimited,
    QuotaExceeded,
    ServerError,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyState {
    pub provider: String,
    pub limits: Limits,
    /// (time, tokens) of each request in the last day.
    pub events: VecDeque<(u64, u64)>,
    pub cooldown_until: u64,
    pub cooldown: Option<Cooldown>,
    /// Consecutive server errors, for the backoff.
    pub server_errors: u32,
    /// Refused authentication: out of rotation until an operator acts.
    pub disabled: bool,
}

/// One dimension's use against its limit.
fn frac(used: u64, limit: Option<u64>) -> f64 {
    match limit {
        None => 0.0,
        Some(0) => 1.0,
        Some(l) => used as f64 / l as f64,
    }
}

impl KeyState {
    pub fn new(provider: &str, limits: Limits) -> KeyState {
        KeyState {
            provider: provider.to_string(),
            limits,
            ..KeyState::default()
        }
    }

    fn prune(&mut self, now: u64) {
        while self.events.front().is_some_and(|(t, _)| *t + DAY_MS <= now) {
            self.events.pop_front();
        }
    }

    /// (requests, tokens) in the last `span` ms.
    fn used(&self, now: u64, span: u64) -> (u64, u64) {
        self.events
            .iter()
            .filter(|(t, _)| *t + span > now)
            .fold((0, 0), |(r, k), (_, n)| (r + 1, k + n))
    }

    /// The fraction used of the tightest of the four windows, counting a
    /// request of `tokens` about to be made.
    pub fn load(&self, now: u64, tokens: u64) -> f64 {
        let (rm, tm) = self.used(now, MINUTE_MS);
        let (rd, td) = self.used(now, DAY_MS);
        [
            frac(rm + 1, self.limits.rpm),
            frac(rd + 1, self.limits.rpd),
            frac(tm + tokens, self.limits.tpm),
            frac(td + tokens, self.limits.tpd),
        ]
        .into_iter()
        .fold(0.0, f64::max)
    }

    pub fn usable(&self, now: u64, tokens: u64) -> bool {
        !self.disabled && now >= self.cooldown_until && self.load(now, tokens) <= 1.0
    }

    /// Tokens left today, if the provider publishes a daily limit.
    pub fn tokens_left_today(&self, now: u64) -> Option<u64> {
        let (_, td) = self.used(now, DAY_MS);
        self.limits.tpd.map(|l| l.saturating_sub(td))
    }
}

/// What came back from a provider, for the pool's bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok {
        tokens: u64,
    },
    /// HTTP status, `Retry-After` in seconds if sent, and whether the body
    /// says a quota (not a rate) was exhausted.
    Failed {
        status: u16,
        retry_after_secs: Option<u64>,
        quota: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    /// Providers in order of preference.
    pub chain: Vec<String>,
    /// Weighted tokens the whole pool may spend in 24 hours, across all
    /// users (separate from each user's caps).
    pub daily_cap: u64,
    /// The pool is saturated past this fraction of its daily cap.
    #[serde(default = "d_saturation")]
    pub saturation: f64,
    /// A user counts as active for this long after a request.
    #[serde(default = "d_active")]
    pub active_ms: u64,
}

fn d_saturation() -> f64 {
    0.8
}
fn d_active() -> u64 {
    15 * MINUTE_MS
}

/// Why the pool cannot take a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PoolRefusal {
    /// The pool's own daily cap is spent.
    PoolCapReached,
    /// Saturated, and this user is over their fair share: wait.
    OverFairShare { share: u64, used: u64 },
    /// Every key that serves the model is cooling down, disabled or full.
    NoKeyAvailable,
}

#[derive(Debug)]
pub struct Pool {
    pub cfg: PoolConfig,
    /// One key per provider (ADR-0032: never a second account).
    pub keys: BTreeMap<String, KeyState>,
    /// (time, user, weighted tokens) spent through the pool in the last day.
    spend: VecDeque<(u64, String, u64)>,
    /// When each user last made a request.
    seen: BTreeMap<String, u64>,
}

impl Pool {
    pub fn new(cfg: PoolConfig, keys: BTreeMap<String, KeyState>) -> Pool {
        Pool {
            cfg,
            keys,
            spend: VecDeque::new(),
            seen: BTreeMap::new(),
        }
    }

    fn prune(&mut self, now: u64) {
        while self
            .spend
            .front()
            .is_some_and(|(t, _, _)| *t + DAY_MS <= now)
        {
            self.spend.pop_front();
        }
        self.seen.retain(|_, t| *t + DAY_MS > now);
        for k in self.keys.values_mut() {
            k.prune(now);
        }
    }

    /// The pool's spend and activity, for the database.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({ "spend": self.spend, "seen": self.seen })
    }

    pub fn restore(&mut self, v: &serde_json::Value) {
        if let Ok(spend) = serde_json::from_value(v["spend"].clone()) {
            self.spend = spend;
        }
        if let Ok(seen) = serde_json::from_value(v["seen"].clone()) {
            self.seen = seen;
        }
    }

    pub fn pool_used(&self) -> u64 {
        self.spend.iter().map(|(_, _, n)| n).sum()
    }

    fn user_used(&self, user: &str) -> u64 {
        self.spend
            .iter()
            .filter(|(_, u, _)| u == user)
            .map(|(_, _, n)| n)
            .sum()
    }

    fn active_users(&self, now: u64, also: &str) -> u64 {
        let mut n = self
            .seen
            .iter()
            .filter(|(u, t)| **t + self.cfg.active_ms > now && u.as_str() != also)
            .count() as u64;
        n += 1; // the user asking is active
        n
    }

    pub fn saturated(&self) -> bool {
        self.pool_used() as f64 >= self.cfg.saturation * self.cfg.daily_cap as f64
    }

    /// Admission against the pool (not the user's own caps): the pool cap,
    /// then fair share when the pool is saturated. Work-conserving: below
    /// saturation users draw freely; at saturation the day's cap is split
    /// between the users active now, so idle users' shares go to those
    /// working, and a user past their share waits.
    pub fn admit(&mut self, user: &str, now: u64, weighted: u64) -> Result<(), PoolRefusal> {
        self.prune(now);
        if self.pool_used() + weighted > self.cfg.daily_cap {
            return Err(PoolRefusal::PoolCapReached);
        }
        if self.saturated() {
            let share = self.cfg.daily_cap / self.active_users(now, user);
            let used = self.user_used(user);
            if used + weighted > share {
                return Err(PoolRefusal::OverFairShare { share, used });
            }
        }
        Ok(())
    }

    /// The key to use among `providers` (those serving the model): the
    /// usable one with the most quota left across its four windows, the
    /// chain's order breaking ties. With one key per provider, a provider
    /// whose key is cooling down is simply passed over for the next.
    pub fn select(
        &mut self,
        providers: &[String],
        now: u64,
        tokens: u64,
    ) -> Result<String, PoolRefusal> {
        self.prune(now);
        let mut best: Option<(&String, f64)> = None;
        for p in self.cfg.chain.iter().filter(|p| providers.contains(p)) {
            let Some(k) = self.keys.get(p) else { continue };
            if !k.usable(now, tokens) {
                continue;
            }
            let load = k.load(now, tokens);
            if best.is_none_or(|(_, b)| load < b) {
                best = Some((p, load));
            }
        }
        best.map(|(p, _)| p.clone())
            .ok_or(PoolRefusal::NoKeyAvailable)
    }

    /// Book a request's result on its key (and, if it went through, on the
    /// pool and the user).
    pub fn record(
        &mut self,
        provider: &str,
        user: &str,
        now: u64,
        outcome: &Outcome,
        weighted: u64,
    ) {
        self.seen.insert(user.to_string(), now);
        let Some(k) = self.keys.get_mut(provider) else {
            return;
        };
        match *outcome {
            Outcome::Ok { tokens } => {
                k.events.push_back((now, tokens));
                k.server_errors = 0;
                k.cooldown = None;
                self.spend.push_back((now, user.to_string(), weighted));
            }
            Outcome::Failed {
                status,
                retry_after_secs,
                quota,
            } => {
                k.events.push_back((now, 0));
                match status {
                    401 | 403 => k.disabled = true,
                    429 if quota => {
                        k.cooldown = Some(Cooldown::QuotaExceeded);
                        k.cooldown_until = now + HOUR_MS;
                    }
                    429 => {
                        k.cooldown = Some(Cooldown::RateLimited);
                        k.cooldown_until = now + retry_after_secs.unwrap_or(10) * 1000;
                    }
                    500..=599 => {
                        k.server_errors = k.server_errors.saturating_add(1);
                        let backoff = 5_000u64 << (k.server_errors - 1).min(6);
                        k.cooldown = Some(Cooldown::ServerError);
                        k.cooldown_until = now + backoff;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Learn a provider's real limits from its rate-limit headers.
    pub fn learn(&mut self, provider: &str, headers: &[(String, String)]) {
        let Some(k) = self.keys.get_mut(provider) else {
            return;
        };
        for (name, value) in headers {
            let Ok(v) = value.trim().parse::<u64>() else {
                continue;
            };
            match name.to_ascii_lowercase().as_str() {
                "x-ratelimit-limit-requests" => k.limits.rpd = Some(v),
                "x-ratelimit-limit-tokens" => k.limits.tpm = Some(v),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_791_417_600_000;

    fn pool(daily_cap: u64) -> Pool {
        let mut keys = BTreeMap::new();
        keys.insert(
            "alpha".to_string(),
            KeyState::new(
                "alpha",
                Limits {
                    rpm: Some(30),
                    rpd: Some(1000),
                    tpm: Some(6000),
                    tpd: None,
                },
            ),
        );
        keys.insert(
            "beta".to_string(),
            KeyState::new(
                "beta",
                Limits {
                    rpm: Some(10),
                    ..Limits::default()
                },
            ),
        );
        Pool::new(
            PoolConfig {
                chain: vec!["alpha".into(), "beta".into()],
                daily_cap,
                saturation: 0.8,
                active_ms: 15 * MINUTE_MS,
            },
            keys,
        )
    }

    fn both() -> Vec<String> {
        vec!["alpha".into(), "beta".into()]
    }

    fn fail(status: u16) -> Outcome {
        Outcome::Failed {
            status,
            retry_after_secs: None,
            quota: false,
        }
    }

    #[test]
    fn a_rate_limited_key_cools_down_and_is_skipped() {
        let mut p = pool(1_000_000);
        assert_eq!(p.select(&both(), T0, 100).unwrap(), "alpha");
        p.record(
            "alpha",
            "u",
            T0,
            &Outcome::Failed {
                status: 429,
                retry_after_secs: Some(30),
                quota: false,
            },
            0,
        );
        assert_eq!(p.keys["alpha"].cooldown, Some(Cooldown::RateLimited));
        assert_eq!(p.select(&both(), T0 + 1000, 100).unwrap(), "beta");
        // Back once Retry-After has passed.
        assert_eq!(p.select(&both(), T0 + 30_000, 100).unwrap(), "alpha");
        // Without Retry-After, ten seconds.
        p.record("alpha", "u", T0 + 40_000, &fail(429), 0);
        assert_eq!(p.keys["alpha"].cooldown_until, T0 + 50_000);
    }

    #[test]
    fn a_quota_refusal_cools_down_for_an_hour() {
        let mut p = pool(1_000_000);
        p.record(
            "alpha",
            "u",
            T0,
            &Outcome::Failed {
                status: 429,
                retry_after_secs: Some(5),
                quota: true,
            },
            0,
        );
        assert_eq!(p.keys["alpha"].cooldown_until, T0 + HOUR_MS);
    }

    #[test]
    fn server_errors_back_off_exponentially() {
        let mut p = pool(1_000_000);
        p.record("alpha", "u", T0, &fail(503), 0);
        assert_eq!(p.keys["alpha"].cooldown_until, T0 + 5_000);
        p.record("alpha", "u", T0 + 5_000, &fail(500), 0);
        assert_eq!(p.keys["alpha"].cooldown_until, T0 + 15_000);
        p.record("alpha", "u", T0 + 15_000, &Outcome::Ok { tokens: 10 }, 10);
        assert_eq!(p.keys["alpha"].server_errors, 0);
    }

    #[test]
    fn a_refused_key_is_disabled_for_good() {
        let mut p = pool(1_000_000);
        p.record("alpha", "u", T0, &fail(401), 0);
        assert!(p.keys["alpha"].disabled);
        assert_eq!(p.select(&both(), T0 + 30 * DAY_MS, 1).unwrap(), "beta");
        let mut q = pool(1_000_000);
        q.record("beta", "u", T0, &fail(403), 0);
        assert!(q.keys["beta"].disabled);
    }

    #[test]
    fn when_a_provider_is_cooling_down_requests_go_to_the_next() {
        let mut p = pool(1_000_000);
        p.record("alpha", "u", T0, &fail(503), 0);
        assert_eq!(p.select(&both(), T0 + 1, 10).unwrap(), "beta");
        p.record("beta", "u", T0 + 1, &fail(503), 0);
        assert_eq!(
            p.select(&both(), T0 + 2, 10),
            Err(PoolRefusal::NoKeyAvailable)
        );
        // A model only alpha serves has no fallback.
        p.keys.get_mut("beta").unwrap().cooldown_until = 0;
        assert_eq!(
            p.select(&["alpha".into()], T0 + 2, 10),
            Err(PoolRefusal::NoKeyAvailable)
        );
    }

    #[test]
    fn the_four_windows_each_block() {
        let mut p = pool(1_000_000);
        // tpm 6,000: a 6,001-token request does not fit alpha.
        assert_eq!(p.select(&both(), T0, 6_001).unwrap(), "beta");
        // rpm 30: thirty requests this minute fill it...
        for i in 0..30 {
            p.record("alpha", "u", T0 + i, &Outcome::Ok { tokens: 1 }, 1);
        }
        assert_eq!(p.select(&both(), T0 + 100, 1).unwrap(), "beta");
        // ...and the minute slides by.
        assert_eq!(p.select(&both(), T0 + MINUTE_MS + 100, 1).unwrap(), "alpha");
    }

    #[test]
    fn fair_share_holds_back_a_heavy_user_when_the_pool_is_saturated() {
        let mut p = pool(100_000);
        // Not saturated: one user may draw freely past what would be a share.
        for i in 0..8 {
            p.admit("heavy", T0 + i, 10_000).unwrap();
            p.record("alpha", "heavy", T0 + i, &Outcome::Ok { tokens: 1 }, 10_000);
        }
        // A light user arrives; the pool is now saturated (80% of 100k).
        p.record(
            "alpha",
            "light",
            T0 + 100,
            &Outcome::Ok { tokens: 1 },
            1_000,
        );
        assert!(p.saturated());
        // Share = 100k / 2 active users = 50k: heavy (80k) waits...
        assert!(matches!(
            p.admit("heavy", T0 + 200, 1_000),
            Err(PoolRefusal::OverFairShare {
                share: 50_000,
                used: 80_000
            })
        ));
        // ...while light, under its share, goes ahead.
        assert!(p.admit("light", T0 + 200, 5_000).is_ok());
    }

    #[test]
    fn an_under_share_user_borrows_from_idle_users() {
        let mut p = pool(100_000);
        p.record("alpha", "a", T0, &Outcome::Ok { tokens: 1 }, 40_000);
        p.record("alpha", "b", T0, &Outcome::Ok { tokens: 1 }, 41_000);
        p.record("alpha", "idle", T0, &Outcome::Ok { tokens: 1 }, 100);
        assert!(p.saturated());
        // With all three active, the share is 33,333: "a" may not go on.
        assert!(p.admit("a", T0 + 1000, 1000).is_err());
        // Twenty minutes later only "a" is active: idle shares are its to use.
        assert!(p.admit("a", T0 + 20 * MINUTE_MS, 1000).is_ok());
    }

    #[test]
    fn the_pool_cap_holds_whatever_the_users_own_caps_say() {
        let mut p = pool(10_000);
        p.record("alpha", "x", T0, &Outcome::Ok { tokens: 1 }, 9_500);
        // A brand-new user, far under any personal cap, is still refused.
        assert_eq!(
            p.admit("new", T0 + 1, 1_000),
            Err(PoolRefusal::PoolCapReached)
        );
        // The pool recovers as the day's spend ages out.
        assert!(p.admit("new", T0 + DAY_MS, 1_000).is_ok());
    }

    #[test]
    fn limits_are_learned_from_rate_limit_headers() {
        let mut p = pool(1_000);
        p.learn(
            "beta",
            &[
                ("X-RateLimit-Limit-Requests".into(), "14400".into()),
                ("x-ratelimit-limit-tokens".into(), "18000".into()),
            ],
        );
        assert_eq!(p.keys["beta"].limits.rpd, Some(14_400));
        assert_eq!(p.keys["beta"].limits.tpm, Some(18_000));
    }
}
