//! Wrong-passcode rate limiting.
//!
//! Every fifth consecutive failure starts a lockout: 30 s after 5, then
//! doubling for each further 5 (60 s after 10, 120 s after 15, ...) up to
//! 15 minutes. The failure count and the pending lockout are persisted, so
//! rebooting neither resets the count nor ends a lockout early: the end is
//! stored as wall-clock time when the clock is known, otherwise the full
//! lockout starts again after the reboot.

use super::Env;
use alloc::format;
use p4os_sdk::config::{LOCK_FAILS, LOCK_UNTIL};

/// Failures allowed before the first lockout (and between later ones).
pub const ATTEMPTS_PER_LOCKOUT: u32 = 5;
const FIRST_LOCKOUT_MS: u64 = 30_000;
const MAX_LOCKOUT_MS: u64 = 15 * 60_000;
/// Stored in `lock.until` when the wall clock is unknown.
const UNKNOWN_END: &str = "?";

/// Lockout that starts when the failure count reaches `fails` (0: none).
pub fn lockout_ms(fails: u32) -> u64 {
    if fails < ATTEMPTS_PER_LOCKOUT || fails % ATTEMPTS_PER_LOCKOUT != 0 {
        return 0;
    }
    let doublings = fails / ATTEMPTS_PER_LOCKOUT - 1;
    if doublings >= 16 {
        return MAX_LOCKOUT_MS;
    }
    (FIRST_LOCKOUT_MS << doublings).min(MAX_LOCKOUT_MS)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Throttle {
    fails: u32,
    /// End of the current lockout in uptime milliseconds.
    until_ms: Option<u64>,
}

impl Throttle {
    /// Restore the persisted state (at boot or whenever the lock engages).
    pub fn load<E: Env + ?Sized>(env: &mut E) -> Self {
        let fails = env.get(LOCK_FAILS).and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut t = Self { fails, until_ms: None };
        let Some(end) = env.get(LOCK_UNTIL).filter(|v| !v.is_empty()) else { return t };
        // Never longer than the lockout this count earns, so a clock set
        // backwards cannot extend it without bound.
        let full = lockout_ms(fails).max(FIRST_LOCKOUT_MS);
        let remaining = match (end.parse::<i64>().ok(), env.unix_now()) {
            (Some(end), Some(now)) => (end.saturating_sub(now).max(0) as u64).saturating_mul(1000).min(full),
            _ => full,
        };
        if remaining > 0 {
            t.until_ms = Some(env.uptime_ms() + remaining);
        } else {
            env.set(LOCK_UNTIL, "");
        }
        t
    }

    pub fn fails(&self) -> u32 {
        self.fails
    }

    /// Milliseconds until another attempt is allowed (0: allowed now).
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.until_ms.map_or(0, |u| u.saturating_sub(now_ms))
    }

    /// Forget a lockout that has run out, persistently.
    pub fn refresh<E: Env + ?Sized>(&mut self, env: &mut E) {
        if self.until_ms.is_some() && self.remaining_ms(env.uptime_ms()) == 0 {
            self.until_ms = None;
            env.set(LOCK_UNTIL, "");
        }
    }

    /// Count a wrong passcode; may start a lockout.
    pub fn record_failure<E: Env + ?Sized>(&mut self, env: &mut E) {
        self.fails = self.fails.saturating_add(1);
        env.set(LOCK_FAILS, &format!("{}", self.fails));
        let ms = lockout_ms(self.fails);
        if ms > 0 {
            self.until_ms = Some(env.uptime_ms() + ms);
            let end = match env.unix_now() {
                Some(now) => format!("{}", now + (ms / 1000) as i64),
                None => UNKNOWN_END.into(),
            };
            env.set(LOCK_UNTIL, &end);
        }
    }

    /// The right passcode: clear the count and any lockout.
    pub fn record_success<E: Env + ?Sized>(&mut self, env: &mut E) {
        if self.fails != 0 {
            self.fails = 0;
            env.set(LOCK_FAILS, "0");
        }
        if self.until_ms.take().is_some() || env.get(LOCK_UNTIL).is_some_and(|v| !v.is_empty()) {
            env.set(LOCK_UNTIL, "");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::MockEnv;
    use super::*;

    #[test]
    fn schedule() {
        assert_eq!(lockout_ms(0), 0);
        assert_eq!(lockout_ms(4), 0);
        assert_eq!(lockout_ms(5), 30_000);
        assert_eq!(lockout_ms(6), 0);
        assert_eq!(lockout_ms(10), 60_000);
        assert_eq!(lockout_ms(15), 120_000);
        assert_eq!(lockout_ms(20), 240_000);
        assert_eq!(lockout_ms(25), 480_000);
        assert_eq!(lockout_ms(30), MAX_LOCKOUT_MS);
        assert_eq!(lockout_ms(500), MAX_LOCKOUT_MS);
        assert_eq!(lockout_ms(u32::MAX - u32::MAX % 5), MAX_LOCKOUT_MS);
    }

    #[test]
    fn five_failures_lock_out_then_expire() {
        let mut env = MockEnv::default();
        let mut t = Throttle::load(&mut env);
        for _ in 0..4 {
            t.record_failure(&mut env);
            assert_eq!(t.remaining_ms(env.uptime), 0);
        }
        t.record_failure(&mut env);
        assert_eq!(t.remaining_ms(env.uptime), 30_000);
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("5"));
        assert_eq!(env.get(LOCK_UNTIL).as_deref(), Some("?"));
        env.uptime += 30_000;
        t.refresh(&mut env);
        assert_eq!(t.remaining_ms(env.uptime), 0);
        assert_eq!(env.get(LOCK_UNTIL).as_deref(), Some(""));
        // Five more double the lockout.
        for _ in 0..5 {
            t.record_failure(&mut env);
        }
        assert_eq!(t.remaining_ms(env.uptime), 60_000);
    }

    #[test]
    fn success_resets_everything() {
        let mut env = MockEnv::default();
        let mut t = Throttle::load(&mut env);
        for _ in 0..5 {
            t.record_failure(&mut env);
        }
        t.record_success(&mut env);
        assert_eq!(t, Throttle::default());
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("0"));
        assert_eq!(env.get(LOCK_UNTIL).as_deref(), Some(""));
        assert_eq!(Throttle::load(&mut env), Throttle::default());
    }

    #[test]
    fn reboot_keeps_count_and_restarts_lockout_without_clock() {
        let mut env = MockEnv::default();
        let mut t = Throttle::load(&mut env);
        for _ in 0..5 {
            t.record_failure(&mut env);
        }
        env.uptime += 10_000;
        // Reboot: uptime restarts, the config survives.
        env.uptime = 0;
        let mut t = Throttle::load(&mut env);
        assert_eq!(t.fails(), 5);
        assert_eq!(t.remaining_ms(0), 30_000);
        // The count keeps going towards the next lockout.
        env.uptime = 30_000;
        t.refresh(&mut env);
        for _ in 0..4 {
            t.record_failure(&mut env);
        }
        env.uptime = 0;
        let t = Throttle::load(&mut env);
        assert_eq!(t.fails(), 9);
        assert_eq!(t.remaining_ms(0), 0);
    }

    #[test]
    fn reboot_with_wall_clock_keeps_the_original_end() {
        let mut env = MockEnv { unix: Some(1_000_000), ..Default::default() };
        let mut t = Throttle::load(&mut env);
        for _ in 0..10 {
            t.record_failure(&mut env);
            if t.remaining_ms(env.uptime) > 0 && t.fails() == 5 {
                env.uptime += 30_000;
                env.unix = env.unix.map(|u| u + 30);
                t.refresh(&mut env);
            }
        }
        assert_eq!(env.get(LOCK_UNTIL), Some(format!("{}", 1_000_030 + 60)));
        // Reboot 20 s later: 40 s of the 60 s lockout are left.
        env.uptime = 0;
        env.unix = Some(1_000_030 + 20);
        let t = Throttle::load(&mut env);
        assert_eq!(t.remaining_ms(0), 40_000);
        // Rebooting after it ended clears it.
        env.unix = Some(1_000_030 + 61);
        let t = Throttle::load(&mut env);
        assert_eq!(t.remaining_ms(0), 0);
        assert_eq!(env.get(LOCK_UNTIL).as_deref(), Some(""));
    }

    #[test]
    fn clock_set_back_cannot_extend_the_lockout() {
        let mut env = MockEnv { unix: Some(5_000), ..Default::default() };
        env.set(LOCK_FAILS, "5");
        env.set(LOCK_UNTIL, "999999999");
        assert_eq!(Throttle::load(&mut env).remaining_ms(0), 30_000);
    }

    #[test]
    fn corrupt_values_are_handled() {
        let mut env = MockEnv::default();
        env.set(LOCK_FAILS, "lots");
        env.set(LOCK_UNTIL, "soon");
        let t = Throttle::load(&mut env);
        assert_eq!(t.fails(), 0);
        // A pending lockout of unknown end is honoured, never skipped.
        assert_eq!(t.remaining_ms(0), FIRST_LOCKOUT_MS);
    }
}
