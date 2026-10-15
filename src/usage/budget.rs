//! Budgets: spend caps per day, week and month, computed from the usage ledger and the price
//! table. Past the cap the answer is a `429` ("budget exhausted"), never a silent downgrade,
//! unless the client names a `budget_fallback` model entry. Only priced usage counts: a request
//! whose model has no price has unknown cost and adds nothing to a budget.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::schema::Budget;

/// A budget period, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Period {
    /// A calendar day.
    Day,
    /// A week starting Monday.
    Week,
    /// A calendar month.
    Month,
}

impl Period {
    /// Every period.
    pub const ALL: [Period; 3] = [Self::Day, Self::Week, Self::Month];

    /// A name for messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "daily",
            Self::Week => "weekly",
            Self::Month => "monthly",
        }
    }
}

pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The first second of the period containing `unix`.
#[must_use]
pub fn period_start(p: Period, unix: u64) -> u64 {
    let days = i64::try_from(unix / 86_400).unwrap_or(i64::MAX);
    let start_day = match p {
        Period::Day => days,
        // 1970-01-01 was a Thursday, so Monday is day (days + 3) % 7 == 0.
        Period::Week => days - (days + 3).rem_euclid(7),
        Period::Month => {
            let (y, m, _) = civil_from_days(days);
            days_from_civil(y, m, 1)
        }
    };
    u64::try_from(start_day).unwrap_or(0) * 86_400
}

/// The first second of the period after the one containing `unix`.
#[must_use]
pub fn period_end(p: Period, unix: u64) -> u64 {
    let days = i64::try_from(unix / 86_400).unwrap_or(i64::MAX);
    let end_day = match p {
        Period::Day => days + 1,
        Period::Week => days - (days + 3).rem_euclid(7) + 7,
        Period::Month => {
            let (y, m, _) = civil_from_days(days);
            if m == 12 {
                days_from_civil(y + 1, 1, 1)
            } else {
                days_from_civil(y, m + 1, 1)
            }
        }
    };
    u64::try_from(end_day).unwrap_or(0) * 86_400
}

/// A budget that has been used up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exhausted {
    /// Which period.
    pub period: Period,
    /// The cap, in dollars.
    pub cap: f64,
    /// What has been spent.
    pub spent: f64,
    /// When the period ends and the budget renews, in Unix seconds.
    pub renews_at: u64,
}

/// Running spend per client per period.
#[derive(Debug, Default)]
pub struct Budgets {
    spent: Mutex<HashMap<(String, Period), (u64, f64)>>,
}

impl Budgets {
    /// No spend yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `usd` spent by `client` at `unix`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn record(&self, client: &str, unix: u64, usd: f64) {
        let mut m = self.spent.lock().expect("budget lock");
        for p in Period::ALL {
            let start = period_start(p, unix);
            let e = m.entry((client.to_owned(), p)).or_insert((start, 0.0));
            if e.0 == start {
                e.1 += usd;
            } else if start > e.0 {
                *e = (start, usd);
            }
            // A record from an earlier period than the one being tracked is history; ignore it.
        }
    }

    /// Rebuilds the totals from a ledger. Records without a cost add nothing.
    #[must_use]
    pub fn rebuild(records: &[super::UsageRecord]) -> Self {
        let b = Self::new();
        let mut sorted: Vec<&super::UsageRecord> =
            records.iter().filter(|r| r.cost_usd.is_some()).collect();
        sorted.sort_by_key(|r| r.unix);
        for r in sorted {
            b.record(&r.client, r.unix, r.cost_usd.unwrap_or(0.0));
        }
        b
    }

    /// What `client` has spent in the period containing `now`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn spent(&self, client: &str, p: Period, now: u64) -> f64 {
        let m = self.spent.lock().expect("budget lock");
        m.get(&(client.to_owned(), p))
            .filter(|(start, _)| *start == period_start(p, now))
            .map_or(0.0, |(_, s)| *s)
    }

    /// The first period whose cap `client` has reached, if any.
    #[must_use]
    pub fn exhausted(&self, client: &str, budget: &Budget, now: u64) -> Option<Exhausted> {
        [
            (Period::Day, budget.usd_per_day),
            (Period::Week, budget.usd_per_week),
            (Period::Month, budget.usd_per_month),
        ]
        .into_iter()
        .find_map(|(period, cap)| {
            let cap = cap?;
            let spent = self.spent(client, period, now);
            (spent >= cap).then(|| Exhausted {
                period,
                cap,
                spent,
                renews_at: period_end(period, now),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    // 2026-10-14 (a Wednesday) 12:34:56 UTC.
    const WED: u64 = 1_791_981_296 - 1_791_981_296 % DAY + 12 * 3600 + 34 * 60 + 56;

    #[test]
    fn periods_start_and_end_on_utc_boundaries() {
        // Find the actual calendar facts rather than trusting a constant.
        let day = period_start(Period::Day, WED);
        assert_eq!(day % DAY, 0);
        assert_eq!(period_end(Period::Day, WED), day + DAY);
        let week = period_start(Period::Week, WED);
        assert_eq!((week / DAY + 3) % 7, 0, "weeks start on a Monday");
        assert!(week <= day && day - week < 7 * DAY);
        assert_eq!(period_end(Period::Week, WED), week + 7 * DAY);
        let month = period_start(Period::Month, WED);
        assert!(month <= day);
        assert!(period_end(Period::Month, WED) > WED);
    }

    #[test]
    fn known_dates_resolve_correctly() {
        // 2026-10-14 00:00:00 UTC.
        let oct14 = (days_from_civil(2026, 10, 14) as u64) * DAY;
        assert_eq!(
            civil_from_days(days_from_civil(2026, 10, 14)),
            (2026, 10, 14)
        );
        assert_eq!(
            period_start(Period::Month, oct14 + 5000),
            (days_from_civil(2026, 10, 1) as u64) * DAY
        );
        assert_eq!(
            period_end(Period::Month, oct14),
            (days_from_civil(2026, 11, 1) as u64) * DAY
        );
        // 2026-10-14 is a Wednesday, so the week began on Monday the 12th.
        assert_eq!(
            period_start(Period::Week, oct14),
            (days_from_civil(2026, 10, 12) as u64) * DAY
        );
        assert_eq!(
            period_end(Period::Week, oct14),
            (days_from_civil(2026, 10, 19) as u64) * DAY
        );
        // December rolls into January.
        let dec = (days_from_civil(2026, 12, 31) as u64) * DAY;
        assert_eq!(
            period_end(Period::Month, dec),
            (days_from_civil(2027, 1, 1) as u64) * DAY
        );
        // The epoch.
        assert_eq!(
            period_start(Period::Week, 0),
            0,
            "the Monday before the epoch clamps to it"
        );
    }

    #[test]
    fn spend_accumulates_per_client_and_period_and_resets_when_the_period_turns() {
        let b = Budgets::new();
        let day1 = (days_from_civil(2026, 10, 14) as u64) * DAY + 100;
        let day2 = day1 + DAY;
        b.record("ci", day1, 1.5);
        b.record("ci", day1 + 10, 0.5);
        b.record("laptop", day1, 9.0);
        assert!((b.spent("ci", Period::Day, day1) - 2.0).abs() < 1e-9);
        assert!((b.spent("laptop", Period::Day, day1) - 9.0).abs() < 1e-9);
        assert_eq!(b.spent("nobody", Period::Day, day1), 0.0);
        // The next day starts a new daily total but the week and month carry on.
        assert_eq!(b.spent("ci", Period::Day, day2), 0.0);
        assert!((b.spent("ci", Period::Week, day2) - 2.0).abs() < 1e-9);
        b.record("ci", day2, 1.0);
        assert!((b.spent("ci", Period::Day, day2) - 1.0).abs() < 1e-9);
        assert!((b.spent("ci", Period::Month, day2) - 3.0).abs() < 1e-9);
        // A late record from before the tracked period is history and changes nothing.
        b.record("ci", day1, 100.0);
        assert!((b.spent("ci", Period::Day, day2) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn exhaustion_names_the_period_cap_spend_and_renewal() {
        let b = Budgets::new();
        let now = (days_from_civil(2026, 10, 14) as u64) * DAY + 3600;
        let budget = Budget {
            usd_per_day: Some(5.0),
            usd_per_week: Some(20.0),
            usd_per_month: None,
        };
        assert_eq!(b.exhausted("ci", &budget, now), None);
        b.record("ci", now, 4.99);
        assert_eq!(b.exhausted("ci", &budget, now), None);
        b.record("ci", now, 0.01);
        let e = b.exhausted("ci", &budget, now).unwrap();
        assert_eq!((e.period, e.cap), (Period::Day, 5.0));
        assert!((e.spent - 5.0).abs() < 1e-9);
        assert_eq!(e.renews_at, period_end(Period::Day, now));
        assert_eq!(
            b.exhausted("ci", &budget, now + DAY),
            None,
            "the daily budget renews at midnight"
        );
        assert_eq!(
            b.exhausted("ci", &Budget::default(), now),
            None,
            "no cap, no exhaustion"
        );
        assert_eq!(Period::Week.name(), "weekly");
    }
}
