/*
 * Panic report (M1, task stacks since M2).  Format of the serial lines: docs/m1-kernel.md
 * ("Отчёт о panic и исключениях").  The harness symbolises the addresses in
 * the BACKTRACE lines with the symbol table of out/kernel.elf.
 */
#include <stdarg.h>

#include <nanox/diag.h>
#include <nanox/port.h>
#include <nanox/printf.h>

#include "arch/x86_64/cpu.h"
#include "arch/x86_64/trap.h"
#include "kernel.h"
#include "mm/mm.h"
#include "task.h"

#define BACKTRACE_MAX 32

static volatile int panicking;

void nx_debug_exit(uint8_t code)
{
    nx_outb(NX_DEBUG_EXIT_PORT, code);
    /* Not under QEMU (or the device is missing): stop here. */
    for (;;)
        __asm__ volatile("cli; hlt");
}

struct stack_range {
    const char *name;
    const char *guard, *bottom, *top;
};

static const struct stack_range stacks[] = {
    {"boot", __boot_stack_guard, __boot_stack_bottom, __boot_stack_top},
    {"ist-df", __ist_df_guard, __ist_df_bottom, __ist_df_top},
    {"ist-nmi", __ist_nmi_guard, __ist_nmi_bottom, __ist_nmi_top},
    {"ist-mc", __ist_mc_guard, __ist_mc_bottom, __ist_mc_top},
};

/* A frame record [rbp, rbp + 16) must lie inside one kernel stack (a fixed
 * stack or a task kernel stack, M2).  User stacks are never followed. */
static int frame_readable(uint64_t rbp)
{
    if (rbp & 7)
        return 0;
    for (unsigned i = 0; i < sizeof(stacks) / sizeof(stacks[0]); i++) {
        uint64_t lo = (uint64_t)(uintptr_t)stacks[i].bottom;
        uint64_t hi = (uint64_t)(uintptr_t)stacks[i].top;
        if (rbp >= lo && rbp + 16 <= hi)
            return 1;
    }
    return nx_task_stack_readable(rbp);
}

static void backtrace_from(uint64_t rbp, unsigned index)
{
    for (; index < BACKTRACE_MAX && rbp; index++) {
        if (!frame_readable(rbp)) {
            nx_printf("NANOX: BACKTRACE end: frame pointer 0x%016" NX_PRIx64
                      " outside kernel stacks\n",
                      rbp);
            return;
        }
        const uint64_t *fr = (const uint64_t *)(uintptr_t)rbp;
        if (fr[1] == 0)
            break;
        nx_printf("NANOX: BACKTRACE %u 0x%016" NX_PRIx64 "\n", index, fr[1]);
        rbp = fr[0];
    }
}

/* Name of the guard page containing `addr`, or NULL. */
static const char *guard_page_of(uint64_t addr)
{
    for (unsigned i = 0; i < sizeof(stacks) / sizeof(stacks[0]); i++) {
        uint64_t lo = (uint64_t)(uintptr_t)stacks[i].guard;
        uint64_t hi = (uint64_t)(uintptr_t)stacks[i].bottom;
        if (addr >= lo && addr < hi)
            return stacks[i].name;
    }
    return 0;
}

void nx_panic(const char *fmt, ...)
{
    __asm__ volatile("cli");
    if (panicking++) {
        nx_printf("NANOX: PANIC nested panic\n");
        nx_debug_exit(NX_EXIT_PANIC);
    }
    va_list ap;
    va_start(ap, fmt);
    nx_printf("NANOX: PANIC ");
    nx_vprintf(fmt, ap);
    nx_printf("\n");
    va_end(ap);
    backtrace_from(nx_read_rbp(), 0);
    nx_debug_exit(NX_EXIT_PANIC);
}

static void dump_registers(const struct nx_trap_frame *f, uint64_t cr2)
{
    nx_printf("NANOX: REGS rip=0x%016" NX_PRIx64 " cs=0x%04" NX_PRIx64 " rflags=0x%016" NX_PRIx64
              " rsp=0x%016" NX_PRIx64 " ss=0x%04" NX_PRIx64 "\n",
              f->rip, f->cs, f->rflags, f->rsp, f->ss);
    nx_printf("NANOX: REGS rax=0x%016" NX_PRIx64 " rbx=0x%016" NX_PRIx64 " rcx=0x%016" NX_PRIx64
              " rdx=0x%016" NX_PRIx64 "\n",
              f->rax, f->rbx, f->rcx, f->rdx);
    nx_printf("NANOX: REGS rsi=0x%016" NX_PRIx64 " rdi=0x%016" NX_PRIx64 " rbp=0x%016" NX_PRIx64
              " r8=0x%016" NX_PRIx64 "\n",
              f->rsi, f->rdi, f->rbp, f->r8);
    nx_printf("NANOX: REGS r9=0x%016" NX_PRIx64 " r10=0x%016" NX_PRIx64 " r11=0x%016" NX_PRIx64
              " r12=0x%016" NX_PRIx64 "\n",
              f->r9, f->r10, f->r11, f->r12);
    nx_printf("NANOX: REGS r13=0x%016" NX_PRIx64 " r14=0x%016" NX_PRIx64 " r15=0x%016" NX_PRIx64
              "\n",
              f->r13, f->r14, f->r15);
    nx_printf("NANOX: REGS cr0=0x%016" NX_PRIx64 " cr2=0x%016" NX_PRIx64 " cr3=0x%016" NX_PRIx64
              " cr4=0x%016" NX_PRIx64 " efer=0x%016" NX_PRIx64 "\n",
              nx_read_cr0(), cr2, nx_read_cr3(), nx_read_cr4(), nx_rdmsr(NX_MSR_EFER));
}

void nx_fatal_trap(struct nx_trap_frame *f)
{
    uint64_t cr2 = nx_read_cr2(); /* before anything can fault again */
    if (panicking++) {
        nx_printf("NANOX: PANIC nested trap vector=%" NX_PRIu64 " rip=0x%016" NX_PRIx64
                  " during panic\n",
                  f->vector, f->rip);
        nx_debug_exit(NX_EXIT_EXCEPTION);
    }
    const char *name = nx_exception_mnemonic(f->vector);
    nx_printf("NANOX: EXCEPTION %s vector=%" NX_PRIu64 " error=0x%" NX_PRIx64
              " rip=0x%016" NX_PRIx64 " cr2=0x%016" NX_PRIx64 "\n",
              name, f->vector, f->error, f->rip, cr2);
    if (f->vector == NX_VEC_PF)
        nx_printf("NANOX: EXCEPTION #PF present=%u write=%u user=%u reserved=%u fetch=%u\n",
                  (unsigned)(f->error & 1), (unsigned)(f->error >> 1 & 1),
                  (unsigned)(f->error >> 2 & 1), (unsigned)(f->error >> 3 & 1),
                  (unsigned)(f->error >> 4 & 1));
    if (f->vector == NX_VEC_PF)
        nx_vmm_describe("NANOX: EXCEPTION #PF mapping", cr2);
    const char *guard = guard_page_of(cr2), *task_guard = nx_task_guard_of(cr2);
    if (guard && (f->vector == NX_VEC_PF || f->vector == NX_VEC_DF))
        nx_printf("NANOX: EXCEPTION stack overflow: guard page of stack %s hit at 0x%016" NX_PRIx64
                  "\n",
                  guard, cr2);
    if (task_guard && (f->vector == NX_VEC_PF || f->vector == NX_VEC_DF))
        nx_printf("NANOX: EXCEPTION stack overflow: guard page of the kernel stack of task %s"
                  " hit at 0x%016" NX_PRIx64 "\n",
                  task_guard, cr2);
    if (nx_current)
        nx_printf("NANOX: EXCEPTION in kernel mode, current task %s#%u\n", nx_current->name,
                  nx_current->id);
    if (f->vector >= 32)
        nx_printf("NANOX: EXCEPTION unexpected interrupt vector %" NX_PRIu64 "\n", f->vector);
    dump_registers(f, cr2);
    nx_printf("NANOX: BACKTRACE 0 0x%016" NX_PRIx64 "\n", f->rip);
    backtrace_from(f->rbp, 1);
    nx_printf("NANOX: PANIC unhandled %s at rip=0x%016" NX_PRIx64 "\n", name, f->rip);
    nx_debug_exit(NX_EXIT_EXCEPTION);
}
