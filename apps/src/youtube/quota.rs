//! YouTube Data API quota accounting.
//!
//! A Google Cloud project gets 10,000 units per day by default; the
//! counter resets at midnight Pacific Time. Costs per call:
//!
//! | Call                                         | Units |
//! |----------------------------------------------|-------|
//! | `search.list`                                | 100   |
//! | `videos.list` (trending, details, liked)     | 1     |
//! | `channels.list`, `playlists.list`            | 1     |
//! | `playlistItems.list`, `subscriptions.list`   | 1     |
//!
//! So a day allows about 100 searches, or roughly 400 refreshes of the
//! subscriptions feed (about 23 units each). The app counts what it
//! spends, refuses calls that would exceed the budget, and stops for the
//! rest of the day when the API reports `quotaExceeded` (the project may be
//! shared with other clients). OAuth token calls are free.

use alloc::format;
use alloc::string::String;
use p4os_sdk::DateTime;

pub const DAILY_LIMIT: u32 = 10_000;
pub const SEARCH_COST: u32 = 100;
pub const LIST_COST: u32 = 1;

/// A calendar day (year, month, day).
pub type Day = (u16, u8, u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaError {
    /// The call would go past the daily budget.
    Budget { used: u32, cost: u32 },
    /// The API said the project's quota is used up.
    Exhausted,
}

impl QuotaError {
    pub fn message(&self) -> String {
        match self {
            QuotaError::Budget { used, cost } => format!(
                "Daily YouTube quota almost used up ({used} of {DAILY_LIMIT} units; this needs {cost}). It resets at midnight Pacific Time."
            ),
            QuotaError::Exhausted => {
                "YouTube reports the daily quota is used up. It resets at midnight Pacific Time.".into()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quota {
    day: Option<Day>,
    used: u32,
    limit: u32,
    exhausted: bool,
}

impl Default for Quota {
    fn default() -> Self {
        Self::new(DAILY_LIMIT)
    }
}

fn is_leap(y: u16) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The quota day for a UTC time. Pacific Time is taken as UTC-8 all year;
/// during daylight saving the reset is therefore counted an hour late,
/// which only makes the budget slightly more conservative.
pub fn pacific_day(utc: DateTime) -> Day {
    if utc.hour >= 8 {
        return (utc.year, utc.month, utc.day);
    }
    if utc.day > 1 {
        return (utc.year, utc.month, utc.day - 1);
    }
    if utc.month > 1 {
        return (utc.year, utc.month - 1, days_in_month(utc.year, utc.month - 1));
    }
    (utc.year - 1, 12, 31)
}

impl Quota {
    pub fn new(limit: u32) -> Self {
        Self { day: None, used: 0, limit, exhausted: false }
    }

    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Start a new count when the Pacific day changed. Without a clock the
    /// count simply continues.
    fn roll(&mut self, now: Option<DateTime>) {
        let Some(today) = now.map(pacific_day) else { return };
        if self.day != Some(today) {
            if self.day.is_some() {
                self.used = 0;
                self.exhausted = false;
            }
            self.day = Some(today);
        }
    }

    /// Whether a call of `cost` units may be made, without spending.
    pub fn check(&mut self, cost: u32, now: Option<DateTime>) -> Result<(), QuotaError> {
        self.roll(now);
        if self.exhausted {
            return Err(QuotaError::Exhausted);
        }
        if self.used + cost > self.limit {
            return Err(QuotaError::Budget { used: self.used, cost });
        }
        Ok(())
    }

    /// Spend `cost` units if the budget allows it.
    pub fn charge(&mut self, cost: u32, now: Option<DateTime>) -> Result<(), QuotaError> {
        self.check(cost, now)?;
        self.used += cost;
        Ok(())
    }

    /// The API answered `quotaExceeded`.
    pub fn mark_exhausted(&mut self, now: Option<DateTime>) {
        self.roll(now);
        self.exhausted = true;
    }

    /// "YYYY-MM-DD:units" for the settings store; `None` before the first
    /// call of a day with a known date.
    pub fn to_config(&self) -> Option<String> {
        let (y, m, d) = self.day?;
        Some(format!("{y:04}-{m:02}-{d:02}:{}", self.used))
    }

    /// Restore a saved count; a malformed value starts from zero.
    pub fn from_config(value: &str, limit: u32) -> Self {
        let mut q = Self::new(limit);
        let parsed = (|| {
            let (date, used) = value.split_once(':')?;
            let mut parts = date.split('-');
            let y = parts.next()?.parse().ok()?;
            let m = parts.next()?.parse().ok()?;
            let d = parts.next()?.parse().ok()?;
            Some(((y, m, d), used.parse().ok()?))
        })();
        if let Some((day, used)) = parsed {
            q.day = Some(day);
            q.used = used;
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: u16, month: u8, day: u8, hour: u8) -> Option<DateTime> {
        Some(DateTime { year, month, day, hour, minute: 0, second: 0 })
    }

    #[test]
    fn pacific_day_boundaries() {
        assert_eq!(pacific_day(at(2026, 10, 28, 8).unwrap()), (2026, 10, 28));
        assert_eq!(pacific_day(at(2026, 10, 28, 7).unwrap()), (2026, 10, 27));
        assert_eq!(pacific_day(at(2026, 3, 1, 0).unwrap()), (2026, 2, 28));
        assert_eq!(pacific_day(at(2024, 3, 1, 0).unwrap()), (2024, 2, 29));
        assert_eq!(pacific_day(at(2027, 1, 1, 3).unwrap()), (2026, 12, 31));
    }

    #[test]
    fn charges_and_refuses_over_budget() {
        let mut q = Quota::new(250);
        let now = at(2026, 10, 28, 12);
        assert!(q.charge(SEARCH_COST, now).is_ok());
        assert!(q.charge(SEARCH_COST, now).is_ok());
        assert_eq!(q.used(), 200);
        assert_eq!(q.charge(SEARCH_COST, now), Err(QuotaError::Budget { used: 200, cost: 100 }));
        assert_eq!(q.used(), 200, "a refused call costs nothing");
        for _ in 0..50 {
            q.charge(LIST_COST, now).unwrap();
        }
        assert!(q.charge(LIST_COST, now).is_err());
    }

    #[test]
    fn resets_on_a_new_pacific_day() {
        let mut q = Quota::new(DAILY_LIMIT);
        q.charge(9_950, at(2026, 10, 28, 12)).unwrap();
        assert!(q.charge(SEARCH_COST, at(2026, 10, 29, 7)).is_err(), "still the 28th in California");
        assert!(q.charge(SEARCH_COST, at(2026, 10, 29, 8)).is_ok());
        assert_eq!(q.used(), 100);
    }

    #[test]
    fn exhausted_until_the_next_day() {
        let mut q = Quota::default();
        q.mark_exhausted(at(2026, 10, 28, 12));
        assert_eq!(q.charge(LIST_COST, at(2026, 10, 28, 23)), Err(QuotaError::Exhausted));
        assert!(q.charge(LIST_COST, at(2026, 10, 29, 9)).is_ok());
    }

    #[test]
    fn unknown_clock_keeps_counting() {
        let mut q = Quota::new(150);
        q.charge(SEARCH_COST, None).unwrap();
        assert!(q.charge(SEARCH_COST, None).is_err());
        assert_eq!(q.to_config(), None);
    }

    #[test]
    fn persists_round_trip() {
        let mut q = Quota::default();
        q.charge(123, at(2026, 1, 5, 20)).unwrap();
        let saved = q.to_config().unwrap();
        assert_eq!(saved, "2026-01-05:123");
        let mut back = Quota::from_config(&saved, DAILY_LIMIT);
        assert_eq!(back.used(), 123);
        back.charge(1, at(2026, 1, 5, 21)).unwrap();
        assert_eq!(back.used(), 124, "same day continues");
        back.charge(1, at(2026, 1, 6, 21)).unwrap();
        assert_eq!(back.used(), 1, "a later day starts over");
        assert_eq!(Quota::from_config("garbage", DAILY_LIMIT).used(), 0);
        assert_eq!(Quota::from_config("2026-01-05:x", DAILY_LIMIT).used(), 0);
    }
}
