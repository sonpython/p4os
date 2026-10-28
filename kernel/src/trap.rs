//! Trap dispatch: interrupts go to registered handlers, `ecall` enters the
//! kernel call interface, exceptions from user apps (syscalls and faults)
//! go to `usermode`, and any other exception is fatal (`fault.rs`).

use crate::boot::TrapFrame;
use crate::{irq, kprintln, sched, usermode};

const ECALL_FROM_M: u32 = 11;

pub fn exception_name(code: u32) -> &'static str {
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

/// mcause bits that identify the trap being serviced (interrupt flag and
/// cause code), as opposed to the MPP/MPIE/MPIL state `mret` restores.
const MCAUSE_TRAP_ID: u32 = (1 << 31) | 0xFFF;

#[unsafe(no_mangle)]
extern "C" fn trap_handler(frame: *mut TrapFrame) -> *mut TrapFrame {
    crate::fault::enter_trap(frame);
    let f = unsafe { &mut *frame };
    let entry_mcause = f.mcause;
    let code = entry_mcause & 0xFFF;
    let resume = if entry_mcause & (1 << 31) != 0 {
        irq::dispatch(code as usize);
        if sched::take_resched() { sched::switch(frame) } else { frame }
    } else if code == ECALL_FROM_M && usermode::is_kernel_call(f.regs[16]) {
        f.mepc += 4;
        usermode::kernel_call(frame)
    } else if code == ECALL_FROM_M {
        f.mepc += 4;
        match f.regs[16] {
            sched::call::YIELD => {}
            n => kprintln!("*** unknown kernel call {}", n),
        }
        sched::take_resched();
        sched::switch(frame)
    } else if usermode::is_user_frame(f) {
        // Syscall or fault in a user app: never fatal for the kernel.
        usermode::user_exception(frame)
    } else {
        fatal_exception(f)
    };

    // The CLIC ends service of the current interrupt on `mret` based on the
    // cause in mcause. When resuming a different thread, keep this trap's
    // cause and take only the previous-privilege/level bits from the frame,
    // otherwise the interrupt line stays "in service" and never fires again.
    let r = unsafe { &mut *resume };
    r.mcause = (r.mcause & !MCAUSE_TRAP_ID) | (entry_mcause & MCAUSE_TRAP_ID);
    usermode::before_resume(r);
    resume
}

fn fatal_exception(f: &TrapFrame) -> ! {
    crate::fault::exception(f, exception_name(f.mcause & 0xFFF))
}
