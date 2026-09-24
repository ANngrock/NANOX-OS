/* x86-64 privileged-instruction helpers (kernel only). */
#ifndef NANOX_ARCH_X86_64_CPU_H
#define NANOX_ARCH_X86_64_CPU_H

#include <stdint.h>

#define NX_CR0_MP (1ull << 1)
#define NX_CR0_EM (1ull << 2)
#define NX_CR0_TS (1ull << 3)
#define NX_CR0_WP (1ull << 16)
#define NX_CR4_OSFXSR (1ull << 9)
#define NX_CR4_OSXMMEXCPT (1ull << 10)
#define NX_CR4_OSXSAVE (1ull << 18)
#define NX_CR4_PGE (1ull << 7)
#define NX_MSR_EFER 0xC0000080u
#define NX_EFER_NXE (1ull << 11)
#define NX_MSR_APIC_BASE 0x1Bu
#define NX_RFLAGS_IF (1ull << 9)

static inline uint64_t nx_read_cr0(void)
{
    uint64_t v;
    __asm__ volatile("mov %%cr0, %0" : "=r"(v));
    return v;
}

static inline void nx_write_cr0(uint64_t v)
{
    __asm__ volatile("mov %0, %%cr0" : : "r"(v) : "memory");
}

static inline uint64_t nx_read_cr2(void)
{
    uint64_t v;
    __asm__ volatile("mov %%cr2, %0" : "=r"(v));
    return v;
}

static inline uint64_t nx_read_cr3(void)
{
    uint64_t v;
    __asm__ volatile("mov %%cr3, %0" : "=r"(v));
    return v;
}

static inline void nx_write_cr3(uint64_t v)
{
    __asm__ volatile("mov %0, %%cr3" : : "r"(v) : "memory");
}

static inline uint64_t nx_read_cr4(void)
{
    uint64_t v;
    __asm__ volatile("mov %%cr4, %0" : "=r"(v));
    return v;
}

static inline void nx_write_cr4(uint64_t v)
{
    __asm__ volatile("mov %0, %%cr4" : : "r"(v) : "memory");
}

static inline uint64_t nx_rdmsr(uint32_t msr)
{
    uint32_t lo, hi;
    __asm__ volatile("rdmsr" : "=a"(lo), "=d"(hi) : "c"(msr));
    return (uint64_t)hi << 32 | lo;
}

static inline void nx_wrmsr(uint32_t msr, uint64_t v)
{
    __asm__ volatile("wrmsr" : : "c"(msr), "a"((uint32_t)v), "d"((uint32_t)(v >> 32)) : "memory");
}

static inline void nx_cpuid(uint32_t leaf, uint32_t sub, uint32_t r[4])
{
    __asm__ volatile("cpuid" : "=a"(r[0]), "=b"(r[1]), "=c"(r[2]), "=d"(r[3]) : "a"(leaf), "c"(sub));
}

static inline void nx_invlpg(uint64_t va)
{
    __asm__ volatile("invlpg (%0)" : : "r"(va) : "memory");
}

static inline uint64_t nx_rdtsc(void)
{
    uint32_t lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}

static inline uint64_t nx_read_rbp(void)
{
    uint64_t v;
    __asm__ volatile("mov %%rbp, %0" : "=r"(v));
    return v;
}

static inline uint64_t nx_read_rsp(void)
{
    uint64_t v;
    __asm__ volatile("mov %%rsp, %0" : "=r"(v));
    return v;
}

static inline uint64_t nx_read_rflags(void)
{
    uint64_t v;
    __asm__ volatile("pushfq; pop %0" : "=r"(v));
    return v;
}

static inline void nx_sti(void)
{
    __asm__ volatile("sti" : : : "memory");
}

static inline void nx_cli(void)
{
    __asm__ volatile("cli" : : : "memory");
}

#endif
