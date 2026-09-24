/*
 * Deliberate faults for the bench scenarios (nanox.test=<name>).  Each
 * function is noinline and has a stable name so that the harness can check
 * that the reported RIP lies inside it (docs/m1-kernel.md, "Сценарии").
 */
#include <stdint.h>

#include "faults.h"

/* #UD: undefined opcode. */
__attribute__((noinline)) void nx_fault_ud(void)
{
    __asm__ volatile("ud2");
}

/* #GP: load through a non-canonical address. */
__attribute__((noinline)) void nx_fault_gp(void)
{
    volatile uint64_t *p = (volatile uint64_t *)0x8000000000000000ull;
    (void)*p;
}

/* #DE: unsigned division by zero (in asm: the C operation would be UB). */
__attribute__((noinline)) void nx_fault_divzero(void)
{
    __asm__ volatile("xorl %%ecx, %%ecx\n\t"
                     "movl $1, %%eax\n\t"
                     "xorl %%edx, %%edx\n\t"
                     "divl %%ecx\n\t"
                     :
                     :
                     : "rax", "rcx", "rdx");
}

/* #PF, not present, write: an address that the kernel never maps. */
__attribute__((noinline)) void nx_fault_pagefault(void)
{
    volatile uint64_t *p = (volatile uint64_t *)NX_FAULT_UNMAPPED_ADDR;
    *p = 0x4E414E4F58ull;
}

/* #PF, not present, read: the null page is never mapped. */
__attribute__((noinline)) uint64_t nx_fault_nullderef(void)
{
    volatile uint64_t *p = (volatile uint64_t *)0;
    return *p;
}

static const uint64_t nx_fault_rodata_word = 0x524F444154410000ull;

/* #PF, present, write: .rodata is mapped read-only (CR0.WP = 1). */
__attribute__((noinline)) void nx_fault_wprotect(void)
{
    volatile uint64_t *p = (volatile uint64_t *)(uintptr_t)&nx_fault_rodata_word;
    *p = 1;
}

/* Writable buffer (.bss) that receives a `ret` instruction at run time. */
volatile uint8_t nx_fault_data_code[16];

/* #PF, present, instruction fetch: data pages are mapped with NX. */
__attribute__((noinline)) void nx_fault_nxexec(void)
{
    nx_fault_data_code[0] = 0xC3;
    void (*fn)(void) = (void (*)(void))(uintptr_t)nx_fault_data_code;
    fn();
}

/* Unbounded recursion into the guard page below the boot stack: #PF cannot
 * be delivered on the exhausted stack, so the CPU raises #DF on its IST stack. */
/* Never reached in practice; keeps the recursion formally bounded. */
static volatile uint64_t nx_fault_depth_limit = UINT64_MAX;

__attribute__((noinline)) uint64_t nx_fault_stack_overflow(uint64_t depth)
{
    if (depth == nx_fault_depth_limit)
        return 0;
    volatile uint8_t frame[512];
    frame[0] = (uint8_t)depth;
    frame[511] = (uint8_t)(depth >> 8);
    return nx_fault_stack_overflow(depth + 1) + frame[0] + frame[511];
}
