/*
 * Kernel entry point.
 *
 * Contract with the loader (docs/boot-info.md, "Точка входа ядра"):
 *   long mode, UEFI identity page tables still active, IF = 0,
 *   RDI = physical (= virtual) address of struct nx_boot_info,
 *   RSP = top of the NX_MEM_KERNEL_STACK region.
 *
 * _start records the loader's RSP (checked against the boot info later) and
 * immediately moves to the kernel's own boot stack inside the kernel image,
 * so that nothing runs on loader-provided memory after the kernel installs
 * its own page tables.
 */
    .section .text.entry, "ax"
    .globl _start
    .type _start, @function
_start:
    cli
    cld
    movq %rsp, nx_boot_entry_rsp(%rip)
    leaq __boot_stack_top(%rip), %rsp
    xorl %ebp, %ebp
    call kernel_main
    /* kernel_main does not return; halt forever if it ever does. */
1:  cli
    hlt
    jmp 1b
    .size _start, . - _start

    .section .bss
    .p2align 3
    .globl nx_boot_entry_rsp
nx_boot_entry_rsp:
    .quad 0

    .section .note.GNU-stack, "", @progbits
