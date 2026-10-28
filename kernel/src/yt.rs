//! `yt` shell commands: drive the YouTube player (`p4os-ytplay`) from the
//! console, and benchmark its decoders on core 1.

use crate::{kprintln, net, sched, systimer, time};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::FetchState;
use p4os_ytplay::bench;
use p4os_ytplay::remote;

const USAGE: &str = "usage: yt play ID|URL [144|240|audio] | yt pause | yt seek [+-]SECONDS | yt stop | yt stat | yt bench URL [FRAMES] | yt bench-opus URL";
const FETCH_WAIT_MS: u32 = 60_000;

pub fn command(args: &str) {
    let mut it = args.split_whitespace();
    match (it.next(), it.next(), it.next()) {
        (Some("stat"), None, None) => kprintln!("{}", remote::stats().unwrap_or_else(|| "yt: no player has run yet".into())),
        (Some("bench"), Some(url), frames) => {
            let frames = frames.and_then(|n| n.parse().ok()).unwrap_or(300);
            run_bench(url, Bench::H264(frames));
        }
        (Some("bench-opus"), Some(url), None) => run_bench(url, Bench::Opus),
        _ => match remote::parse_command(args) {
            // The desktop opens the player for a queued command.
            Ok(cmd) => {
                if !remote::post(cmd) {
                    kprintln!("yt: the player is busy; try again");
                }
            }
            Err(e) => kprintln!("yt: {}\n{}", e, USAGE),
        },
    }
}

fn now_us() -> u64 {
    systimer::now_ticks() / systimer::TICKS_PER_US
}

/// Fetch `url` and wait for the whole body.
fn fetch_body(url: &str) -> Result<Vec<u8>, String> {
    let id = net::fetch(url);
    let start = time::ticks();
    loop {
        match net::poll_fetch(id) {
            FetchState::Pending if time::ticks().wrapping_sub(start) > FETCH_WAIT_MS => {
                return Err("fetch still pending after 60 s (is Wi-Fi up?)".into());
            }
            FetchState::Pending => sched::sleep_ms(50),
            FetchState::Done { status: 200, body, .. } => return Ok(body),
            FetchState::Done { status, .. } => return Err(alloc::format!("HTTP {}", status)),
            FetchState::Failed(e) => return Err(e),
        }
    }
}

#[derive(Clone, Copy)]
enum Bench {
    H264(u32),
    Opus,
}

struct BenchJob {
    kind: Bench,
    data: Vec<u8>,
}

static BENCH_DONE: crate::sync::Semaphore = crate::sync::Semaphore::new(0);

/// Decode a file fetched from `url` on a thread pinned to core 1 (where the
/// player decodes video) and print timing and peak heap use.
fn run_bench(url: &str, kind: Bench) {
    let data = match fetch_body(url) {
        Ok(d) => d,
        Err(e) => return kprintln!("yt bench: {}", e),
    };
    kprintln!("yt bench: {} bytes, decoding on core 1", data.len());
    let job = Box::new(BenchJob { kind, data });
    let _ = crate::heap::take_peaks();
    let (sram0, psram0) = (crate::heap::usage().0, crate::heap::psram_usage().0);
    // The thread takes ownership of the job and signals when it is done.
    sched::spawn_on(Some(1), "ytbench", 2, 160 * 1024, bench_thread, Box::into_raw(job) as usize);
    BENCH_DONE.acquire();
    let (sram_peak, psram_peak) = crate::heap::take_peaks();
    kprintln!(
        "yt bench: peak heap during decode: sram +{} KiB, psram +{} KiB",
        sram_peak.saturating_sub(sram0) / 1024,
        psram_peak.saturating_sub(psram0) / 1024
    );
}

fn bench_thread(arg: usize) {
    // SAFETY: `run_bench` passed a leaked `Box<BenchJob>` and never touches it again.
    let job = unsafe { Box::from_raw(arg as *mut BenchJob) };
    let result = match job.kind {
        Bench::H264(frames) => bench::h264(&job.data, frames, &now_us).map(|r| r.summary(bench::H264_DECODER)),
        Bench::Opus => bench::opus(&job.data, &now_us).map(|r| r.summary()),
    };
    match result {
        Ok(line) => kprintln!("{}", line),
        Err(e) => kprintln!("yt bench: {}", e),
    }
    drop(job);
    BENCH_DONE.release();
}
