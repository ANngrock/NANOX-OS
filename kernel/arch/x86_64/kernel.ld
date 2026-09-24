/*
 * NANOX kernel link map.  The kernel image is linked at its physical load
 * address (VMA == LMA); the kernel's own page tables map it at the same
 * virtual address with per-section permissions (docs/m1-kernel.md):
 *   .text R-X, .rodata R--, .data/.bss RW-, stacks RW- with an unmapped
 *   guard page below each stack.
 */
OUTPUT_FORMAT("elf64-x86-64")
ENTRY(_start)

NX_KERNEL_PHYS_BASE = 0x200000;
NX_BOOT_STACK_SIZE = 64K;
NX_IST_STACK_SIZE = 16K;

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
        __text_start = .;
        KEEP(*(.text.entry))
        *(.text .text.*)
    } :text
    . = ALIGN(4096);
    __text_end = .;

    .rodata : ALIGN(4096) {
        __rodata_start = .;
        *(.rodata .rodata.*)
    } :rodata
    . = ALIGN(4096);
    __rodata_end = .;

    .data : ALIGN(4096) {
        __data_start = .;
        *(.data .data.*)
    } :data

    .bss : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    . = ALIGN(4096);
    __data_end = .;

    /* Each stack is preceded by one guard page that is never mapped. */
    .stacks (NOLOAD) : ALIGN(4096) {
        __stacks_start = .;
        __boot_stack_guard = .;  . += 4096;
        __boot_stack_bottom = .; . += NX_BOOT_STACK_SIZE;
        __boot_stack_top = .;
        __ist_df_guard = .;      . += 4096;
        __ist_df_bottom = .;     . += NX_IST_STACK_SIZE;
        __ist_df_top = .;
        __ist_nmi_guard = .;     . += 4096;
        __ist_nmi_bottom = .;    . += NX_IST_STACK_SIZE;
        __ist_nmi_top = .;
        __ist_mc_guard = .;      . += 4096;
        __ist_mc_bottom = .;     . += NX_IST_STACK_SIZE;
        __ist_mc_top = .;
        __stacks_end = .;
    } :data

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
