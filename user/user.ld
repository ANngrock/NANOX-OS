/*
 * Link map of NANOX user programs (M2).  Programs are static ELF64
 * executables linked at the bottom of the user half (NX_USER_BASE,
 * abi/nanox/syscall.h); the kernel loads each PT_LOAD segment into fresh
 * pages of the task's own address space with the segment's permissions
 * (docs/m2-kernel.md).  Segments are page aligned so that no page needs two
 * sets of permissions; writable+executable segments are rejected.
 */
OUTPUT_FORMAT("elf64-x86-64")
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);  /* R X */
    rodata PT_LOAD FLAGS(4);  /* R   */
    data   PT_LOAD FLAGS(6);  /* R W */
}

SECTIONS
{
    . = 0x0000008000000000;

    .text : ALIGN(4096) {
        KEEP(*(.text.start))
        *(.text .text.*)
    } :text
    . = ALIGN(4096);

    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    } :rodata
    . = ALIGN(4096);

    .data : ALIGN(4096) {
        *(.data .data.*)
        *(.got .got.*)
    } :data
    .bss : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
