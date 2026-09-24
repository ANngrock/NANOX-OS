/*
 * Kernel-owned GDT and TSS (M1).  Layout (docs/m1-kernel.md):
 *   0x00 null, 0x08 kernel code (64-bit), 0x10 kernel data, 0x18 TSS (16 bytes).
 * The TSS provides IST stacks for #DF, NMI and #MC; RSP0 is unused until
 * user mode exists (M2).
 */
#include <stdint.h>

#include "kernel.h"
#include "trap.h"

struct __attribute__((packed)) tss64 {
    uint32_t reserved0;
    uint64_t rsp[3];
    uint64_t reserved1;
    uint64_t ist[7];
    uint64_t reserved2;
    uint16_t reserved3;
    uint16_t iomap_base;
};
_Static_assert(sizeof(struct tss64) == 104, "TSS size");

struct __attribute__((packed)) descriptor_ptr {
    uint16_t limit;
    uint64_t base;
};

static struct tss64 tss __attribute__((aligned(16)));
static uint64_t gdt[5] __attribute__((aligned(16)));

void nx_gdt_init(void)
{
    tss.ist[NX_IST_DF - 1] = (uint64_t)(uintptr_t)__ist_df_top;
    tss.ist[NX_IST_NMI - 1] = (uint64_t)(uintptr_t)__ist_nmi_top;
    tss.ist[NX_IST_MC - 1] = (uint64_t)(uintptr_t)__ist_mc_top;
    tss.iomap_base = sizeof(tss); /* no I/O permission bitmap */

    uint64_t base = (uint64_t)(uintptr_t)&tss, limit = sizeof(tss) - 1;
    gdt[0] = 0;
    gdt[1] = 0x00AF9A000000FFFFull; /* code: P, DPL0, S, exec/read, L=1, G */
    gdt[2] = 0x00CF92000000FFFFull; /* data: P, DPL0, S, read/write, D/B, G */
    gdt[3] = (limit & 0xFFFF) | (base & 0xFFFFFF) << 16 | 0x89ull << 40 /* P, 64-bit TSS */ |
             ((limit >> 16) & 0xF) << 48 | ((base >> 24) & 0xFF) << 56;
    gdt[4] = base >> 32;

    struct descriptor_ptr p = {sizeof(gdt) - 1, (uint64_t)(uintptr_t)gdt};
    __asm__ volatile("lgdt %0\n\t"
                     "pushq $0x08\n\t"
                     "leaq 1f(%%rip), %%rax\n\t"
                     "pushq %%rax\n\t"
                     "lretq\n"
                     "1:\n\t"
                     "movw $0x10, %%ax\n\t"
                     "movw %%ax, %%ds\n\t"
                     "movw %%ax, %%es\n\t"
                     "movw %%ax, %%ss\n\t"
                     "xorw %%ax, %%ax\n\t"
                     "movw %%ax, %%fs\n\t"
                     "movw %%ax, %%gs\n\t"
                     "movw $0x18, %%ax\n\t"
                     "ltr %%ax\n\t"
                     :
                     : "m"(p)
                     : "rax", "memory");
}
