//! Reset entry and machine-mode trap entry.
//!
//! The 2nd-stage bootloader jumps to `_start` on HP core 0 in M-mode, with
//! interrupts off and its own stack still in use. Core 1 stays parked in ROM
//! until `smp::start_core1` hands it `_start_hart1`.
//!
//! Traps from user apps (U-mode) are entered on the thread's kernel stack
//! taken from `mscratch`; see `usermode.rs` for the protocol.

use core::arch::global_asm;

global_asm!(
    ".equ FRAME_SIZE, 144",
    /* Must match IRQ_STACK_SIZE in link.x (checked there). */
    ".equ IRQ_STACK_SHIFT, 13",
    /* assist_debug INTR_ENA of core 0; core 1's is 0x80 further
       (stack_guard.rs BASE and CORE_STRIDE). */
    ".equ STACK_GUARD_BASE, 0x3FF06000",
    ".equ STACK_GUARD_CORE_SHIFT, 7",
    r#"
    .section .text.start, "ax"
    .global _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    la sp, _stack_top
    csrci mstatus, 0x8
    /* mscratch >= 0 marks "trapped from U-mode" in _trap_vector. */
    li t0, -1
    csrw mscratch, t0

    la t0, _bss_start
    la t1, _bss_end
1:  bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
2:
    /* mstatus.FS = Initial: without it every float instruction traps. */
    li t0, 0x2000
    csrs mstatus, t0
    fscsr zero

    /* CLIC mode (mtvec[1:0] = 3). Exceptions jump to mtvec[31:6] << 6. */
    la t0, _trap_vector
    ori t0, t0, 3
    csrw mtvec, t0

    call kmain
3:  wfi
    j 3b

    /* Core 1 entry, jumped to by the ROM. bss is already cleared and the
       caches and MMU are shared with core 0, so only per-core CPU state
       needs setting up. */
    .balign 4
    .global _start_hart1
_start_hart1:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    csrci mstatus, 0x8
    li t0, -1
    csrw mscratch, t0
    la sp, _stack1_top
    li t0, 0x2000
    csrs mstatus, t0
    fscsr zero
    la t0, _trap_vector
    ori t0, t0, 3
    csrw mtvec, t0
    call kmain_hart1
4:  wfi
    j 4b

    .section .trap_vector, "ax"
    .balign 64
    .global _trap_vector
_trap_vector:
    /* While a user app runs, mscratch holds its thread's kernel stack top;
       in M-mode it is -1. Swap, and swap back if we came from M-mode, so a
       trap from U-mode never stores through the untrusted user sp. The
       M-mode marker is all ones rather than 0 because the hardware stack
       guard watches sp on every cycle: a transient sp of 0 is below any
       bound, 0xFFFFFFFF is not (only the lower bound is armed). Kernel
       stack tops are below 0x80000000, so the sign tells the two apart. */
    csrrw sp, mscratch, sp
    bgez sp, 10f
    csrrw sp, mscratch, sp
10:
    addi sp, sp, -FRAME_SIZE
    sw x1, 0(sp)
    sw x3, 8(sp)
    sw x4, 12(sp)
    sw x5, 16(sp)
    sw x6, 20(sp)
    sw x7, 24(sp)
    sw x8, 28(sp)
    sw x9, 32(sp)
    sw x10, 36(sp)
    sw x11, 40(sp)
    sw x12, 44(sp)
    sw x13, 48(sp)
    sw x14, 52(sp)
    sw x15, 56(sp)
    sw x16, 60(sp)
    sw x17, 64(sp)
    sw x18, 68(sp)
    sw x19, 72(sp)
    sw x20, 76(sp)
    sw x21, 80(sp)
    sw x22, 84(sp)
    sw x23, 88(sp)
    sw x24, 92(sp)
    sw x25, 96(sp)
    sw x26, 100(sp)
    sw x27, 104(sp)
    sw x28, 108(sp)
    sw x29, 112(sp)
    sw x30, 116(sp)
    sw x31, 120(sp)
    /* Interrupted sp: the user sp parked in mscratch (mcause.MPP = U), or
       the M-mode sp above this frame. mscratch is -1 again afterwards. */
    li t1, -1
    csrrw t0, mscratch, t1
    csrr t1, mcause
    srli t1, t1, 28
    andi t1, t1, 3
    beqz t1, 11f
    addi t0, sp, FRAME_SIZE
11: sw t0, 4(sp)
    csrr t0, mepc
    sw t0, 124(sp)
    csrr t0, mcause
    sw t0, 128(sp)
    csrr t0, mstatus
    sw t0, 132(sp)
    csrr t0, mtval
    sw t0, 136(sp)

    /* Stop this core's hardware stack guard: the handler runs on the
       interrupt stack, below every thread stack (stack_guard.rs). The
       read-back makes sure the write has reached the monitor, and sp below
       depends on it, so sp cannot move to the interrupt stack before the
       monitor is off. */
    csrr t0, mhartid
    slli t0, t0, STACK_GUARD_CORE_SHIFT
    li t1, STACK_GUARD_BASE
    add t1, t1, t0
    sw zero, 0(t1)
    lw t2, 0(t1)

    /* a0 = frame of the interrupted context; the handler returns the
       frame to resume, which may belong to a different thread. */
    /* An app may have changed gp; the kernel needs its own. */
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop

    mv a0, sp
    /* Handlers run on a dedicated per-core stack so thread stacks only
       need room for one saved frame on top of their own usage. Traps never
       nest. Hart h uses the h-th stack down from _irq_stack_top. */
    csrr t0, mhartid
    slli t0, t0, IRQ_STACK_SHIFT
    and t2, t2, zero
    add t0, t0, t2
    la sp, _irq_stack_top
    sub sp, sp, t0
    call trap_handler
    mv sp, a0

    /* Re-arm the stack guard with the bounds of the thread being resumed
       (STACK_GUARD_ENA[hart] is 0 until the core runs a thread). Not when
       resuming U-mode (mcause.MPP = 0): sp is then on the app's own
       stack, outside the thread's kernel stack. */
    csrr t0, mhartid
    slli t1, t0, 2
    la t2, STACK_GUARD_ENA
    add t2, t2, t1
    lw t2, 0(t2)
    lw t1, 128(sp)
    srli t1, t1, 28
    andi t1, t1, 3
    bnez t1, 13f
    li t2, 0
13: slli t0, t0, STACK_GUARD_CORE_SHIFT
    li t1, STACK_GUARD_BASE
    add t1, t1, t0
    sw t2, 0(t1)

    lw t0, 132(sp)
    csrw mstatus, t0
    /* On CLIC, mcause carries the previous interrupt level and MPIE that
       mret restores, so it is part of the saved context. */
    lw t0, 128(sp)
    csrw mcause, t0
    /* Resuming U-mode (mcause.MPP = 0): park this thread's kernel stack
       top, where its next trap frame goes, in mscratch. */
    srli t1, t0, 28
    andi t1, t1, 3
    bnez t1, 12f
    addi t1, sp, FRAME_SIZE
    csrw mscratch, t1
12:
    lw t0, 124(sp)
    csrw mepc, t0
    lw x1, 0(sp)
    lw x3, 8(sp)
    lw x4, 12(sp)
    lw x5, 16(sp)
    lw x6, 20(sp)
    lw x7, 24(sp)
    lw x8, 28(sp)
    lw x9, 32(sp)
    lw x10, 36(sp)
    lw x11, 40(sp)
    lw x12, 44(sp)
    lw x13, 48(sp)
    lw x14, 52(sp)
    lw x15, 56(sp)
    lw x16, 60(sp)
    lw x17, 64(sp)
    lw x18, 68(sp)
    lw x19, 72(sp)
    lw x20, 76(sp)
    lw x21, 80(sp)
    lw x22, 84(sp)
    lw x23, 88(sp)
    lw x24, 92(sp)
    lw x25, 96(sp)
    lw x26, 100(sp)
    lw x27, 104(sp)
    lw x28, 108(sp)
    lw x29, 112(sp)
    lw x30, 116(sp)
    lw x31, 120(sp)
    lw sp, 4(sp)
    mret
"#
);

/// Register state captured on trap entry. `regs[i]` holds `x(i+1)`.
/// The layout is shared with the assembly above (144 bytes, 16-aligned).
#[repr(C)]
pub struct TrapFrame {
    pub regs: [u32; 31],
    pub mepc: u32,
    pub mcause: u32,
    pub mstatus: u32,
    pub mtval: u32,
    _pad: u32,
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == 144);
