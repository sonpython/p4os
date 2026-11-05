//! Worker threads for apps (`System::spawn_worker`): a boxed closure run on
//! its own low-priority kernel thread, optionally pinned to one core.

use crate::{irq, sched, systimer};
use alloc::boxed::Box;
use alloc::string::String;
use p4os_sdk::{Worker, WorkerJob};

/// Below the net (2), GUI (3) and audio (4) threads, so a busy worker never
/// delays input, network or sound.
const PRIORITY: u8 = 1;

struct KernelWorker;

impl Worker for KernelWorker {
    fn sleep_ms(&self, ms: u32) {
        sched::sleep_ms(ms);
    }

    fn now_us(&self) -> u64 {
        systimer::now_ticks() / systimer::TICKS_PER_US
    }
}

pub fn spawn(name: &'static str, core: Option<usize>, stack_size: usize, job: WorkerJob) -> Result<(), String> {
    if core.is_some_and(|c| c >= irq::HART_COUNT) {
        return Err(alloc::format!("no core {:?}", core));
    }
    // `spawn_on` panics on a full thread table; check first. Exited threads
    // are reaped by the spawn itself, so their slots count as free.
    let free = sched::snapshot()
        .map_err(|_| String::from("scheduler busy"))?
        .iter()
        .filter(|t| t.as_ref().is_none_or(|t| t.state == sched::State::Exited))
        .count();
    if free == 0 {
        return Err("no free thread slot".into());
    }
    // A fat pointer does not fit the thread argument: box it once more.
    let arg = Box::into_raw(Box::new(job)) as usize;
    sched::spawn_on(core, name, PRIORITY, stack_size, run, arg);
    Ok(())
}

fn run(arg: usize) {
    // SAFETY: `spawn` leaked exactly this box for this thread.
    let job = unsafe { Box::from_raw(arg as *mut WorkerJob) };
    job(&KernelWorker);
}
