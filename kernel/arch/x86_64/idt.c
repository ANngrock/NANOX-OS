/*
 * Kernel-owned IDT and trap dispatch (M1).  All 256 vectors point at the
 * stubs in isr.S (interrupt gates, DPL 0).  #DF, NMI and #MC run on IST
 * stacks so that they are reported even when the kernel stack is exhausted.
 */
#include <stdint.h>

#include <nanox/printf.h>

#include "kernel.h"
#include "trap.h"
#include "task.h"

struct __attribute__((packed)) idt_entry {
    uint16_t offset_lo;
    uint16_t selector;
    uint8_t ist;
    uint8_t type_attr;
    uint16_t offset_mid;
    uint32_t offset_hi;
    uint32_t zero;
};
_Static_assert(sizeof(struct idt_entry) == 16, "IDT entry size");

struct __attribute__((packed)) idt_ptr {
    uint16_t limit;
    uint64_t base;
};

extern char nx_isr_stubs[];
extern char nx_isr_stubs_end[];
_Static_assert(256 * 16 <= 8192, "stub table");

static struct idt_entry idt[256] __attribute__((aligned(16)));

volatile uint64_t nx_breakpoint_count;
volatile uint64_t nx_spurious_count;
void (*volatile nx_timer_handler)(struct nx_trap_frame *f);

static const char *const mnemonics[32] = {
    "#DE", "#DB", "NMI", "#BP", "#OF", "#BR", "#UD", "#NM", "#DF", "CSO", "#TS",
    "#NP", "#SS", "#GP", "#PF", "RES", "#MF", "#AC", "#MC", "#XM", "#VE", "#CP",
    "RES", "RES", "RES", "RES", "RES", "RES", "#HV", "#VC", "#SX", "RES",
};

const char *nx_exception_mnemonic(uint64_t vector)
{
    return vector < 32 ? mnemonics[vector] : "IRQ";
}

static void set_gate(unsigned vec, uint64_t handler, uint8_t ist, uint8_t dpl)
{
    idt[vec].offset_lo = (uint16_t)handler;
    idt[vec].selector = 0x08;
    idt[vec].ist = ist;
    idt[vec].type_attr = (uint8_t)(0x8E | dpl << 5); /* present, 64-bit interrupt gate */
    idt[vec].offset_mid = (uint16_t)(handler >> 16);
    idt[vec].offset_hi = (uint32_t)(handler >> 32);
    idt[vec].zero = 0;
}

void nx_idt_init(void)
{
    if (nx_isr_stubs_end - nx_isr_stubs != 256 * 16)
        nx_panic("isr stubs are not 16 bytes each");
    uint64_t base = (uint64_t)(uintptr_t)nx_isr_stubs;
    for (unsigned v = 0; v < 256; v++) {
        uint8_t ist = 0;
        if (v == NX_VEC_DF)
            ist = NX_IST_DF;
        else if (v == 2)
            ist = NX_IST_NMI;
        else if (v == 18)
            ist = NX_IST_MC;
        /* Only the syscall vector may be raised by `int` from user mode. */
        set_gate(v, base + 16ull * v, ist, v == NX_VEC_SYSCALL ? 3 : 0);
    }
    struct idt_ptr p = {sizeof(idt) - 1, (uint64_t)(uintptr_t)idt};
    __asm__ volatile("lidt %0" : : "m"(p) : "memory");
}

void nx_trap_dispatch(struct nx_trap_frame *f)
{
    int from_user = (f->cs & 3) == 3;
    if (f->vector == NX_VEC_BP && !from_user) {
        /* Breakpoints are resumable: RIP already points after int3. */
        nx_breakpoint_count++;
        return;
    }
    if (f->vector == NX_VEC_TIMER && nx_timer_handler) {
        nx_timer_handler(f);
        return;
    }
    if (f->vector == NX_VEC_SPURIOUS) {
        nx_spurious_count++; /* no EOI for spurious interrupts */
        return;
    }
    if (from_user) {
        if (f->vector == NX_VEC_SYSCALL) {
            nx_syscall(f);
            return;
        }
        nx_user_trap(f); /* terminates the task, does not return */
    }
    nx_fatal_trap(f);
}
