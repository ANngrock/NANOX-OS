/*
 * Entry point of NANOX user programs (M2).  The kernel enters ring 3 here
 * with RSP = NX_USER_STACK_TOP (16-byte aligned) and the four program
 * arguments in RDI, RSI, RDX, RCX.  umain() receives them as its C
 * arguments (RCX is the fourth argument register of the SysV ABI); its
 * return value becomes the exit code.
 */
    .section .text.start, "ax"
    .globl _start
    .type _start, @function
_start:
    xorl %ebp, %ebp          /* outermost frame */
    call umain               /* the call leaves RSP 16-byte aligned + 8, as the ABI requires */
    movq %rax, %rdi
    movl $2, %eax            /* NX_SYS_EXIT */
    int $0x80
    ud2                      /* exit does not return */
    .size _start, . - _start

    .section .note.GNU-stack, "", @progbits
