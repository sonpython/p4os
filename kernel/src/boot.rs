//! Reset entry and machine-mode trap entry.
//!
//! The 2nd-stage bootloader jumps to `_start` on HP core 0 in M-mode, with
//! interrupts off and its own stack still in use. Core 1 stays parked in ROM
//! until `smp::start_core1` hands it `_start_hart1`.

use core::arch::global_asm;

global_asm!(
    ".equ FRAME_SIZE, 144",
    /* Must match IRQ_STACK_SIZE in link.x (checked there). */
    ".equ IRQ_STACK_SHIFT, 13",
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
    addi t0, sp, FRAME_SIZE
    sw t0, 4(sp)
    csrr t0, mepc
    sw t0, 124(sp)
    csrr t0, mcause
    sw t0, 128(sp)
    csrr t0, mstatus
    sw t0, 132(sp)
    csrr t0, mtval
    sw t0, 136(sp)

    /* a0 = frame of the interrupted context; the handler returns the
       frame to resume, which may belong to a different thread. */
    mv a0, sp
    /* Handlers run on a dedicated per-core stack so thread stacks only
       need room for one saved frame on top of their own usage. Traps never
       nest. Hart h uses the h-th stack down from _irq_stack_top. */
    csrr t0, mhartid
    slli t0, t0, IRQ_STACK_SHIFT
    la sp, _irq_stack_top
    sub sp, sp, t0
    call trap_handler
    mv sp, a0

    lw t0, 132(sp)
    csrw mstatus, t0
    /* On CLIC, mcause carries the previous interrupt level and MPIE that
       mret restores, so it is part of the saved context. */
    lw t0, 128(sp)
    csrw mcause, t0
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
