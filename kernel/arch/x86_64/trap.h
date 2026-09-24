/*
 * Trap frame and interrupt vectors.  Layout matches isr.S: the CPU pushes
 * ss..rip (and an error code for some exceptions), the stub pushes a zero
 * error code where the CPU does not, then the vector, then isr_common pushes
 * the general-purpose registers.
 */
#ifndef NANOX_ARCH_X86_64_TRAP_H
#define NANOX_ARCH_X86_64_TRAP_H

#include <stddef.h>
#include <stdint.h>

struct nx_trap_frame {
    uint64_t r15, r14, r13, r12, r11, r10, r9, r8;
    uint64_t rbp, rdi, rsi, rdx, rcx, rbx, rax;
    uint64_t vector, error;
    uint64_t rip, cs, rflags, rsp, ss;
};
_Static_assert(offsetof(struct nx_trap_frame, vector) == 15 * 8, "trap frame layout");
_Static_assert(sizeof(struct nx_trap_frame) == 22 * 8, "trap frame size");

/* Vector assignment (docs/m1-kernel.md). */
#define NX_VEC_DE 0
#define NX_VEC_BP 3
#define NX_VEC_UD 6
#define NX_VEC_DF 8
#define NX_VEC_GP 13
#define NX_VEC_PF 14
#define NX_VEC_PIC_BASE 0x20    /* legacy 8259 remapped here, all masked */
#define NX_VEC_TIMER 0x40       /* local APIC timer */
#define NX_VEC_SPURIOUS 0xFF    /* local APIC spurious vector */

/* IST slots (TSS.ist[n-1]). */
#define NX_IST_DF 1
#define NX_IST_NMI 2
#define NX_IST_MC 3

void nx_gdt_init(void);
void nx_idt_init(void);
/* Called from isr_common with interrupts disabled. */
void nx_trap_dispatch(struct nx_trap_frame *f);

/* Number of #BP traps handled and resumed (self-test). */
extern volatile uint64_t nx_breakpoint_count;
/* Spurious local-APIC interrupts seen. */
extern volatile uint64_t nx_spurious_count;
/* Handler for NX_VEC_TIMER, installed by the timer driver (NULL = unexpected). */
extern void (*volatile nx_timer_handler)(struct nx_trap_frame *f);

const char *nx_exception_mnemonic(uint64_t vector);

#endif
