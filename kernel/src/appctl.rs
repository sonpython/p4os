//! Supervisor for isolated user-mode apps (untrusted code): how many may
//! run, how much RAM each may take, and a CPU budget.
//!
//! * Memory: an app's whole RAM (image, data, stack) is allocated once at
//!   load, so the quota is checked there, per app and in total. There is
//!   no allocation syscall yet; one would charge the same quota.
//! * CPU: user apps run at the lowest priority, below every kernel and GUI
//!   thread, so they only get otherwise idle time. On top of that each app
//!   may use [`CPU_BUDGET_PERMILLE`] of a core per one-second window: once
//!   used up, the scheduler keeps it off the CPU until the window ends.
//!   An app throttled for [`KILL_AFTER_WINDOWS`] windows in a row is
//!   killed.
//!
//! The supervisor thread samples the scheduler's per-thread CPU accounting
//! every [`CHECK_MS`], so an app may overrun its budget by up to that much
//! per window.
//!
//! Built-in apps run inside the GUI thread and are managed by the desktop
//! (`p4os_apps::wm`); their usage is published here for the shell.

use crate::apploader::{self, LoadError};
use crate::sync::Mutex;
use crate::usermode::{self, AppStatus};
use crate::{kprintln, psram, sched, time};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use p4os_apps::wm::policy::{TickBudget, TickDecision};
use p4os_sdk::{AppHandle, AppState, AppUsage};

pub const MAX_APPS: usize = 4;
/// RAM one app may take (image + data + stack).
pub const APP_RAM_QUOTA: usize = 192 * 1024;
/// RAM all user apps together may take.
pub const TOTAL_RAM_QUOTA: usize = 512 * 1024;
/// Share of one core an app may use per window, in 1/1000.
pub const CPU_BUDGET_PERMILLE: u32 = 250;
pub const WINDOW_MS: u32 = 1000;
pub const KILL_AFTER_WINDOWS: u32 = 10;
pub const CHECK_MS: u32 = 20;

#[derive(Debug)]
pub enum StartError {
    Load(LoadError),
    TooManyApps,
    OverQuota { need: usize, available: usize },
}

impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            StartError::Load(e) => write!(f, "{}", e),
            StartError::TooManyApps => write!(f, "{} user apps are running already", MAX_APPS),
            StartError::OverQuota { need, available } => {
                write!(f, "needs {} KiB RAM, the quota leaves {} KiB", need.div_ceil(1024), available / 1024)
            }
        }
    }
}

impl From<LoadError> for StartError {
    fn from(e: LoadError) -> Self {
        StartError::Load(e)
    }
}

struct Entry {
    tid: usize,
    name: String,
    ram: usize,
    in_psram: bool,
    status: Arc<AppStatus>,
    start_ticks: u32,
    budget: TickBudget,
}

static APPS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static BUILTIN: Mutex<Vec<AppUsage>> = Mutex::new(Vec::new());
static SUPERVISOR: AtomicBool = AtomicBool::new(false);

/// Load and start the user app in `slot`, within the limits above.
pub fn start(slot: usize, arg: u32) -> Result<(usize, Arc<AppStatus>), StartError> {
    let header = apploader::probe(slot)?;
    let need = header.ram_size() as usize;
    // Held across the load so two starts cannot both pass the checks.
    let mut apps = APPS.lock();
    apps.retain(|e| e.status.outcome().is_none());
    if apps.len() >= MAX_APPS {
        return Err(StartError::TooManyApps);
    }
    let used: usize = apps.iter().map(|e| e.ram).sum();
    let available = APP_RAM_QUOTA.min(TOTAL_RAM_QUOTA - used);
    if need > available {
        return Err(StartError::OverQuota { need, available });
    }
    let (tid, status, base) = apploader::run(slot, arg)?;
    let run = sched::run_ticks(tid).unwrap_or(0);
    let in_psram = (psram::BASE..psram::BASE + 0x0400_0000).contains(&base);
    apps.push(Entry {
        tid,
        name: String::from(header.name()),
        ram: need,
        in_psram,
        status: status.clone(),
        start_ticks: run,
        budget: TickBudget::new(WINDOW_MS, CPU_BUDGET_PERMILLE, KILL_AFTER_WINDOWS, time::ticks(), run),
    });
    drop(apps);
    if !SUPERVISOR.swap(true, Ordering::AcqRel) {
        sched::spawn("appmon", 4, 6 * 1024, supervise, 0);
    }
    Ok((tid, status))
}

/// Kill the user app running in thread `tid`.
pub fn kill(tid: usize) -> bool {
    let known = APPS.lock().iter().any(|e| e.tid == tid);
    known && usermode::request_kill(tid)
}

fn supervise(_: usize) {
    loop {
        sched::sleep_ms(CHECK_MS);
        let now = time::ticks();
        let mut apps = APPS.lock();
        apps.retain(|e| e.status.outcome().is_none());
        for e in apps.iter_mut() {
            let Some(run) = sched::run_ticks(e.tid) else { continue };
            match e.budget.check(now, run) {
                TickDecision::Run => {}
                TickDecision::Throttle(until) => {
                    if e.budget.over_windows == 0 {
                        kprintln!("appmon: '{}' (thread {}) used its CPU budget, throttled", e.name, e.tid);
                    }
                    sched::hold_until(e.tid, until);
                }
                TickDecision::Kill => {
                    kprintln!(
                        "appmon: '{}' (thread {}) exceeded its CPU budget for {} s, killing it",
                        e.name, e.tid, KILL_AFTER_WINDOWS
                    );
                    usermode::request_kill(e.tid);
                }
            }
        }
    }
}

/// Running user apps with their resource use.
pub fn user_apps() -> Vec<AppUsage> {
    let apps = APPS.lock();
    apps.iter()
        .filter(|e| e.status.outcome().is_none())
        .map(|e| {
            let cpu = sched::run_ticks(e.tid).unwrap_or(e.start_ticks).wrapping_sub(e.start_ticks);
            let (sram, psram) = if e.in_psram { (0, e.ram) } else { (e.ram, 0) };
            AppUsage {
                handle: AppHandle::User(e.tid as u32),
                name: e.name.clone(),
                // No UI: always off screen.
                state: AppState::Background,
                cpu_permille: e.budget.last_permille,
                cpu_ms: cpu as u64,
                sram,
                psram,
                note: String::from(if e.budget.throttled() { "throttled" } else { "" }),
            }
        })
        .collect()
}

/// The desktop's built-in apps, published by the GUI thread for `top`.
pub fn publish_builtin(list: Vec<AppUsage>) {
    let old = core::mem::replace(&mut *BUILTIN.lock(), list);
    drop(old);
}

/// Shell `top`: every running app with CPU and memory.
pub fn print_top() {
    let fmt_state = |s: AppState| match s {
        AppState::Foreground => "fg",
        AppState::Background => "bg",
        AppState::Suspended => "susp",
        AppState::Stopped => "stop",
    };
    kprintln!("  handle name             state  cpu%  cpu ms    sram KiB  psram KiB  note");
    let builtin = BUILTIN.lock().clone();
    for a in builtin.iter().chain(user_apps().iter()) {
        let handle = match a.handle {
            AppHandle::Builtin(t) => alloc::format!("b{}", t),
            AppHandle::User(t) => alloc::format!("t{}", t),
        };
        kprintln!(
            "  {:>6} {:<16} {:<5} {:>3}.{} {:>8} {:>9} {:>10}  {}",
            handle,
            a.name,
            fmt_state(a.state),
            a.cpu_permille / 10,
            a.cpu_permille % 10,
            a.cpu_ms,
            a.sram / 1024,
            a.psram / 1024,
            a.note
        );
    }
    match crate::gui::busy_ms() {
        Some(ms) if ms > 100 => kprintln!("  gui: current frame has run for {} ms", ms),
        _ => {}
    }
    kprintln!(
        "  limits: {} user apps, {} KiB each / {} KiB total, {}.{}% CPU per app; built-in apps per p4os_apps::wm::policy",
        MAX_APPS,
        APP_RAM_QUOTA / 1024,
        TOTAL_RAM_QUOTA / 1024,
        CPU_BUDGET_PERMILLE / 10,
        CPU_BUDGET_PERMILLE % 10
    );
}
