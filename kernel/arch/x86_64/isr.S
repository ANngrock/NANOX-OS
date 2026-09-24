/*
 * Interrupt entry stubs: one 16-byte stub per vector at nx_isr_stubs +
 * 16 * vector.  Vectors 8, 10-14, 17, 21, 29, 30 get an error code from the
 * CPU; all others push a zero so that the frame layout is uniform
 * (struct nx_trap_frame in trap.h).
 */
    .section .text, "ax"
    .globl nx_isr_stubs
    .p2align 4
nx_isr_stubs:
    .set vec, 0
    .rept 256
    .p2align 4
    .if (vec == 8) || (vec == 10) || (vec == 11) || (vec == 12) || (vec == 13) || (vec == 14) || (vec == 17) || (vec == 21) || (vec == 29) || (vec == 30)
    pushq $vec
    .else
    pushq $0
    pushq $vec
    .endif
    jmp isr_common
    .set vec, vec + 1
    .endr
    .p2align 4
    .globl nx_isr_stubs_end
nx_isr_stubs_end:

    .type isr_common, @function
isr_common:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    cld
    movq %rsp, %rdi
    /* RBP still holds the interrupted frame pointer, so backtraces taken in
     * the handler continue into the interrupted code. */
    call nx_trap_dispatch
    .globl nx_trap_return
nx_trap_return:
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp          /* vector + error code */
    iretq
    .size isr_common, . - isr_common

/* void nx_ctx_switch(uint64_t *save_rsp, uint64_t next_rsp) */
    .globl nx_ctx_switch
    .type nx_ctx_switch, @function
nx_ctx_switch:
    pushfq
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rsp, (%rdi)
    movq %rsi, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    popfq
    ret
    .size nx_ctx_switch, . - nx_ctx_switch

/* First code of a new kernel thread: R12 = function, R13 = argument
 * (restored by nx_ctx_switch from the prepared stack).  Kernel threads run
 * with interrupts disabled; only the idle thread enables them around hlt. */
    .globl nx_kthread_trampoline
    .type nx_kthread_trampoline, @function
nx_kthread_trampoline:
    xorl %ebp, %ebp
    movq %r13, %rdi
    call *%r12
    movq %rax, %rdi
    call nx_task_exit_current
1:  hlt
    jmp 1b
    .size nx_kthread_trampoline, . - nx_kthread_trampoline

    .section .note.GNU-stack, "", @progbits
