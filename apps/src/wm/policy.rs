//! Resource policy for built-in apps: how many may run, how much memory
//! and background CPU each may use, what to do under memory pressure, and
//! when a foreground app counts as hung. Pure logic, tested on the host;
//! the desktop feeds it measurements and applies its decisions.

use p4os_sdk::AppState;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Apps running at once (foreground + background + suspended).
    pub max_running: usize,
    /// Per-app heap budget. Background apps above it are asked to trim,
    /// then stopped; the foreground app is stopped above twice the budget.
    pub app_sram: usize,
    pub app_psram: usize,
    /// Free heap below which the system is under memory pressure.
    pub min_free_sram: usize,
    pub min_free_psram: usize,
    /// Time an app gets after `on_low_memory` before it may be stopped.
    pub trim_grace_ms: u64,
    /// Background CPU budget per app, in 1/1000 of one core per second.
    pub bg_cpu_permille: u32,
    /// Consecutive seconds over the background budget before an app is
    /// suspended.
    pub bg_over_windows: u32,
    /// Shortest interval between background ticks.
    pub bg_min_tick_ms: u32,
    /// A foreground frame taking longer than this is a strike.
    pub frame_limit_ms: u32,
    /// Strikes within `hang_window_ms` that make the desktop offer to close
    /// the app.
    pub hang_strikes: u32,
    pub hang_window_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_running: 6,
            app_sram: 160 * 1024,
            app_psram: 8 * 1024 * 1024,
            min_free_sram: 48 * 1024,
            min_free_psram: 2 * 1024 * 1024,
            trim_grace_ms: 2000,
            bg_cpu_permille: 100,
            bg_over_windows: 5,
            bg_min_tick_ms: 100,
            frame_limit_ms: 1000,
            hang_strikes: 3,
            hang_window_ms: 60_000,
        }
    }
}

/// What the memory policy knows about one running app.
#[derive(Clone, Copy, Debug)]
pub struct AppRecord {
    pub state: AppState,
    /// When the user last had the app in the foreground (ms).
    pub last_used: u64,
    pub sram: usize,
    pub psram: usize,
    /// When `on_low_memory` was last sent and not yet resolved.
    pub trimmed_at: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KillReason {
    /// Above its own memory budget after being asked to trim.
    OverBudget,
    /// The system ran short of memory.
    Pressure,
    /// Room for a newly launched app.
    AppLimit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Send `on_low_memory` to the app at this index.
    Trim(usize),
    Kill(usize, KillReason),
}

fn over(a: &AppRecord, l: &Limits, factor: usize) -> bool {
    a.sram > l.app_sram * factor || a.psram > l.app_psram * factor
}

fn off_screen(a: &AppRecord) -> bool {
    matches!(a.state, AppState::Background | AppState::Suspended)
}

/// Least valuable app to stop: never the foreground one; suspended apps
/// before ones doing background work (audio), then least recently used.
pub fn victim(apps: &[AppRecord], exclude: &[usize]) -> Option<usize> {
    apps.iter()
        .enumerate()
        .filter(|(i, a)| off_screen(a) && !exclude.contains(i))
        .min_by_key(|(_, a)| (a.state == AppState::Background, a.last_used))
        .map(|(i, _)| i)
}

/// App to stop so that one more can start, if the limit is reached.
pub fn launch_victim(apps: &[AppRecord], l: &Limits) -> Option<usize> {
    let running = apps.iter().filter(|a| a.state != AppState::Stopped).count();
    if running < l.max_running { None } else { victim(apps, &[]) }
}

pub fn under_pressure(free_sram: usize, free_psram: usize, l: &Limits) -> bool {
    free_sram < l.min_free_sram || free_psram < l.min_free_psram
}

/// One pass of the memory policy (run about once a second). Updates
/// `trimmed_at` and returns what to do; at most one app is stopped for
/// pressure per pass so the effect can be measured before the next.
pub fn plan(apps: &mut [AppRecord], free_sram: usize, free_psram: usize, l: &Limits, now: u64) -> alloc::vec::Vec<Action> {
    let mut actions = alloc::vec::Vec::new();
    let pressure = under_pressure(free_sram, free_psram, l);
    let grace_over = |a: &AppRecord| a.trimmed_at.is_some_and(|t| now.saturating_sub(t) >= l.trim_grace_ms);
    let mut killed = alloc::vec::Vec::new();

    for (i, a) in apps.iter_mut().enumerate() {
        match a.state {
            AppState::Stopped => continue,
            AppState::Foreground if over(a, l, 2) => {
                actions.push(Action::Kill(i, KillReason::OverBudget));
                killed.push(i);
            }
            AppState::Foreground | AppState::Background | AppState::Suspended if over(a, l, 1) => {
                if a.state != AppState::Foreground && grace_over(a) {
                    actions.push(Action::Kill(i, KillReason::OverBudget));
                    killed.push(i);
                } else if a.trimmed_at.is_none() {
                    a.trimmed_at = Some(now);
                    actions.push(Action::Trim(i));
                }
            }
            _ if !pressure => a.trimmed_at = None,
            _ => {}
        }
    }

    if pressure {
        let mut untrimmed = false;
        for (i, a) in apps.iter_mut().enumerate() {
            if off_screen(a) && a.trimmed_at.is_none() && !killed.contains(&i) {
                a.trimmed_at = Some(now);
                actions.push(Action::Trim(i));
                untrimmed = true;
            }
        }
        // Everyone was asked already and had time to react: stop one.
        let waited = apps.iter().enumerate().filter(|(i, a)| off_screen(a) && !killed.contains(i)).all(|(_, a)| grace_over(a));
        if !untrimmed && waited && killed.is_empty() {
            if let Some(v) = victim(apps, &killed) {
                actions.push(Action::Kill(v, KillReason::Pressure));
            }
        }
    }
    actions
}

/// CPU time of one app, measured by the desktop around its callbacks.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuMeter {
    window_start: u64,
    window_us: u64,
    pub total_us: u64,
    /// Share of a core used in the last complete one-second window.
    pub permille: u16,
    /// Consecutive windows the app spent over the background budget.
    pub over_windows: u32,
}

pub const WINDOW_MS: u64 = 1000;

impl CpuMeter {
    pub fn new(now: u64) -> Self {
        Self { window_start: now, ..Default::default() }
    }

    pub fn add(&mut self, us: u64) {
        self.window_us += us;
        self.total_us += us;
    }

    /// May the app run another background tick in this window?
    pub fn bg_allowed(&self, l: &Limits) -> bool {
        self.window_us < l.bg_cpu_permille as u64 * WINDOW_MS
    }

    /// Close the window if it is over. `background` says whether the app
    /// was in the background, where the budget applies. Returns true when
    /// the app has been over its background budget for too long.
    pub fn roll(&mut self, now: u64, background: bool, l: &Limits) -> bool {
        let elapsed = now.saturating_sub(self.window_start);
        if elapsed < WINDOW_MS {
            return false;
        }
        self.permille = (self.window_us / elapsed).min(1000) as u16;
        let budget_us = l.bg_cpu_permille as u64 * elapsed;
        if background && self.window_us >= budget_us {
            self.over_windows += 1;
        } else {
            self.over_windows = 0;
        }
        self.window_start = now;
        self.window_us = 0;
        self.over_windows >= l.bg_over_windows
    }
}

/// Counts slow foreground frames.
#[derive(Clone, Copy, Debug, Default)]
pub struct HangWatch {
    strikes: u32,
    first: u64,
    pub worst_ms: u32,
}

impl HangWatch {
    /// Record a frame that took `ms`. Returns true when the app has been
    /// slow often enough that the user should be offered to close it; the
    /// count then starts over.
    pub fn record(&mut self, ms: u32, now: u64, l: &Limits) -> bool {
        if ms <= l.frame_limit_ms {
            return false;
        }
        if self.strikes == 0 || now.saturating_sub(self.first) > l.hang_window_ms {
            self.strikes = 0;
            self.first = now;
            self.worst_ms = 0;
        }
        self.strikes += 1;
        self.worst_ms = self.worst_ms.max(ms);
        if self.strikes >= l.hang_strikes {
            self.strikes = 0;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AppState::*;

    const K: usize = 1024;

    fn rec(state: AppState, last_used: u64, sram: usize, psram: usize) -> AppRecord {
        AppRecord { state, last_used, sram, psram, trimmed_at: None }
    }

    fn roomy() -> (usize, usize) {
        (400 * K, 20 * K * K)
    }

    #[test]
    fn victim_is_lru_and_prefers_suspended() {
        let apps = [
            rec(Foreground, 1, 0, 0),
            rec(Background, 2, 0, 0),
            rec(Suspended, 9, 0, 0),
            rec(Suspended, 5, 0, 0),
            rec(Stopped, 0, 0, 0),
        ];
        assert_eq!(victim(&apps, &[]), Some(3));
        assert_eq!(victim(&apps, &[3]), Some(2));
        assert_eq!(victim(&apps, &[2, 3]), Some(1));
        // Never the foreground app.
        assert_eq!(victim(&apps[..1], &[]), None);
    }

    #[test]
    fn launch_limit() {
        let l = Limits { max_running: 3, ..Default::default() };
        let mut apps = alloc::vec![rec(Foreground, 5, 0, 0), rec(Suspended, 1, 0, 0), rec(Stopped, 0, 0, 0)];
        assert_eq!(launch_victim(&apps, &l), None);
        apps.push(rec(Background, 3, 0, 0));
        assert_eq!(launch_victim(&apps, &l), Some(1));
    }

    #[test]
    fn background_app_over_budget_is_trimmed_then_killed() {
        let l = Limits::default();
        let (fs, fp) = roomy();
        let mut apps = [rec(Foreground, 9, 0, 0), rec(Suspended, 1, l.app_sram + 1, 0)];
        assert_eq!(plan(&mut apps, fs, fp, &l, 1000), [Action::Trim(1)]);
        // Still within the grace period: nothing more.
        assert!(plan(&mut apps, fs, fp, &l, 2000).is_empty());
        assert_eq!(plan(&mut apps, fs, fp, &l, 3000), [Action::Kill(1, KillReason::OverBudget)]);
    }

    #[test]
    fn trimming_that_works_is_forgotten() {
        let l = Limits::default();
        let (fs, fp) = roomy();
        let mut apps = [rec(Suspended, 1, 0, l.app_psram + 1)];
        assert_eq!(plan(&mut apps, fs, fp, &l, 0), [Action::Trim(0)]);
        apps[0].psram = 10;
        assert!(plan(&mut apps, fs, fp, &l, 5000).is_empty());
        assert_eq!(apps[0].trimmed_at, None);
    }

    #[test]
    fn foreground_is_trimmed_but_only_killed_far_over_budget() {
        let l = Limits::default();
        let (fs, fp) = roomy();
        let mut apps = [rec(Foreground, 1, l.app_sram + 1, 0)];
        assert_eq!(plan(&mut apps, fs, fp, &l, 0), [Action::Trim(0)]);
        assert!(plan(&mut apps, fs, fp, &l, 10_000).is_empty());
        apps[0].sram = l.app_sram * 2 + 1;
        assert_eq!(plan(&mut apps, fs, fp, &l, 11_000), [Action::Kill(0, KillReason::OverBudget)]);
    }

    #[test]
    fn pressure_trims_everyone_then_kills_lru_one_at_a_time() {
        let l = Limits::default();
        let low_sram = l.min_free_sram - 1;
        let fp = 20 * K * K;
        let mut apps = [rec(Foreground, 9, 0, 0), rec(Background, 2, 0, 0), rec(Suspended, 5, 0, 0), rec(Suspended, 3, 0, 0)];
        assert_eq!(plan(&mut apps, low_sram, fp, &l, 0), [Action::Trim(1), Action::Trim(2), Action::Trim(3)]);
        assert!(plan(&mut apps, low_sram, fp, &l, 1000).is_empty());
        assert_eq!(plan(&mut apps, low_sram, fp, &l, 2000), [Action::Kill(3, KillReason::Pressure)]);
        apps[3].state = Stopped;
        assert_eq!(plan(&mut apps, low_sram, fp, &l, 3000), [Action::Kill(2, KillReason::Pressure)]);
        apps[2].state = Stopped;
        // The app playing audio goes last, and the foreground app never.
        assert_eq!(plan(&mut apps, low_sram, fp, &l, 4000), [Action::Kill(1, KillReason::Pressure)]);
        apps[1].state = Stopped;
        assert!(plan(&mut apps, low_sram, fp, &l, 5000).is_empty());
    }

    #[test]
    fn psram_pressure_counts_too() {
        let l = Limits::default();
        assert!(under_pressure(400 * K, l.min_free_psram - 1, &l));
        assert!(!under_pressure(400 * K, l.min_free_psram, &l));
    }

    #[test]
    fn cpu_meter_budget_and_suspension() {
        let l = Limits { bg_cpu_permille: 100, bg_over_windows: 2, ..Default::default() };
        let mut m = CpuMeter::new(0);
        m.add(50_000);
        assert!(m.bg_allowed(&l));
        m.add(60_000);
        assert!(!m.bg_allowed(&l));
        assert!(!m.roll(500, true, &l), "window not over yet");
        assert!(!m.roll(1000, true, &l));
        assert_eq!(m.permille, 110);
        assert_eq!(m.over_windows, 1);
        assert!(m.bg_allowed(&l), "a new window starts with a fresh budget");
        m.add(100_000);
        assert!(m.roll(2000, true, &l));
        // In the foreground the same use is fine.
        let mut f = CpuMeter::new(0);
        f.add(900_000);
        assert!(!f.roll(1000, false, &l));
        assert_eq!(f.permille, 900);
        assert_eq!(f.over_windows, 0);
        assert_eq!(f.total_us, 900_000);
    }

    #[test]
    fn hang_watch_needs_repeated_slow_frames() {
        let l = Limits { frame_limit_ms: 500, hang_strikes: 3, hang_window_ms: 10_000, ..Default::default() };
        let mut h = HangWatch::default();
        assert!(!h.record(100, 0, &l));
        assert!(!h.record(600, 0, &l));
        assert!(!h.record(700, 1000, &l));
        // A strike outside the window starts a new count.
        assert!(!h.record(800, 20_000, &l));
        assert!(!h.record(900, 21_000, &l));
        assert!(h.record(650, 22_000, &l));
        assert_eq!(h.worst_ms, 900);
        assert!(!h.record(650, 23_000, &l), "count restarts after a report");
    }
}
