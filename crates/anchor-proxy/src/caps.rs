//! The free tier's three nested caps (Update #1 §3, ADR-0031): a 5-hour
//! window, a week counted from the account's creation, and the calendar
//! month in UTC. Hitting any one stops the user. All times are the
//! proxy's own clock, in milliseconds since the Unix epoch (UTC): nothing
//! the client says about time is used.

use serde::{Deserialize, Serialize};

pub const HOUR_MS: u64 = 3_600_000;
pub const DAY_MS: u64 = 24 * HOUR_MS;
pub const WEEK_MS: u64 = 7 * DAY_MS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapConfig {
    /// Length of the rolling window (5 hours).
    #[serde(default = "five_hours")]
    pub window_ms: u64,
    /// Weighted tokens per window.
    #[serde(default = "d_window")]
    pub window_budget: u64,
    /// Weighted tokens per week (a quarter of the month).
    #[serde(default = "d_week")]
    pub weekly_budget: u64,
    /// Weighted tokens per calendar month (UTC).
    #[serde(default = "d_month")]
    pub monthly_budget: u64,
}

fn five_hours() -> u64 {
    5 * HOUR_MS
}
fn d_window() -> u64 {
    50_000
}
fn d_week() -> u64 {
    250_000
}
fn d_month() -> u64 {
    1_000_000
}

impl Default for CapConfig {
    fn default() -> Self {
        CapConfig {
            window_ms: five_hours(),
            window_budget: d_window(),
            weekly_budget: d_week(),
            monthly_budget: d_month(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    FiveHour,
    Weekly,
    Monthly,
}

impl Cap {
    pub fn describe(self) -> &'static str {
        match self {
            Cap::FiveHour => "the 5-hour limit",
            Cap::Weekly => "the weekly limit",
            Cap::Monthly => "the monthly limit",
        }
    }
}

/// One user's usage, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserCaps {
    /// When the account was created: the weekly cycle counts from here.
    pub created_ms: u64,
    /// The open 5-hour window: when it opened, and what it has used.
    pub window_start_ms: Option<u64>,
    pub window_used: u64,
    /// Overrun from a wrap-up turn, charged to the next window when it opens.
    pub next_window_debt: u64,
    /// Which week (counted from creation) `week_used` belongs to.
    pub week_index: u64,
    pub week_used: u64,
    /// Which month (`year * 12 + month0`) `month_used` belongs to.
    pub month_key: u64,
    pub month_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meter {
    pub used: u64,
    pub budget: u64,
    /// When this meter frees up: the window closing, the next weekly or
    /// monthly reset (ms, UTC). `None` for a window not yet opened.
    pub resets_at_ms: Option<u64>,
}

impl Meter {
    pub fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.used)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapStatus {
    pub five_hour: Meter,
    pub weekly: Meter,
    pub monthly: Meter,
    /// The cap that stops the user now, if any (the tightest first).
    pub blocked_by: Option<Cap>,
    /// Overrun from a wrap-up turn, waiting for the next window.
    pub next_window_debt: u64,
}

impl CapStatus {
    /// Weighted tokens the user may still spend across all three caps.
    pub fn remaining(&self) -> u64 {
        self.five_hour
            .remaining()
            .min(self.weekly.remaining())
            .min(self.monthly.remaining())
    }
}

/// Days since the epoch → (year, month 1..=12), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month)
}

/// (year, month) → days since the epoch of its first day.
fn days_from_civil(year: i64, month: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The calendar month (UTC) holding `ms`, as `year * 12 + month0`.
pub fn month_key(ms: u64) -> u64 {
    let (y, m) = civil_from_days((ms / DAY_MS) as i64);
    (y * 12 + i64::from(m) - 1) as u64
}

/// 00:00 UTC on the first day of the month after the one holding `ms`.
pub fn next_month_start_ms(ms: u64) -> u64 {
    let (y, m) = civil_from_days((ms / DAY_MS) as i64);
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    days_from_civil(ny, nm) as u64 * DAY_MS
}

impl UserCaps {
    pub fn new(created_ms: u64) -> UserCaps {
        UserCaps {
            created_ms,
            week_index: 0,
            month_key: month_key(created_ms),
            ..UserCaps::default()
        }
    }

    fn week_of(&self, now: u64) -> u64 {
        now.saturating_sub(self.created_ms) / WEEK_MS
    }

    /// Bring the counters up to `now`: close an expired window, start a
    /// new week or month. Unused budget is not carried over.
    fn roll(&mut self, cfg: &CapConfig, now: u64) {
        if let Some(start) = self.window_start_ms
            && now >= start + cfg.window_ms
        {
            self.window_start_ms = None;
            self.window_used = 0;
        }
        let week = self.week_of(now);
        if week != self.week_index {
            self.week_index = week;
            self.week_used = 0;
        }
        let month = month_key(now);
        if month != self.month_key {
            self.month_key = month;
            self.month_used = 0;
        }
    }

    pub fn status(&self, cfg: &CapConfig, now: u64) -> CapStatus {
        let mut s = self.clone();
        s.roll(cfg, now);
        // A window not yet opened starts with the wrap-up debt.
        let (window_used, window_resets) = match s.window_start_ms {
            Some(start) => (s.window_used, Some(start + cfg.window_ms)),
            None => (s.next_window_debt, None),
        };
        let five_hour = Meter {
            used: window_used,
            budget: cfg.window_budget,
            resets_at_ms: window_resets,
        };
        let weekly = Meter {
            used: s.week_used,
            budget: cfg.weekly_budget,
            resets_at_ms: Some(s.created_ms + (s.week_index + 1) * WEEK_MS),
        };
        let monthly = Meter {
            used: s.month_used,
            budget: cfg.monthly_budget,
            resets_at_ms: Some(next_month_start_ms(now)),
        };
        let blocked_by = if monthly.remaining() == 0 {
            Some(Cap::Monthly)
        } else if weekly.remaining() == 0 {
            Some(Cap::Weekly)
        } else if five_hour.remaining() == 0 {
            Some(Cap::FiveHour)
        } else {
            None
        };
        CapStatus {
            five_hour,
            weekly,
            monthly,
            blocked_by,
            next_window_debt: if s.window_start_ms.is_some() {
                s.next_window_debt
            } else {
                0
            },
        }
    }

    /// Charge `tokens` used at `now`. The first charge after a window
    /// closed opens the next one (with any wrap-up debt in it). What goes
    /// past the window's budget, which only a wrap-up turn can do, is owed
    /// by the next window.
    pub fn charge(&mut self, cfg: &CapConfig, now: u64, tokens: u64) {
        self.roll(cfg, now);
        if self.window_start_ms.is_none() {
            self.window_start_ms = Some(now);
            self.window_used = std::mem::take(&mut self.next_window_debt);
        }
        let room = cfg.window_budget.saturating_sub(self.window_used);
        let over = tokens.saturating_sub(room);
        self.window_used += tokens - over;
        self.next_window_debt += over;
        self.week_used += tokens;
        self.month_used += tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-08T00:00:00Z.
    const T0: u64 = 1_791_417_600_000;

    fn cfg() -> CapConfig {
        CapConfig::default()
    }

    #[test]
    fn month_arithmetic_is_utc() {
        assert_eq!(next_month_start_ms(T0), 1_793_491_200_000); // 2026-11-01T00:00Z
        assert_eq!(month_key(T0), 2026 * 12 + 9);
        // 2026-12-31T23:59:59.999Z rolls to 2027-01-01.
        let nye = 1_798_761_599_999;
        assert_eq!(next_month_start_ms(nye), 1_798_761_600_000);
        // A leap February.
        assert_eq!(next_month_start_ms(1_709_164_800_000), 1_709_251_200_000); // 2024-02-29 → 03-01
    }

    #[test]
    fn the_window_opens_on_the_first_request_and_closes_five_hours_later() {
        let c = cfg();
        let mut u = UserCaps::new(T0);
        assert_eq!(u.status(&c, T0 + 10).five_hour.resets_at_ms, None);
        let open = T0 + 3 * HOUR_MS;
        u.charge(&c, open, 1000);
        let s = u.status(&c, open);
        assert_eq!(s.five_hour.used, 1000);
        assert_eq!(s.five_hour.resets_at_ms, Some(open + 5 * HOUR_MS));
        // Still the same window a millisecond before it closes...
        assert_eq!(u.status(&c, open + 5 * HOUR_MS - 1).five_hour.used, 1000);
        // ...and closed exactly at five hours.
        assert_eq!(u.status(&c, open + 5 * HOUR_MS).five_hour.used, 0);
        assert_eq!(
            u.status(&c, open + 5 * HOUR_MS).five_hour.resets_at_ms,
            None
        );
    }

    #[test]
    fn usage_ages_out_rather_than_resetting_at_a_fixed_time() {
        let c = cfg();
        let mut u = UserCaps::new(T0);
        // A window opened at 22:30 runs past midnight: no reset at 00:00.
        let open = T0 + 22 * HOUR_MS + 30 * 60_000;
        u.charge(&c, open, c.window_budget);
        assert_eq!(
            u.status(&c, T0 + DAY_MS + 1).blocked_by,
            Some(Cap::FiveHour)
        );
        assert_eq!(u.status(&c, open + 5 * HOUR_MS).blocked_by, None);
        // A user who stopped working does not keep the window open: the
        // next request opens a new one, five hours from then.
        let later = open + 9 * HOUR_MS;
        u.charge(&c, later, 10);
        assert_eq!(
            u.status(&c, later).five_hour.resets_at_ms,
            Some(later + 5 * HOUR_MS)
        );
    }

    #[test]
    fn the_week_resets_seven_days_from_account_creation() {
        let c = cfg();
        // Created on a Thursday afternoon: weeks run Thursday to Thursday.
        let created = T0 + 15 * HOUR_MS;
        let mut u = UserCaps::new(created);
        u.charge(&c, created + HOUR_MS, 100);
        let s = u.status(&c, created + HOUR_MS);
        assert_eq!(s.weekly.used, 100);
        assert_eq!(s.weekly.resets_at_ms, Some(created + WEEK_MS));
        assert_eq!(u.status(&c, created + WEEK_MS - 1).weekly.used, 100);
        assert_eq!(u.status(&c, created + WEEK_MS).weekly.used, 0);
        assert_eq!(
            u.status(&c, created + WEEK_MS).weekly.resets_at_ms,
            Some(created + 2 * WEEK_MS)
        );
    }

    #[test]
    fn unused_weekly_budget_does_not_carry_over() {
        let c = cfg();
        let mut u = UserCaps::new(T0);
        u.charge(&c, T0 + HOUR_MS, 1000);
        // Week two: the full budget, not budget + the 249,000 left unused.
        let wk2 = T0 + WEEK_MS + HOUR_MS;
        assert_eq!(u.status(&c, wk2).weekly.remaining(), c.weekly_budget);
        // Spending through several windows in week two stops at the budget.
        let mut t = wk2;
        while u.status(&c, t).blocked_by != Some(Cap::Weekly) {
            u.charge(&c, t, c.window_budget);
            t += 5 * HOUR_MS;
        }
        assert_eq!(u.status(&c, t).weekly.used, c.weekly_budget);
    }

    #[test]
    fn the_monthly_cap_stops_the_user_on_its_own() {
        let c = CapConfig {
            monthly_budget: 120_000,
            ..cfg()
        };
        let mut u = UserCaps::new(T0);
        // Three windows, two weeks: weekly is far from its budget.
        u.charge(&c, T0, 50_000);
        u.charge(&c, T0 + 6 * HOUR_MS, 50_000);
        u.charge(&c, T0 + WEEK_MS + HOUR_MS, 20_000);
        let s = u.status(&c, T0 + WEEK_MS + 2 * HOUR_MS);
        assert!(s.weekly.remaining() > 0);
        assert_eq!(s.blocked_by, Some(Cap::Monthly));
        // Until 00:00 UTC on the 1st.
        assert_eq!(s.monthly.resets_at_ms, Some(next_month_start_ms(T0)));
        assert_eq!(u.status(&c, next_month_start_ms(T0)).blocked_by, None);
    }

    #[test]
    fn ninety_percent_monthly_and_ten_percent_weekly_is_not_stopped() {
        let c = cfg();
        let mut u = UserCaps::new(T0);
        u.month_used = 900_000;
        u.week_used = 25_000;
        let s = u.status(&c, T0 + HOUR_MS);
        assert_eq!(s.blocked_by, None);
        assert_eq!(s.remaining(), 50_000);
    }

    #[test]
    fn a_full_week_stops_the_user_at_thirty_percent_monthly() {
        let c = cfg();
        let mut u = UserCaps::new(T0);
        u.month_used = 300_000;
        u.week_used = 250_000;
        assert_eq!(u.status(&c, T0 + HOUR_MS).blocked_by, Some(Cap::Weekly));
    }

    #[test]
    fn a_wrap_up_overrun_is_owed_by_the_next_window() {
        let c = cfg();
        let mut u = UserCaps::new(T0);
        u.charge(&c, T0, 49_000);
        // The in-flight turn finishes 3,000 over.
        u.charge(&c, T0 + 60_000, 4_000);
        let s = u.status(&c, T0 + 60_000);
        assert_eq!(s.five_hour.used, c.window_budget);
        assert_eq!(s.blocked_by, Some(Cap::FiveHour));
        assert_eq!(s.next_window_debt, 3_000);
        // The next window opens already charged with it.
        let next = T0 + 5 * HOUR_MS;
        assert_eq!(u.status(&c, next).five_hour.used, 3_000);
        u.charge(&c, next, 100);
        assert_eq!(u.status(&c, next).five_hour.used, 3_100);
        // Week and month counted the real total.
        assert_eq!(u.status(&c, next).weekly.used, 53_100);
    }
}
