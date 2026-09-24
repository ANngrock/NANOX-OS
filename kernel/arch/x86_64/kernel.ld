/*
 * NANOX kernel link map (M0).  The kernel runs identity mapped on the UEFI
 * page tables, so VMA == LMA == physical address.  NX_KERNEL_PHYS_BASE is
 * part of the documented memory layout (docs/boot-info.md).
 */
OUTPUT_FORMAT("elf64-x86-64")
ENTRY(_start)

NX_KERNEL_PHYS_BASE = 0x200000;

PHDRS
{
    text   PT_LOAD FLAGS(5);  /* R X */
    rodata PT_LOAD FLAGS(4);  /* R   */
    data   PT_LOAD FLAGS(6);  /* R W */
}

SECTIONS
{
    . = NX_KERNEL_PHYS_BASE;
    __kernel_start = .;

    .text : ALIGN(4096) {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    } :rodata

    .data : ALIGN(4096) {
        *(.data .data.*)
    } :data

    .bss : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
