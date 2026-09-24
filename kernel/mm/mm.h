/*
 * Kernel memory management (M1): physical allocator instance, kernel page
 * tables and the physical-memory window ("physmap").  Layout and rules:
 * docs/m1-kernel.md ("Адресное пространство ядра").
 */
#ifndef NANOX_KERNEL_MM_MM_H
#define NANOX_KERNEL_MM_MM_H

#include <stdint.h>

#include <nanox/bootinfo.h>

#include "pmm.h"
#include "pt.h"

/* All RAM regions (except the kernel image) are mapped at this offset. */
#define NX_PHYSMAP_BASE ((uint64_t)0xFFFF800000000000ull)
/* Scratch virtual address used by the VMM self-test. */
#define NX_VMM_SELFTEST_VA ((uint64_t)0xFFFFC00000000000ull)
/* Task kernel stacks: one NX_KSTACK_STRIDE window per task slot, the lowest
 * page of each window is an unmapped guard page. */
#define NX_KSTACK_BASE ((uint64_t)0xFFFFFE0000000000ull)
#define NX_KSTACK_PAGES 4u
#define NX_KSTACK_STRIDE ((uint64_t)0x8000u)
/* PML4 slots of the user half (slot 0 is the kernel image, 256+ kernel). */
#define NX_AS_USER_SLOT_FIRST 1u
#define NX_AS_USER_SLOT_LAST 255u

extern struct nx_pmm nx_pmm;
/* Kernel PML4 (physical), 0 until nx_vmm_init switched CR3. */
extern uint64_t nx_kernel_root;
/* Added to a physical address to reach it: 0 on the UEFI identity map,
 * NX_PHYSMAP_BASE after nx_vmm_init. */
extern uint64_t nx_phys_offset;

static inline void *nx_phys_to_virt(uint64_t phys)
{
    return (void *)(uintptr_t)(phys + nx_phys_offset);
}

/* Physical allocator from the USABLE regions (runs on the identity map). */
void nx_pmm_setup(const struct nx_mem_region *rg, uint32_t n);
/* Allocates a page or panics; frees a page or panics (double free etc.). */
uint64_t nx_page_alloc(void);
void nx_page_free(uint64_t phys);

/* Builds the kernel page tables, switches CR3, verifies the result. */
void nx_vmm_init(const struct nx_mem_region *rg, uint32_t n);
/* Maps one 4 KiB MMIO page uncached into the physmap; returns its address. */
void *nx_vmm_map_mmio(uint64_t phys);
/* Maps/unmaps a 4 KiB page in the kernel address space (with invlpg). */
int nx_vmm_map_page(uint64_t va, uint64_t pa, uint64_t flags);
int nx_vmm_unmap_page(uint64_t va, uint64_t *pa);
int nx_vmm_query(uint64_t va, uint64_t *pa, uint64_t *flags, uint64_t *size);
/* Page-table environment of the kernel (physmap access, allocator). */
const struct nx_pt_env *nx_vmm_env(void);
/* Map / query in an arbitrary address space (invlpg if it is active). */
int nx_vmm_map_page_in(uint64_t root, uint64_t va, uint64_t pa, uint64_t flags);
int nx_vmm_query_in(uint64_t root, uint64_t va, uint64_t *pa, uint64_t *flags, uint64_t *size);
/* New address space sharing PML4 slot 0 and 256..511 with the kernel. */
uint64_t nx_as_create(void);
/* Frees the user half (leaf callback decides about leaf pages) and the root. */
void nx_as_destroy(uint64_t root,
                   void (*leaf)(void *ctx, uint64_t pa, uint64_t flags, uint64_t size),
                   void *ctx);
/* Page-table pages currently allocated by the kernel. */
uint64_t nx_vmm_tables(void);
/* One serial line describing how va is mapped (used by the fault report). */
void nx_vmm_describe(const char *prefix, uint64_t va);

#endif
