//! Lock screen: passcode storage, rate limiting, the lock state machine the
//! desktop drives, and the Settings > Security page.
//!
//! The lock is active only when `lock.enabled` is "1" and a passcode hash is
//! stored. While locked the desktop draws nothing but the lock screen and
//! routes no input to apps; apps keep running in their OS threads.
//!
//! Events the desktop forwards (see `Desktop::lock_now`, `on_screen_off`,
//! `on_wake`): boot, an explicit "lock now", the screen turning off, and
//! the screen waking. Until the display reports screen-off events, the
//! "on screen off" policy falls back to an inactivity timer.

pub mod pin;
mod screen;
pub mod security;
pub mod throttle;

use alloc::string::String;
use p4os_sdk::config::{LOCK_AUTO, LOCK_ENABLED, LOCK_PIN};
use p4os_sdk::gui::font::builtin::Fonts;
use p4os_sdk::gui::ui::{Input, Theme, UiState};
use p4os_sdk::{Canvas, DateTime, System};
use pin::PinEntry;
pub use screen::padlock;
use throttle::Throttle;

/// What the lock logic needs from the OS. Implemented for every `System`;
/// tests use an in-memory mock.
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&mut self, key: &str, value: &str);
    /// Fill `buf` with secure random bytes; false if unavailable.
    fn random(&mut self, buf: &mut [u8]) -> bool;
    fn uptime_ms(&self) -> u64;
    /// Wall-clock time in Unix seconds, when known.
    fn unix_now(&self) -> Option<i64>;
    /// Ask the desktop to lock.
    fn request_lock(&mut self);
}

impl<S: System + ?Sized> Env for S {
    fn get(&self, key: &str) -> Option<String> {
        self.config_get(key)
    }
    fn set(&mut self, key: &str, value: &str) {
        self.config_set(key, value)
    }
    fn random(&mut self, buf: &mut [u8]) -> bool {
        self.fill_random(buf)
    }
    fn uptime_ms(&self) -> u64 {
        System::uptime_ms(self)
    }
    fn unix_now(&self) -> Option<i64> {
        self.now().map(unix_seconds)
    }
    fn request_lock(&mut self) {
        self.lock_now()
    }
}

/// Seconds since 1970-01-01 for a civil date and time.
pub fn unix_seconds(t: DateTime) -> i64 {
    let (y, m, d) = (t.year as i64, t.month.clamp(1, 12) as i64, t.day as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64
}

/// When the screen locks by itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AutoLock {
    #[default]
    ScreenOff,
    OneMinute,
    FiveMinutes,
    Never,
}

/// Stand-in for "screen off" until the display reports it.
pub const SCREEN_OFF_FALLBACK_MS: u64 = 60_000;

impl AutoLock {
    pub const ALL: [AutoLock; 4] = [AutoLock::ScreenOff, AutoLock::OneMinute, AutoLock::FiveMinutes, AutoLock::Never];

    pub fn value(self) -> &'static str {
        match self {
            AutoLock::ScreenOff => "screen-off",
            AutoLock::OneMinute => "1m",
            AutoLock::FiveMinutes => "5m",
            AutoLock::Never => "never",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AutoLock::ScreenOff => "Screen off",
            AutoLock::OneMinute => "1 min",
            AutoLock::FiveMinutes => "5 min",
            AutoLock::Never => "Never",
        }
    }

    /// Unknown or missing values fall back to the default.
    pub fn parse(v: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|a| Some(a.value()) == v).unwrap_or_default()
    }

    pub fn load<E: Env + ?Sized>(env: &E) -> Self {
        Self::parse(env.get(LOCK_AUTO).as_deref())
    }
}

pub fn has_passcode<E: Env + ?Sized>(env: &E) -> bool {
    // A record that does not decode still counts: failing closed keeps the
    // device locked (recover with `lock reset` on the serial console).
    env.get(LOCK_PIN).is_some_and(|v| !v.is_empty())
}

pub fn is_enabled<E: Env + ?Sized>(env: &E) -> bool {
    env.get(LOCK_ENABLED).as_deref() == Some("1")
}

/// The lock engages only when switched on and a passcode exists.
pub fn is_configured<E: Env + ?Sized>(env: &E) -> bool {
    is_enabled(env) && has_passcode(env)
}

/// Progress of a submitted passcode on the lock screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Checking {
    #[default]
    Idle,
    /// Enter was pressed; the next frame shows "Checking...".
    Requested,
    /// That frame is on screen; the following one runs the check.
    Shown,
}

/// Lock state owned by the desktop.
#[derive(Default)]
pub struct Lock {
    locked: bool,
    booted: bool,
    /// Uptime of the last touch or key while unlocked.
    last_activity_ms: u64,
    /// The display has reported screen-off/wake, so the inactivity
    /// fallback for the "screen off" policy is no longer needed.
    screen_events: bool,
    entry: PinEntry,
    throttle: Throttle,
    /// When the last wrong passcode was entered (uptime ms), for feedback.
    wrong_at: Option<u64>,
    checking: Checking,
    ui: UiState,
}

impl Lock {
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Locked with at least one passcode digit typed (or being checked).
    pub fn has_typed_digits(&self) -> bool {
        self.locked && !self.entry.is_empty()
    }

    /// Lock now if a passcode is configured.
    pub fn lock_now<E: Env + ?Sized>(&mut self, env: &mut E) {
        if !is_configured(env) {
            return;
        }
        self.locked = true;
        self.entry.clear();
        self.wrong_at = None;
        self.checking = Checking::Idle;
        self.ui = UiState::default();
        self.throttle = Throttle::load(env);
    }

    pub fn on_screen_off<E: Env + ?Sized>(&mut self, env: &mut E) {
        self.screen_events = true;
        if AutoLock::load(env) == AutoLock::ScreenOff {
            self.lock_now(env);
        }
    }

    /// The screen woke: apply a timed auto-lock that ran out while it was off.
    pub fn on_wake<E: Env + ?Sized>(&mut self, env: &mut E) {
        self.screen_events = true;
        if !self.locked && self.idle_expired(env) {
            self.lock_now(env);
        }
    }

    fn timeout_ms<E: Env + ?Sized>(&self, env: &E) -> Option<u64> {
        match AutoLock::load(env) {
            AutoLock::ScreenOff if self.screen_events => None,
            AutoLock::ScreenOff => Some(SCREEN_OFF_FALLBACK_MS),
            AutoLock::OneMinute => Some(60_000),
            AutoLock::FiveMinutes => Some(5 * 60_000),
            AutoLock::Never => None,
        }
    }

    fn idle_expired<E: Env + ?Sized>(&self, env: &E) -> bool {
        let idle = env.uptime_ms().saturating_sub(self.last_activity_ms);
        self.timeout_ms(env).is_some_and(|limit| idle >= limit)
    }

    /// Run before every desktop frame: lock at boot and on inactivity, and
    /// release a lock whose passcode was cleared from the serial console.
    /// Returns true when the lock engaged in this call; the frame's input
    /// must then not reach the keypad (it was the wake-up touch).
    pub fn check<E: Env + ?Sized>(&mut self, input: &Input, env: &mut E) -> bool {
        let now = env.uptime_ms();
        if !self.booted {
            self.booted = true;
            self.last_activity_ms = now;
            self.lock_now(env);
            return self.locked;
        }
        if self.locked {
            if !is_configured(env) {
                self.unlock(now);
            }
            return false;
        }
        if self.idle_expired(env) {
            self.lock_now(env);
            if self.locked {
                return true;
            }
        }
        if input.pressed || input.released || input.down || !input.keys.is_empty() {
            self.last_activity_ms = now;
        }
        false
    }

    fn unlock(&mut self, now: u64) {
        self.locked = false;
        self.entry.clear();
        self.wrong_at = None;
        self.last_activity_ms = now;
    }

    /// Check the typed passcode; unlocks on success.
    fn submit<E: Env + ?Sized>(&mut self, env: &mut E) {
        let now = env.uptime_ms();
        if self.entry.is_empty() || self.throttle.remaining_ms(now) > 0 {
            return;
        }
        let ok = env.get(LOCK_PIN).and_then(|v| pin::PinHash::decode(&v)).is_some_and(|h| h.verify(&self.entry));
        self.entry.clear();
        if ok {
            self.throttle.record_success(env);
            self.unlock(now);
        } else {
            self.throttle.record_failure(env);
            self.wrong_at = Some(now);
        }
    }

    /// Draw the lock screen over the whole canvas and handle its input.
    /// Returns the delay until the next frame it needs.
    pub fn frame(&mut self, canvas: &mut Canvas, input: &Input, fonts: &Fonts, theme: &Theme, sys: &mut dyn System) -> Option<u32> {
        screen::frame(self, canvas, input, fonts, theme, sys)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use p4os_sdk::config::{LOCK_FAILS, LOCK_UNTIL};

    #[derive(Default)]
    pub struct MockEnv {
        pub config: BTreeMap<String, String>,
        pub uptime: u64,
        pub unix: Option<i64>,
        pub no_random: bool,
        pub lock_requests: u32,
        pub writes: u32,
    }

    impl Env for MockEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.config.get(key).cloned()
        }
        fn set(&mut self, key: &str, value: &str) {
            self.writes += 1;
            self.config.insert(key.into(), value.into());
        }
        fn random(&mut self, buf: &mut [u8]) -> bool {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (i as u8).wrapping_mul(37).wrapping_add(self.writes as u8);
            }
            !self.no_random
        }
        fn uptime_ms(&self) -> u64 {
            self.uptime
        }
        fn unix_now(&self) -> Option<i64> {
            self.unix
        }
        fn request_lock(&mut self) {
            self.lock_requests += 1;
        }
    }

    pub fn entry(digits: &str) -> PinEntry {
        let mut e = PinEntry::default();
        for b in digits.bytes() {
            e.push(b);
        }
        e
    }

    /// A mock with lock enabled and passcode `pin`.
    pub fn configured(pin: &str) -> MockEnv {
        let mut env = MockEnv::default();
        let hash = pin::PinHash::new(&entry(pin), [9; pin::SALT_LEN], pin::ITERATIONS);
        env.set(LOCK_PIN, &hash.encode());
        env.set(LOCK_ENABLED, "1");
        env
    }

    fn type_pin(lock: &mut Lock, env: &mut MockEnv, pin: &str) {
        lock.entry = entry(pin);
        lock.submit(env);
    }

    fn idle() -> Input {
        Input::default()
    }

    fn touch() -> Input {
        Input { pressed: true, down: true, ..Default::default() }
    }

    #[test]
    fn unix_time() {
        let t = |year, month, day, hour, minute, second| DateTime { year, month, day, hour, minute, second };
        assert_eq!(unix_seconds(t(1970, 1, 1, 0, 0, 0)), 0);
        assert_eq!(unix_seconds(t(2000, 3, 1, 0, 0, 0)), 951_868_800);
        assert_eq!(unix_seconds(t(2026, 10, 28, 15, 9, 30)), 1_793_200_170);
    }

    #[test]
    fn auto_lock_values() {
        for a in AutoLock::ALL {
            assert_eq!(AutoLock::parse(Some(a.value())), a);
        }
        assert_eq!(AutoLock::parse(None), AutoLock::ScreenOff);
        assert_eq!(AutoLock::parse(Some("soon")), AutoLock::ScreenOff);
    }

    #[test]
    fn typed_digits_are_reported_until_submitted() {
        let mut env = configured("1234");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        assert!(!lock.has_typed_digits());
        lock.entry.push(b'1');
        assert!(lock.has_typed_digits());
        lock.entry.push(b'2');
        lock.submit(&mut env);
        assert!(!lock.has_typed_digits(), "a wrong attempt clears the entry");
        type_pin(&mut lock, &mut env, "1234");
        assert!(!lock.is_locked() && !lock.has_typed_digits());
    }

    #[test]
    fn needs_both_switch_and_passcode() {
        let mut env = MockEnv::default();
        assert!(!is_configured(&env));
        env.set(LOCK_ENABLED, "1");
        assert!(!is_configured(&env));
        env.set(LOCK_PIN, "");
        assert!(!is_configured(&env));
        let mut env = configured("1234");
        assert!(is_configured(&env));
        env.set(LOCK_ENABLED, "0");
        assert!(!is_configured(&env));
        let mut lock = Lock::default();
        lock.lock_now(&mut env);
        assert!(!lock.is_locked());
    }

    #[test]
    fn locks_at_boot_only_when_configured() {
        let mut env = MockEnv::default();
        let mut lock = Lock::default();
        assert!(!lock.check(&idle(), &mut env));
        assert!(!lock.is_locked());

        let mut env = configured("1234");
        let mut lock = Lock::default();
        assert!(lock.check(&idle(), &mut env));
        assert!(lock.is_locked());
    }

    #[test]
    fn right_passcode_unlocks_wrong_one_does_not() {
        let mut env = configured("1234");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        type_pin(&mut lock, &mut env, "4321");
        assert!(lock.is_locked());
        assert!(lock.wrong_at.is_some());
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("1"));
        type_pin(&mut lock, &mut env, "1234");
        assert!(!lock.is_locked());
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("0"));
        assert!(lock.entry.is_empty());
    }

    #[test]
    fn lockout_blocks_even_the_right_passcode_and_survives_reboot() {
        let mut env = configured("1234");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        for _ in 0..5 {
            type_pin(&mut lock, &mut env, "0000");
        }
        type_pin(&mut lock, &mut env, "1234");
        assert!(lock.is_locked(), "attempts during a lockout are ignored");
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("5"));

        // Reboot: a fresh desktop still enforces the lockout.
        env.uptime = 0;
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        assert_eq!(lock.throttle.remaining_ms(0), 30_000);
        type_pin(&mut lock, &mut env, "1234");
        assert!(lock.is_locked());
        env.uptime = 30_000;
        lock.throttle.refresh(&mut env);
        type_pin(&mut lock, &mut env, "1234");
        assert!(!lock.is_locked());
    }

    #[test]
    fn inactivity_locks_and_swallows_the_waking_touch() {
        let mut env = configured("1234");
        env.set(LOCK_AUTO, "1m");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        type_pin(&mut lock, &mut env, "1234");
        env.uptime = 59_000;
        assert!(!lock.check(&touch(), &mut env));
        env.uptime = 59_000 + 59_999;
        assert!(!lock.check(&idle(), &mut env));
        env.uptime += 1;
        assert!(lock.check(&touch(), &mut env));
        assert!(lock.is_locked());
    }

    #[test]
    fn never_does_not_auto_lock() {
        let mut env = configured("1234");
        env.set(LOCK_AUTO, "never");
        env.set(LOCK_ENABLED, "0");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        env.set(LOCK_ENABLED, "1");
        env.uptime = 10 * 60 * 60_000;
        assert!(!lock.check(&idle(), &mut env));
        lock.on_screen_off(&mut env);
        assert!(!lock.is_locked());
        lock.lock_now(&mut env);
        assert!(lock.is_locked());
    }

    #[test]
    fn screen_off_policy_uses_events_once_they_arrive() {
        let mut env = configured("1234");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        type_pin(&mut lock, &mut env, "1234");
        // No display events yet: the fallback timer applies.
        env.uptime = SCREEN_OFF_FALLBACK_MS;
        assert!(lock.check(&idle(), &mut env));
        type_pin(&mut lock, &mut env, "1234");
        // Once events arrive, only screen-off locks.
        lock.on_wake(&mut env);
        assert!(!lock.is_locked());
        env.uptime += 10 * SCREEN_OFF_FALLBACK_MS;
        assert!(!lock.check(&idle(), &mut env));
        lock.on_screen_off(&mut env);
        assert!(lock.is_locked());
    }

    #[test]
    fn wake_applies_a_timer_that_ran_out_while_off() {
        let mut env = configured("1234");
        env.set(LOCK_AUTO, "5m");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        type_pin(&mut lock, &mut env, "1234");
        lock.on_screen_off(&mut env);
        assert!(!lock.is_locked());
        env.uptime += 4 * 60_000;
        lock.on_wake(&mut env);
        assert!(!lock.is_locked());
        env.uptime += 60_000;
        lock.on_wake(&mut env);
        assert!(lock.is_locked());
    }

    #[test]
    fn clearing_the_passcode_releases_the_lock() {
        let mut env = configured("1234");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        assert!(lock.is_locked());
        env.config.remove(LOCK_PIN);
        env.config.remove(LOCK_UNTIL);
        lock.check(&idle(), &mut env);
        assert!(!lock.is_locked());
    }

    #[test]
    fn corrupt_record_fails_closed() {
        let mut env = configured("1234");
        env.set(LOCK_PIN, "v9$whatever");
        let mut lock = Lock::default();
        lock.check(&idle(), &mut env);
        assert!(lock.is_locked());
        type_pin(&mut lock, &mut env, "1234");
        assert!(lock.is_locked());
    }
}
