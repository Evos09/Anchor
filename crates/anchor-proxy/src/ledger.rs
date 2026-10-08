//! Reservations (Update #1 §4.2): the proxy decides before it spends,
//! and settles on the provider's own count afterwards.
//!
//! 1. Pre-flight: estimate the request (input, plus the output it may
//!    produce) and refuse if the user's remaining budget cannot cover the
//!    input.
//! 2. Reserve: hold the estimate against the budget before forwarding, so
//!    concurrent requests cannot each fit and together overrun.
//! 3. Settle: replace the hold with the reported usage. A hold larger
//!    than the use is released; use beyond it is charged in full (past the
//!    window's budget it is owed by the next window, [`crate::caps`]).
//!
//! The wrap-up allowance: a request admitted while budget remains may
//! always produce at least `wrap_up_min_tokens` of output, so the turn in
//! flight can finish instead of stopping mid-edit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::caps::{Cap, CapConfig, CapStatus, UserCaps};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerConfig {
    /// Most output tokens one request may ask for.
    #[serde(default = "d_ceiling")]
    pub max_output_ceiling: u64,
    /// Output always allowed to a request admitted while budget remains.
    #[serde(default = "d_wrap")]
    pub wrap_up_min_tokens: u64,
    /// The stream is cut when output passes the reservation by this
    /// fraction (a provider ignoring `max_tokens`, or a bad estimate).
    #[serde(default = "d_margin")]
    pub overrun_margin: f64,
}

fn d_ceiling() -> u64 {
    8192
}
fn d_wrap() -> u64 {
    2048
}
fn d_margin() -> f64 {
    0.2
}

impl Default for LedgerConfig {
    fn default() -> Self {
        LedgerConfig {
            max_output_ceiling: d_ceiling(),
            wrap_up_min_tokens: d_wrap(),
            overrun_margin: d_margin(),
        }
    }
}

/// Why a request is refused before it is forwarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub cap: Cap,
    /// When the blocking cap frees up (ms, UTC), if known.
    pub resets_at_ms: Option<u64>,
    pub needed: u64,
    pub remaining: u64,
}

/// Budget held for one request in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct Reservation {
    pub id: u64,
    pub user: String,
    /// Weighted tokens held.
    pub held: u64,
    /// The output tokens the provider is asked to stop at.
    pub max_output_tokens: u64,
    /// Weighted output the stream may reach before it is cut.
    pub output_cut_at: u64,
    /// Admitted on the wrap-up allowance (the hold passes the budget).
    pub wrap_up: bool,
    pub weight: f64,
}

#[derive(Debug, Default)]
pub struct Ledger {
    pub caps: CapConfig,
    pub cfg: LedgerConfig,
    users: BTreeMap<String, UserCaps>,
    holds: BTreeMap<u64, (String, u64)>,
    next_id: u64,
}

fn weighted(tokens: u64, weight: f64) -> u64 {
    (tokens as f64 * weight).ceil() as u64
}

impl Ledger {
    pub fn new(caps: CapConfig, cfg: LedgerConfig) -> Ledger {
        Ledger {
            caps,
            cfg,
            ..Ledger::default()
        }
    }

    pub fn add_user(&mut self, user: &str, state: UserCaps) {
        self.users.insert(user.to_string(), state);
    }

    pub fn user(&self, user: &str) -> Option<&UserCaps> {
        self.users.get(user)
    }

    fn held_by(&self, user: &str) -> u64 {
        self.holds
            .values()
            .filter(|(u, _)| u == user)
            .map(|(_, h)| h)
            .sum()
    }

    /// The user's caps, with what is held for requests in flight counted
    /// as used.
    pub fn status(&self, user: &str, now: u64) -> Option<CapStatus> {
        let mut s = self.users.get(user)?.status(&self.caps, now);
        let held = self.held_by(user);
        for m in [&mut s.five_hour, &mut s.weekly, &mut s.monthly] {
            m.used += held;
        }
        if s.blocked_by.is_none() {
            s.blocked_by = if s.monthly.remaining() == 0 {
                Some(Cap::Monthly)
            } else if s.weekly.remaining() == 0 {
                Some(Cap::Weekly)
            } else if s.five_hour.remaining() == 0 {
                Some(Cap::FiveHour)
            } else {
                None
            };
        }
        Some(s)
    }

    /// Pre-flight and reserve. `input_tokens` is the estimate,
    /// `asked_output` the client's own `max_tokens`, if any.
    pub fn reserve(
        &mut self,
        user: &str,
        now: u64,
        input_tokens: u64,
        asked_output: Option<u64>,
        weight: f64,
    ) -> Result<Reservation, Refusal> {
        let s = self.status(user, now).ok_or(Refusal {
            cap: Cap::FiveHour,
            resets_at_ms: None,
            needed: 0,
            remaining: 0,
        })?;
        let remaining = s.remaining();
        let need_in = weighted(input_tokens, weight);
        let tightest = [
            (Cap::Monthly, &s.monthly),
            (Cap::Weekly, &s.weekly),
            (Cap::FiveHour, &s.five_hour),
        ]
        .into_iter()
        .min_by_key(|(_, m)| m.remaining())
        .map(|(c, m)| (c, m.resets_at_ms));
        let (cap, resets_at_ms) = tightest.unwrap_or((Cap::FiveHour, None));
        if remaining == 0 || need_in >= remaining {
            return Err(Refusal {
                cap: s.blocked_by.unwrap_or(cap),
                resets_at_ms,
                needed: need_in,
                remaining,
            });
        }
        // Output the budget still covers, at least the wrap-up allowance,
        // at most the ceiling (and the client's own limit).
        let affordable = ((remaining - need_in) as f64 / weight.max(f64::MIN_POSITIVE)) as u64;
        let mut max_out = affordable
            .max(self.cfg.wrap_up_min_tokens)
            .min(self.cfg.max_output_ceiling);
        if let Some(a) = asked_output {
            max_out = max_out.min(a.max(1));
        }
        let held = need_in + weighted(max_out, weight);
        self.next_id += 1;
        let id = self.next_id;
        self.holds.insert(id, (user.to_string(), held));
        let out_w = weighted(max_out, weight);
        Ok(Reservation {
            id,
            user: user.to_string(),
            held,
            max_output_tokens: max_out,
            output_cut_at: out_w + (out_w as f64 * self.cfg.overrun_margin).ceil() as u64,
            wrap_up: held > remaining,
            weight,
        })
    }

    /// Settle a reservation on the provider's count (or the proxy's own,
    /// when the provider reported none). Returns the weighted charge.
    pub fn settle(
        &mut self,
        r: &Reservation,
        now: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) -> u64 {
        self.holds.remove(&r.id);
        let charge = weighted(input_tokens + output_tokens, r.weight);
        if let Some(u) = self.users.get_mut(&r.user) {
            u.charge(&self.caps, now, charge);
        }
        charge
    }

    /// Release a reservation unused (the request never reached a provider).
    pub fn release(&mut self, r: &Reservation) {
        self.holds.remove(&r.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_791_417_600_000;

    fn ledger() -> Ledger {
        let mut l = Ledger::new(CapConfig::default(), LedgerConfig::default());
        l.add_user("u", UserCaps::new(T0));
        l
    }

    #[test]
    fn pre_flight_refuses_a_request_beyond_the_remaining_budget() {
        let mut l = ledger();
        // 50,000 in the window: a 60,000-token prompt cannot start.
        let r = l.reserve("u", T0, 60_000, None, 1.0).unwrap_err();
        assert_eq!(r.cap, Cap::FiveHour);
        assert_eq!(r.remaining, 50_000);
        // Nothing was held for the refused request.
        assert_eq!(l.status("u", T0).unwrap().five_hour.used, 0);
    }

    #[test]
    fn reservations_stop_concurrent_requests_from_overrunning() {
        let mut l = ledger();
        // Each fits alone; together they would not.
        let a = l.reserve("u", T0, 20_000, None, 1.0).unwrap();
        assert_eq!(a.max_output_tokens, 8192);
        let b = l.reserve("u", T0, 15_000, None, 1.0).unwrap();
        let c = l.reserve("u", T0, 5_000, None, 1.0);
        assert!(c.is_err(), "{c:?}");
        assert!(a.held + b.held >= 40_000);
        // Settling the first on a small real count frees its hold.
        l.settle(&a, T0 + 1000, 20_000, 300);
        assert!(l.reserve("u", T0 + 2000, 5_000, None, 1.0).is_ok());
    }

    #[test]
    fn settling_replaces_the_hold_with_the_reported_usage() {
        let mut l = ledger();
        let r = l.reserve("u", T0, 1000, Some(4000), 1.0).unwrap();
        assert_eq!(r.max_output_tokens, 4000);
        assert_eq!(l.status("u", T0).unwrap().five_hour.used, 5000);
        // Over-reserved: the unused 3,500 come back.
        assert_eq!(l.settle(&r, T0 + 10, 1100, 400), 1500);
        assert_eq!(l.status("u", T0 + 10).unwrap().five_hour.used, 1500);
    }

    #[test]
    fn under_reservation_is_charged_against_the_next_requests() {
        let mut l = ledger();
        let r = l.reserve("u", T0, 1000, Some(100), 1.0).unwrap();
        // The estimate was low: the provider counted 9,000 in, 100 out.
        l.settle(&r, T0 + 10, 9000, 100);
        let s = l.status("u", T0 + 10).unwrap();
        assert_eq!(s.five_hour.used, 9100);
        assert_eq!(s.remaining(), 40_900);
    }

    #[test]
    fn weights_scale_what_a_request_costs() {
        let mut l = ledger();
        let r = l.reserve("u", T0, 1000, Some(1000), 2.5).unwrap();
        assert_eq!(r.held, 5000);
        assert_eq!(l.settle(&r, T0, 1000, 200), 3000);
    }

    #[test]
    fn the_wrap_up_allowance_finishes_the_turn_and_charges_the_next_window() {
        let mut l = ledger();
        let r0 = l.reserve("u", T0, 1000, Some(1), 1.0).unwrap();
        l.settle(&r0, T0, 49_500, 0);
        // 500 left: the next step is still admitted, with room for the
        // wrap-up allowance of output rather than 499 tokens.
        let r = l.reserve("u", T0 + 1000, 400, None, 1.0).unwrap();
        assert!(r.wrap_up);
        assert_eq!(r.max_output_tokens, 2048);
        l.settle(&r, T0 + 2000, 400, 1600);
        let s = l.status("u", T0 + 2000).unwrap();
        assert_eq!(s.blocked_by, Some(Cap::FiveHour));
        assert_eq!(s.next_window_debt, 1500);
        // A new request is refused until the window frees up...
        let refusal = l.reserve("u", T0 + 3000, 10, None, 1.0).unwrap_err();
        assert_eq!(refusal.resets_at_ms, Some(T0 + 5 * crate::caps::HOUR_MS));
        // ...and the next window starts owing the overrun.
        let next = T0 + 5 * crate::caps::HOUR_MS;
        assert_eq!(l.status("u", next).unwrap().five_hour.used, 1500);
    }

    #[test]
    fn the_stream_cut_point_is_the_reservation_plus_the_margin() {
        let mut l = ledger();
        let r = l.reserve("u", T0, 100, Some(1000), 1.0).unwrap();
        assert_eq!(r.output_cut_at, 1200);
    }
}
