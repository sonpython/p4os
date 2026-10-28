/* p4os memory layout for ESP32-P4 rev v1.x.
 *
 * The ESP-IDF v5.5 2nd-stage bootloader requires exactly two segments in the
 * flash-mapped window (0x40000000..0x44000000) and maps both through the
 * cache MMU before jumping to `_start`. Segment 0 must begin with the app
 * descriptor, so read-only data sits at the lowest flash address and code
 * starts on the next 64 KiB MMU page.
 *
 * Writable data lives in HP L2 SRAM. The bootloader's own code occupies
 * 0x4FF2CBD0 and above while it loads us, so loaded sections stay below it.
 */
ENTRY(_start)

MEMORY
{
  DROM : ORIGIN = 0x40000020, LENGTH = 0x1000000 - 0x20
  IROM : ORIGIN = 0x41000020, LENGTH = 0x1000000 - 0x20
  SRAM : ORIGIN = 0x4FF00000, LENGTH = 0x2CBD0
}

STACK_SIZE = 16K;
IRQ_STACK_SIZE = 8K;

SECTIONS
{
  .rodata : ALIGN(16)
  {
    KEEP(*(.app_desc))
    *(.srodata .srodata.*)
    *(.rodata .rodata.*)
  } > DROM

  .text : ALIGN(64)
  {
    KEEP(*(.trap_vector))
    *(.text.start)
    *(.text .text.*)
  } > IROM

  .data : ALIGN(16)
  {
    __global_pointer$ = . + 0x800;
    *(.sdata .sdata.*)
    *(.data .data.*)
  } > SRAM

  .bss (NOLOAD) : ALIGN(16)
  {
    _bss_start = .;
    *(.sbss .sbss.*)
    *(.bss .bss.*)
    *(COMMON)
    . = ALIGN(16);
    _bss_end = .;
  } > SRAM

  .stack (NOLOAD) : ALIGN(16)
  {
    _stack_bottom = .;
    . += STACK_SIZE;
    _stack_top = .;
    _irq_stack_bottom = .;
    . += IRQ_STACK_SIZE;
    _irq_stack_top = .;
  } > SRAM

  _heap_start = .;
  /* After boot the loader's own SRAM is free; stop below the ROM stack. */
  _heap_end = 0x4FF3CFC0;

  /DISCARD/ : { *(.eh_frame .eh_frame_hdr) }
}
