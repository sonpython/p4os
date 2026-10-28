//! Trap dispatch: interrupts go to registered handlers, `ecall` enters the
//! kernel call interface, and any other exception is fatal.

use crate::boot::TrapFrame;
use crate::{irq, kprintln, sched};

const ECALL_FROM_M: u32 = 11;

fn exception_name(code: u32) -> &'static str {
    match code {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store address misaligned",
        7 => "store access fault",
        8 => "ecall from U-mode",
        11 => "ecall from M-mode",
        _ => "unknown",
    }
}

const REG_NAMES: [&str; 31] = [
    "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5",
    "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5",
    "t6",
];

/// mcause bits that identify the trap being serviced (interrupt flag and
/// cause code), as opposed to the MPP/MPIE/MPIL state `mret` restores.
const MCAUSE_TRAP_ID: u32 = (1 << 31) | 0xFFF;

#[unsafe(no_mangle)]
extern "C" fn trap_handler(frame: *mut TrapFrame) -> *mut TrapFrame {
    let f = unsafe { &mut *frame };
    let entry_mcause = f.mcause;
    let code = entry_mcause & 0xFFF;
    let resume = if entry_mcause & (1 << 31) != 0 {
        irq::dispatch(code as usize);
        if sched::take_resched() { sched::switch(frame) } else { frame }
    } else if code == ECALL_FROM_M {
        f.mepc += 4;
        match f.regs[16] {
            sched::call::YIELD => {}
            n => kprintln!("*** unknown kernel call {}", n),
        }
        sched::take_resched();
        sched::switch(frame)
    } else {
        fatal_exception(f)
    };

    // The CLIC ends service of the current interrupt on `mret` based on the
    // cause in mcause. When resuming a different thread, keep this trap's
    // cause and take only the previous-privilege/level bits from the frame,
    // otherwise the interrupt line stays "in service" and never fires again.
    let r = unsafe { &mut *resume };
    r.mcause = (r.mcause & !MCAUSE_TRAP_ID) | (entry_mcause & MCAUSE_TRAP_ID);
    resume
}

fn fatal_exception(f: &TrapFrame) -> ! {
    let code = f.mcause & 0xFFF;
    kprintln!();
    kprintln!("*** exception {} on hart {}: {}", code, irq::hart_id(), exception_name(code));
    kprintln!("mepc={:#010x} mtval={:#010x} mcause={:#010x}", f.mepc, f.mtval, f.mcause);
    for (i, chunk) in f.regs.chunks(4).enumerate() {
        for (j, value) in chunk.iter().enumerate() {
            crate::kprint!("{:>4}={:#010x} ", REG_NAMES[i * 4 + j], value);
        }
        kprintln!();
    }
    crate::halt()
}
