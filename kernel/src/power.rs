//! Power management for battery operation.
//!
//! - Idle: both cores halt in `wfi` with the core clock gated
//!   (HP_SYS CPU_WAITI_CONF), and the 1 kHz tick stops while every core is
//!   idle (tickless, see `time.rs`). Time in `wfi`, wake-ups and the cycles
//!   counted while halted are recorded per core as proxy metrics.
//! - CPU frequency scaling: a governor thread samples per-core idle time
//!   and moves CPU_CLK between 360/180/90/40 MHz (`clock.rs`) within the
//!   bounds of the power mode, screen state and active users (audio).
//!   `boost` raises it at once for interaction and network work.
//! - Policy (`power.mode`): performance, balanced or battery. It also sets
//!   the Wi-Fi co-processor's power save mode and the polling intervals of
//!   the network and touch loops.
//! - Display: the GUI thread dims the backlight and turns the panel off
//!   after the `power.dim_s` / `power.off_s` timeouts read from here, and
//!   forwards screen-off / wake to the desktop (lock screen).

use crate::irq::HART_COUNT;
use crate::spin::SpinLock;
use crate::{clock, config, irq, kprintln, mmio, sched, soc, systimer, time, wifi};
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use p4os_sdk::config::{self as keys, PowerMode};

/// HP_SYS CPU_WAITI_CONF: bit 0 set (reset default) keeps the core clock
/// running in `wfi`; clear, the clock is gated after `delay` cycles.
const CPU_WAITI_CONF: usize = soc::HP_SYS + 0x118;
const WAIT_MODE_FORCE_ON: u32 = 1 << 0;

/// Level indices into `clock::LEVELS_MHZ`: 0 = 360 MHz ... 3 = 40 MHz.
const LEVEL_COUNT: usize = clock::LEVELS_MHZ.len();
/// Slowest level while the panel is refreshed. Down to 180 MHz CPU the
/// memory and system buses keep their full 180 MHz, so the display DMA's
/// PSRAM bandwidth is untouched; at 90 MHz they run at 90 MHz, which
/// measured no underruns (40 MHz did underrun). Balanced keeps the margin.
const DISPLAY_SLOWEST_BALANCED: usize = 1;
const DISPLAY_SLOWEST_BATTERY: usize = 2;
/// Slowest level while audio plays (decoding runs in the audio thread).
const AUDIO_SLOWEST: usize = 1;
/// Governor sampling windows.
const WINDOW_SCREEN_ON_MS: u32 = 100;
const WINDOW_SCREEN_OFF_MS: u32 = 500;
/// Consecutive calm windows before stepping one level down.
const CALM_WINDOWS: u32 = 3;
const WIFI_PS_RETRY_MS: u32 = 60_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    On = 0,
    Dim = 1,
    Off = 2,
}

impl Screen {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Screen::Dim,
            2 => Screen::Off,
            _ => Screen::On,
        }
    }
}

static MODE: AtomicU8 = AtomicU8::new(PowerMode::Balanced as u8);
static DIM_S: AtomicU32 = AtomicU32::new(keys::DEFAULT_DIM_S);
static OFF_S: AtomicU32 = AtomicU32::new(keys::DEFAULT_OFF_S);
static SCREEN: AtomicU8 = AtomicU8::new(Screen::On as u8);
/// Screen state asked for from the shell, applied by the GUI thread
/// (0 = none, otherwise `Screen as u8 + 1`).
static SCREEN_REQUEST: AtomicU8 = AtomicU8::new(0);
static TICKLESS: AtomicBool = AtomicBool::new(false);
/// Tick until which the CPU runs at the fastest allowed level.
static BOOST_UNTIL: AtomicU32 = AtomicU32::new(0);
/// Fixed frequency set from the shell (0 = governor decides).
static PINNED_MHZ: AtomicU32 = AtomicU32::new(0);
/// WFI clock gating override from the shell: 0 = by mode, 1 = on, 2 = off.
static GATE_OVERRIDE: AtomicU8 = AtomicU8::new(0);
static LEVEL: AtomicUsize = AtomicUsize::new(0);

/// Per core: systimer ticks spent in `wfi` (not tracked in performance
/// mode) and `wfi` exits.
static WFI_TICKS: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];
static WAKEUPS: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];
/// Low 32 bits of the systimer when the core entered the `wfi` it is in
/// now, or 0 while it runs. A core can halt for seconds (core 1 has no
/// tick), so samples add the open stretch to `WFI_TICKS`.
static WFI_SINCE: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];

/// Frequency residency, guarded so level changes and reads agree.
struct Residency {
    since: u64,
    ticks: [u64; LEVEL_COUNT],
}

static RESIDENCY: SpinLock<Residency> = SpinLock::new(Residency { since: 0, ticks: [0; LEVEL_COUNT] });

pub fn mode() -> PowerMode {
    keys::POWER_MODES[MODE.load(Ordering::Relaxed) as usize % keys::POWER_MODES.len()]
}

/// Bumped on every CPU frequency change, so an idle core can tell whether
/// the clock stayed constant while it was halted.
static FREQ_EPOCH: AtomicU32 = AtomicU32::new(0);

fn read_mcycle64() -> u64 {
    loop {
        let (hi, lo, hi2): (u32, u32, u32);
        unsafe { asm!("csrr {0}, mcycleh", "csrr {1}, mcycle", "csrr {2}, mcycleh", out(reg) hi, out(reg) lo, out(reg) hi2) };
        if hi == hi2 {
            return ((hi as u64) << 32) | lo as u64;
        }
    }
}

/// One pass of a core's idle thread.
///
/// Performance mode keeps the plain `wfi` loop, so interrupt latency from
/// idle is what it was without power management; only wake-ups are
/// counted.
///
/// Otherwise interrupts are masked from the tickless decision until the
/// core wakes, so a wake-up cannot slip in between (`wfi` still resumes on
/// a pending interrupt, whose handler runs once they are unmasked). The
/// halted time comes from `mcycle`, which keeps counting in `wfi` on this
/// chip even with the WAITI clock gate enabled: a CSR read after waking
/// instead of a systimer access on the interrupt latency path. If the
/// clock changed meanwhile, the systimer is read instead.
pub fn idle() {
    let hart = irq::hart_id();
    if !TICKLESS.load(Ordering::Relaxed) {
        unsafe { asm!("wfi") };
        WAKEUPS[hart].fetch_add(1, Ordering::Relaxed);
        return;
    }
    let was_enabled = irq::disable_global();
    sched::when_all_idle(time::stop);
    let epoch = FREQ_EPOCH.load(Ordering::Relaxed);
    let mhz = clock::cpu_mhz().max(1) as u64;
    let t0 = systimer::now_ticks();
    WFI_SINCE[hart].store((t0 as u32).max(1), Ordering::Relaxed);
    let c0 = read_mcycle64();
    unsafe { asm!("wfi") };
    let c1 = read_mcycle64();
    let measured = (FREQ_EPOCH.load(Ordering::Relaxed) != epoch).then(|| systimer::now_ticks() - t0);
    // Close the open stretch first so a sampler never counts it twice
    // (it may miss part of it instead).
    WFI_SINCE[hart].store(0, Ordering::Relaxed);
    if was_enabled {
        irq::enable_global();
    }
    let halted = measured.unwrap_or_else(|| (c1 - c0) * systimer::TICKS_PER_US / mhz);
    WFI_TICKS[hart].fetch_add(halted as u32, Ordering::Relaxed);
    WAKEUPS[hart].fetch_add(1, Ordering::Relaxed);
}

fn set_level(level: usize) {
    let level = level.min(LEVEL_COUNT - 1);
    let mut r = RESIDENCY.lock();
    let current = LEVEL.load(Ordering::Relaxed);
    if current == level {
        return;
    }
    let now = systimer::now_ticks();
    r.ticks[current] += now - r.since;
    r.since = now;
    FREQ_EPOCH.fetch_add(1, Ordering::Relaxed);
    clock::set_cpu_mhz(clock::LEVELS_MHZ[level]);
    FREQ_EPOCH.fetch_add(1, Ordering::Relaxed);
    LEVEL.store(level, Ordering::Relaxed);
}

fn level_of(mhz: u32) -> Option<usize> {
    clock::LEVELS_MHZ.iter().position(|&m| m == mhz)
}

/// (fastest, slowest) level the policy allows right now.
fn bounds() -> (usize, usize) {
    let screen_off = screen() == Screen::Off;
    let (fast, mut slow) = match mode() {
        PowerMode::Performance => (0, 0),
        PowerMode::Balanced => (0, if screen_off { 2 } else { DISPLAY_SLOWEST_BALANCED }),
        PowerMode::Battery => (1, if screen_off { 3 } else { DISPLAY_SLOWEST_BATTERY }),
    };
    if crate::audio::is_active() {
        slow = slow.min(AUDIO_SLOWEST);
    }
    (fast, slow.max(fast))
}

fn boosted() -> bool {
    (BOOST_UNTIL.load(Ordering::Relaxed).wrapping_sub(time::ticks()) as i32) > 0
}

/// Run at the fastest allowed clock for the next `ms` milliseconds (user
/// input, network transfers). Cheap when already boosted.
pub fn boost(ms: u32) {
    BOOST_UNTIL.store(time::ticks().wrapping_add(ms), Ordering::Relaxed);
    let fast = bounds().0;
    if PINNED_MHZ.load(Ordering::Relaxed) == 0 && LEVEL.load(Ordering::Relaxed) > fast {
        set_level(fast);
    }
}

pub fn screen() -> Screen {
    Screen::from_u8(SCREEN.load(Ordering::Relaxed))
}

/// The GUI reports the screen state it applied.
pub fn set_screen(s: Screen) {
    SCREEN.store(s as u8, Ordering::Relaxed);
    if s != Screen::Off {
        // Leaving the off state needs the display's clock floor at once.
        let slow = bounds().1;
        if PINNED_MHZ.load(Ordering::Relaxed) == 0 && LEVEL.load(Ordering::Relaxed) > slow {
            set_level(slow);
        }
    }
}

/// Ask the GUI to turn the screen on (also restarting the inactivity
/// timers) and wait up to `timeout_ms` for it. For shell commands that need
/// a lit panel, such as `bl` used by the camera checks.
pub fn wake_screen_blocking(timeout_ms: u32) {
    if screen() == Screen::On {
        SCREEN_REQUEST.store(Screen::On as u8 + 1, Ordering::Relaxed);
        return;
    }
    SCREEN_REQUEST.store(Screen::On as u8 + 1, Ordering::Relaxed);
    let start = time::ticks();
    while screen() != Screen::On && time::ticks().wrapping_sub(start) < timeout_ms {
        sched::sleep_ms(10);
    }
}

/// Screen state requested from the shell, consumed by the GUI thread.
pub fn take_screen_request() -> Option<Screen> {
    match SCREEN_REQUEST.swap(0, Ordering::Relaxed) {
        0 => None,
        v => Some(Screen::from_u8(v - 1)),
    }
}

/// Inactivity timeouts in milliseconds (`None` = never).
pub fn dim_after_ms() -> Option<u64> {
    let s = DIM_S.load(Ordering::Relaxed);
    (s > 0).then_some(s as u64 * 1000)
}

pub fn off_after_ms() -> Option<u64> {
    let s = OFF_S.load(Ordering::Relaxed);
    (s > 0).then_some(s as u64 * 1000)
}

/// How often the GUI polls the touch panel while the screen is off: the
/// GT911's interrupt line is not wired, so this bounds wake-up latency.
pub fn touch_poll_off_ms() -> u32 {
    match mode() {
        PowerMode::Performance => 50,
        PowerMode::Balanced => 100,
        PowerMode::Battery => 200,
    }
}

/// How often the network thread polls the co-processor with no transfer
/// in progress.
pub fn net_idle_poll_ms() -> u32 {
    match (mode(), screen() == Screen::Off) {
        (PowerMode::Performance, _) | (PowerMode::Balanced, false) => 10,
        (PowerMode::Balanced, true) | (PowerMode::Battery, false) => 50,
        (PowerMode::Battery, true) => 100,
    }
}

/// ESP-Hosted `wifi_ps_type_t`: none, minimum modem sleep (wakes every
/// DTIM), maximum modem sleep (wakes every listen interval).
fn wifi_ps_for(m: PowerMode) -> i32 {
    match m {
        PowerMode::Performance => 0,
        PowerMode::Balanced => 1,
        PowerMode::Battery => 2,
    }
}

/// Reload `power.*` settings from the config store (boot, and whenever an
/// app or the shell changes one).
pub fn load_config() {
    let m = config::get(keys::POWER_MODE).and_then(|v| keys::parse_power_mode(&v)).unwrap_or_default();
    MODE.store(m as u8, Ordering::Relaxed);
    let dim = config::get(keys::POWER_DIM_S).and_then(|v| keys::parse_timeout(&v)).unwrap_or(keys::DEFAULT_DIM_S);
    let off = config::get(keys::POWER_OFF_S).and_then(|v| keys::parse_timeout(&v)).unwrap_or(keys::DEFAULT_OFF_S);
    DIM_S.store(dim, Ordering::Relaxed);
    OFF_S.store(off, Ordering::Relaxed);
    apply_mode();
}

/// Settings that follow the mode directly and are cheap to apply.
fn apply_mode() {
    TICKLESS.store(mode() != PowerMode::Performance, Ordering::Relaxed);
    // ESP-IDF notes that with the gate enabled a JTAG debugger cannot
    // reach memory while the core sits in `wfi`, so only the battery
    // policy turns it on by default.
    let gate = match GATE_OVERRIDE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => mode() == PowerMode::Battery,
    };
    if gate {
        mmio::clear_bits(CPU_WAITI_CONF, WAIT_MODE_FORCE_ON);
    } else {
        mmio::set_bits(CPU_WAITI_CONF, WAIT_MODE_FORCE_ON);
    }
    let (fast, slow) = bounds();
    let level = LEVEL.load(Ordering::Relaxed);
    if PINNED_MHZ.load(Ordering::Relaxed) == 0 && !(fast..=slow).contains(&level) {
        set_level(level.clamp(fast, slow));
    }
}

/// Start power management. Call after `config::init` and the 360 MHz
/// switch, before the scheduler starts.
pub fn init() {
    LEVEL.store(level_of(clock::cpu_mhz()).unwrap_or(0), Ordering::Relaxed);
    RESIDENCY.lock().since = systimer::now_ticks();
    load_config();
    kprintln!("power: {} mode", keys::power_mode_value(mode()));
    sched::spawn("power", 5, 8 * 1024, governor, 0);
}

/// Idle counters of every core at one instant.
#[derive(Clone, Copy)]
struct Sample {
    at: u64,
    /// Halted time including a `wfi` still in progress.
    wfi: [u32; HART_COUNT],
    wakeups: [u32; HART_COUNT],
}

impl Sample {
    fn take() -> Self {
        let at = systimer::now_ticks();
        Sample {
            at,
            wfi: core::array::from_fn(|h| {
                let done = WFI_TICKS[h].load(Ordering::Relaxed);
                match WFI_SINCE[h].load(Ordering::Relaxed) {
                    0 => done,
                    since => done.wrapping_add((at as u32).wrapping_sub(since)),
                }
            }),
            wakeups: core::array::from_fn(|h| WAKEUPS[h].load(Ordering::Relaxed)),
        }
    }

    /// Share of the interval core `hart` spent outside `wfi`.
    fn busy(&self, earlier: &Sample, hart: usize) -> f32 {
        let span = (self.at - earlier.at).max(1) as f32;
        let halted = self.wfi[hart].wrapping_sub(earlier.wfi[hart]) as f32;
        (1.0 - halted / span).clamp(0.0, 1.0)
    }

    /// Busy share of the busiest online core.
    fn max_busy(&self, earlier: &Sample) -> f32 {
        (0..HART_COUNT).filter(|&h| sched::is_online(h)).map(|h| self.busy(earlier, h)).fold(0.0, f32::max)
    }
}

/// Next level from the busiest core's load over the last window: jump to
/// the fastest allowed level when saturated, step up when getting busy,
/// step down only after several calm windows and only if the load would
/// still fit at the slower clock.
fn next_level(level: usize, busy: f32, fast: usize, slow: usize, calm: &mut u32) -> usize {
    let next = if boosted() || busy > 0.75 {
        *calm = 0;
        fast
    } else if busy > 0.5 {
        *calm = 0;
        level.saturating_sub(1)
    } else if level < slow
        && busy * (clock::LEVELS_MHZ[level] as f32) / (clock::LEVELS_MHZ[level + 1] as f32) < 0.4
    {
        *calm += 1;
        if *calm >= CALM_WINDOWS {
            *calm = 0;
            level + 1
        } else {
            level
        }
    } else {
        *calm = 0;
        level
    };
    next.clamp(fast, slow)
}

/// Bring the co-processor's power save mode in line with the policy.
/// Retried at most once a minute if the slave refuses.
fn sync_wifi_ps(last_try: &mut Option<(i32, u32)>) {
    if !wifi::is_connected() {
        return;
    }
    let want = wifi_ps_for(mode());
    if wifi::power_save() == Some(want) {
        return;
    }
    if let Some((tried, at)) = *last_try
        && tried == want
        && time::ticks().wrapping_sub(at) < WIFI_PS_RETRY_MS
    {
        return;
    }
    *last_try = Some((want, time::ticks()));
    match wifi::set_power_save(want) {
        Ok(()) => kprintln!("power: Wi-Fi power save {}", ["off", "modem sleep (min)", "modem sleep (max)"][want as usize]),
        Err(e) => kprintln!("power: Wi-Fi power save not applied: {}", e),
    }
}

fn governor(_: usize) {
    let mut last = Sample::take();
    let mut calm = 0;
    let mut wifi_try = None;
    loop {
        let window = if screen() == Screen::Off { WINDOW_SCREEN_OFF_MS } else { WINDOW_SCREEN_ON_MS };
        sched::sleep_ms(window);
        let now = Sample::take();
        let busy = now.max_busy(&last);
        last = now;
        match PINNED_MHZ.load(Ordering::Relaxed) {
            0 => {
                let (fast, slow) = bounds();
                set_level(next_level(LEVEL.load(Ordering::Relaxed), busy, fast, slow, &mut calm));
            }
            mhz => set_level(level_of(mhz).unwrap_or(0)),
        }
        sync_wifi_ps(&mut wifi_try);
    }
}

/// Time at each frequency level since boot, in systimer ticks.
fn residency() -> [u64; LEVEL_COUNT] {
    let r = RESIDENCY.lock();
    let mut t = r.ticks;
    t[LEVEL.load(Ordering::Relaxed)] += systimer::now_ticks() - r.since;
    t
}

fn persist(key: &str, value: &str) {
    match config::set(key, value) {
        Ok(()) => load_config(),
        Err(e) => kprintln!("power: saving {} failed: {:?}", key, e),
    }
}

fn print_status() {
    let m = mode();
    let pinned = PINNED_MHZ.load(Ordering::Relaxed);
    kprintln!(
        "mode {}, cpu {} MHz{}, screen {:?}, tickless {}, wfi clock gating {}",
        keys::power_mode_value(m),
        clock::cpu_mhz(),
        if pinned != 0 { " (pinned)" } else { "" },
        screen(),
        TICKLESS.load(Ordering::Relaxed),
        mmio::read(CPU_WAITI_CONF) & WAIT_MODE_FORCE_ON == 0,
    );
    kprintln!(
        "dim after {}, screen off after {}, wifi power save {:?}",
        keys::timeout_label(DIM_S.load(Ordering::Relaxed)),
        keys::timeout_label(OFF_S.load(Ordering::Relaxed)),
        wifi::power_save()
    );
    let mv = crate::battery::millivolts();
    let (raw, cal) = crate::battery::diagnostics();
    match mv {
        Some(mv) => kprintln!("battery {}.{:02} V (adc raw {}, {:?})", mv / 1000, mv % 1000 / 10, raw, cal),
        None => kprintln!("battery: not measured (adc calibration {:?})", cal),
    }
}

/// `power stat [S]`: sample the idle counters over S seconds (1..=10).
fn print_stat(secs: u32) {
    let (irqs0, stops0) = time::stats();
    let res0 = residency();
    let a = Sample::take();
    sched::sleep_ms(secs * 1000);
    let b = Sample::take();
    let res1 = residency();
    let (irqs1, stops1) = time::stats();
    let span = (b.at - a.at) as f32;
    let secs_f = span / (systimer::TICKS_PER_US as f32 * 1e6);
    kprintln!("over {:.2} s at {} MHz now:", secs_f, clock::cpu_mhz());
    let tracked = TICKLESS.load(Ordering::Relaxed);
    for h in 0..HART_COUNT {
        let halted = b.wfi[h].wrapping_sub(a.wfi[h]) as f32;
        let wakeups = b.wakeups[h].wrapping_sub(a.wakeups[h]);
        if tracked {
            kprintln!("  core {}: wfi {:5.1}%  wakeups {:6.1}/s", h, 100.0 * halted / span, wakeups as f32 / secs_f);
        } else {
            // Performance mode does not time `wfi`; show the tick-based load.
            let idle = 100.0 * (1.0 - sched::cpu_load(h));
            kprintln!("  core {}: idle {:5.1}% (tick-based)  wakeups {:6.1}/s", h, idle, wakeups as f32 / secs_f);
        }
    }
    kprintln!(
        "  tick irqs {:.1}/s, tickless stops {:.1}/s",
        irqs1.wrapping_sub(irqs0) as f32 / secs_f,
        stops1.wrapping_sub(stops0) as f32 / secs_f
    );
    let total: u64 = (0..LEVEL_COUNT).map(|i| res1[i] - res0[i]).sum::<u64>().max(1);
    let mut line = alloc::string::String::from("  residency:");
    for (i, mhz) in clock::LEVELS_MHZ.iter().enumerate() {
        let pct = 100.0 * (res1[i] - res0[i]) as f32 / total as f32;
        line += &alloc::format!(" {} MHz {:.1}%", mhz, pct);
    }
    kprintln!("{}", line);
}

/// Shell: `power [stat [S] | mode M | freq auto|MHz | screen on|dim|off |
/// dim S | off S | gate auto|on|off]`.
pub fn command(args: &str) {
    let mut it = args.split_whitespace();
    match (it.next(), it.next()) {
        (None, _) => print_status(),
        (Some("stat"), n) => match n.map_or(Ok(5), str::parse::<u32>) {
            Ok(s) if (1..=10).contains(&s) => print_stat(s),
            _ => kprintln!("usage: power stat [1..10]"),
        },
        (Some("mode"), Some(m)) => match keys::parse_power_mode(m) {
            Some(_) => persist(keys::POWER_MODE, m),
            None => kprintln!("modes: performance balanced battery"),
        },
        (Some("freq"), Some("auto")) => PINNED_MHZ.store(0, Ordering::Relaxed),
        (Some("freq"), Some(f)) => match f.parse::<u32>().ok().and_then(level_of) {
            Some(level) => {
                PINNED_MHZ.store(clock::LEVELS_MHZ[level], Ordering::Relaxed);
                set_level(level);
                kprintln!("cpu {} MHz, measured ~{} MHz", clock::cpu_mhz(), crate::measure_cpu_mhz());
            }
            None => kprintln!("usage: power freq auto|360|180|90|40"),
        },
        (Some("screen"), Some(s)) => {
            let req = match s {
                "on" => Screen::On,
                "dim" => Screen::Dim,
                "off" => Screen::Off,
                _ => return kprintln!("usage: power screen on|dim|off"),
            };
            SCREEN_REQUEST.store(req as u8 + 1, Ordering::Relaxed);
        }
        (Some(which @ ("dim" | "off")), Some(v)) => match keys::parse_timeout(v) {
            Some(_) => persist(if which == "dim" { keys::POWER_DIM_S } else { keys::POWER_OFF_S }, v),
            None => kprintln!("usage: power {} SECONDS (0 = never, max {})", which, keys::MAX_TIMEOUT_S),
        },
        (Some("gate"), Some(g)) => {
            let v = match g {
                "auto" => 0,
                "on" => 1,
                "off" => 2,
                _ => return kprintln!("usage: power gate auto|on|off"),
            };
            GATE_OVERRIDE.store(v, Ordering::Relaxed);
            apply_mode();
        }
        _ => kprintln!("usage: power [stat [S] | mode M | freq auto|MHz | screen on|dim|off | dim S | off S | gate auto|on|off]"),
    }
}
