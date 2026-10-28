//! Background Wi-Fi bring-up. One thread owns `wifi::connect`: it joins the
//! requested network (the saved one at boot) and retries with exponential
//! backoff on failure, so neither boot nor the GUI ever waits for it.
//! Shell commands and apps hand it credentials through `request`.

use crate::sync::{Mutex, Semaphore};
use crate::{config, kprintln, sched, time, wifi};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use p4os_sdk::config::{WIFI_PASSWORD, WIFI_SSID};

const FIRST_RETRY_MS: u32 = 5_000;
const MAX_RETRY_MS: u32 = 120_000;
const STACK: usize = 24 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Creds {
    ssid: String,
    password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Connecting,
    Up,
    /// The last attempt failed; the next one starts at `retry_at` (ms).
    Retrying { error: String, retry_at: u32 },
}

/// Credentials waiting for the thread to pick up.
static PENDING: Mutex<Option<Creds>> = Mutex::new(None);
static WAKE: Semaphore = Semaphore::new(0);
/// Bumped by every request or cancel; an attempt belongs to one generation.
static GENERATION: AtomicU32 = AtomicU32::new(0);
/// Phase of the current generation and the SSID it is for.
static STATUS: Mutex<(u32, Phase, String)> = Mutex::new((0, Phase::Idle, String::new()));
/// Network the link is currently up on.
static JOINED: Mutex<Option<Creds>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// At boot: join the saved network, if any, in the background.
pub fn start_from_config() {
    if let Some(ssid) = config::get(WIFI_SSID) {
        let password = config::get(WIFI_PASSWORD).unwrap_or_default();
        kprintln!("wifi: auto-connecting to '{}'", ssid);
        request(&ssid, &password);
    }
}

/// Ask the background thread to join `ssid`; supersedes any earlier
/// request. Returns the generation to pass to `wait`.
pub fn request(ssid: &str, password: &str) -> u32 {
    let creds = Creds { ssid: ssid.into(), password: password.into() };
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    if wifi::is_connected() && JOINED.lock().as_ref() == Some(&creds) {
        // Already on this network; resetting the co-processor would only
        // drop the link.
        *STATUS.lock() = (generation, Phase::Up, creds.ssid);
        return generation;
    }
    *STATUS.lock() = (generation, Phase::Connecting, creds.ssid.clone());
    *PENDING.lock() = Some(creds);
    if !RUNNING.swap(true, Ordering::AcqRel) {
        sched::spawn("wifi-auto", 1, STACK, run, 0);
    }
    WAKE.release();
    generation
}

/// Stop retrying (after `wifi forget`). An established link stays up.
pub fn cancel() {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    PENDING.lock().take();
    let up = wifi::is_connected();
    *STATUS.lock() = (generation, if up { Phase::Up } else { Phase::Idle }, String::new());
}

/// Wait up to `timeout_ms` for the first attempt of `generation` to finish.
/// `None`: still connecting, or superseded by a newer request.
pub fn wait(generation: u32, timeout_ms: u32) -> Option<Result<(), String>> {
    let start = time::ticks();
    while time::ticks().wrapping_sub(start) < timeout_ms {
        {
            let s = STATUS.lock();
            if s.0 != generation {
                return None;
            }
            match &s.1 {
                Phase::Up => return Some(Ok(())),
                Phase::Retrying { error, .. } => return Some(Err(error.clone())),
                Phase::Idle | Phase::Connecting => {}
            }
        }
        sched::sleep_ms(100);
    }
    None
}

/// One-line link state for status displays.
pub fn describe() -> Option<String> {
    let s = STATUS.lock();
    match &s.1 {
        Phase::Idle | Phase::Up => None,
        Phase::Connecting => Some(format!("connecting to {}...", s.2)),
        Phase::Retrying { retry_at, .. } => {
            let secs = retry_at.wrapping_sub(time::ticks()) as i32 / 1000;
            Some(format!("{}: retry in {} s", s.2, secs.max(0)))
        }
    }
}

pub fn phase() -> Phase {
    STATUS.lock().1.clone()
}

fn set_phase(generation: u32, phase: Phase) {
    let mut s = STATUS.lock();
    if s.0 == generation {
        s.1 = phase;
    }
}

fn current(generation: u32) -> bool {
    GENERATION.load(Ordering::Acquire) == generation
}

fn run(_: usize) {
    loop {
        WAKE.acquire();
        let Some(creds) = PENDING.lock().take() else { continue };
        let generation = GENERATION.load(Ordering::Acquire);
        let mut delay = FIRST_RETRY_MS;
        while current(generation) {
            set_phase(generation, Phase::Connecting);
            JOINED.lock().take();
            match wifi::connect(&creds.ssid, &creds.password) {
                Ok(()) => {
                    *JOINED.lock() = Some(creds.clone());
                    set_phase(generation, Phase::Up);
                    break;
                }
                Err(e) => {
                    kprintln!("wifi: joining '{}' failed: {}; retry in {} s", creds.ssid, e, delay / 1000);
                    let retry_at = time::ticks().wrapping_add(delay);
                    set_phase(generation, Phase::Retrying { error: e, retry_at });
                    // Sleep in slices so a new request or cancel takes over.
                    while current(generation) && (retry_at.wrapping_sub(time::ticks()) as i32) > 0 {
                        sched::sleep_ms(200);
                    }
                    delay = (delay * 2).min(MAX_RETRY_MS);
                }
            }
        }
    }
}
