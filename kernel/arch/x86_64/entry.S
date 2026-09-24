/*
 * Kernel entry point (M0).
 *
 * Contract with the loader (docs/boot-info.md, "Точка входа ядра"):
 *   long mode, UEFI identity page tables still active, IF = 0,
 *   RDI = physical (= virtual) address of struct nx_boot_info,
 *   RSP = top of the NX_MEM_KERNEL_STACK region.
 */
    .section .text.entry, "ax"
    .globl _start
    .type _start, @function
_start:
    cli
    cld
    xorl %ebp, %ebp
    andq $-16, %rsp
    call kernel_main
    /* kernel_main does not return; halt forever if it ever does. */
1:  cli
    hlt
    jmp 1b
    .size _start, . - _start

    .section .note.GNU-stack, "", @progbits
