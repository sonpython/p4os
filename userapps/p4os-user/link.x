/* Layout of a p4os user app (position-independent, linked at 0).
 *
 * The kernel loads the image at any 128-byte aligned address and grants
 * read+execute on [0, __rx_end) and read+write on [__rx_end, end of stack)
 * through PMP, so everything writable, including data that needs load-time
 * relocation, must follow __rx_end. The PMP granule on the P4 is 128 bytes.
 * scripts/mkapp.py checks these rules when it packs the image.
 */
ENTRY(_start)

STACK_SIZE = 8K;

SECTIONS
{
  . = 0;
  .text : ALIGN(4)
  {
    KEEP(*(.text.start))
    *(.text .text.*)
  }

  .rodata : ALIGN(16)
  {
    *(.srodata .srodata.*)
    *(.rodata .rodata.*)
  }

  . = ALIGN(128);
  __rx_end = .;

  .data : ALIGN(128)
  {
    *(.data.rel.ro .data.rel.ro.*)
    *(.got .got.*)
    *(.sdata .sdata.*)
    *(.data .data.*)
  }

  .bss (NOLOAD) : ALIGN(16)
  {
    *(.sbss .sbss.*)
    *(.bss .bss.*)
    *(COMMON)
  }
  . = ALIGN(128);
  __mem_end = .;
  __stack_size = STACK_SIZE;

  /* Dynamic linking metadata: read by the packer, never loaded. */
  .dynamic : { *(.dynamic) }
  .dynsym : { *(.dynsym) }
  .dynstr : { *(.dynstr) }
  .hash : { *(.hash) }
  .gnu.hash : { *(.gnu.hash) }
  .rela.dyn : { *(.rela.dyn .rela.*) }

  /DISCARD/ : { *(.eh_frame .eh_frame_hdr) }
}
