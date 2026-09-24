/*
 * Physical page allocator (M1): two bitmaps over [0, npages * 4 KiB).
 *   managed[i] = 1  page i belongs to the allocator
 *   used[i]    = 1  page i is allocated (always 1 for unmanaged pages)
 * Pure code over caller-provided storage; compiled into the kernel and into
 * tests/host.  Rules: docs/m1-kernel.md ("Физический allocator").
 */
#ifndef NANOX_KERNEL_MM_PMM_H
#define NANOX_KERNEL_MM_PMM_H

#include <stdint.h>

#include <nanox/bootinfo.h>

/* Pages below 1 MiB are never managed (real-mode area, future AP trampoline). */
#define NX_PMM_MIN_PHYS 0x100000ull

enum nx_pmm_status {
    NX_PMM_OK = 0,
    NX_PMM_E_ALIGN,       /* address not 4 KiB aligned */
    NX_PMM_E_UNMANAGED,   /* page is not owned by the allocator */
    NX_PMM_E_DOUBLE_FREE, /* page is already free */
    NX_PMM_E_STORAGE,     /* storage too small, misplaced or not page aligned */
    NX_PMM_E_EMPTY,       /* no manageable memory in the map */
    NX_PMM_E__COUNT
};

struct nx_pmm {
    uint8_t *used;
    uint8_t *managed;
    uint64_t npages;
    uint64_t managed_pages;
    uint64_t free_pages;
    uint64_t next; /* next-fit search hint (page index) */
    uint64_t storage_first, storage_last; /* bitmap pages, never managed */
};

/* Pages covered by the bitmaps: up to the end of the highest region whose
 * type can ever be handed to the allocator. */
uint64_t nx_pmm_span_pages(const struct nx_mem_region *rg, uint32_t n);
/* Bytes of storage needed for both bitmaps, rounded up to whole pages. */
uint64_t nx_pmm_storage_bytes(uint64_t npages);
/* Page-aligned physical address inside a USABLE region >= NX_PMM_MIN_PHYS
 * with room for `bytes`, or 0. */
uint64_t nx_pmm_find_storage(const struct nx_mem_region *rg, uint32_t n, uint64_t bytes);
/* Initialises from USABLE regions; the storage pages themselves stay
 * unmanaged.  `storage` is the address through which the kernel accesses
 * `storage_phys`. */
int nx_pmm_init(struct nx_pmm *p, const struct nx_mem_region *rg, uint32_t n, void *storage,
                uint64_t storage_phys, uint64_t storage_bytes);
/* Hands every page of regions of `type` (>= NX_PMM_MIN_PHYS, inside the span,
 * not yet managed) to the allocator as free.  *added counts them. */
int nx_pmm_add_type(struct nx_pmm *p, const struct nx_mem_region *rg, uint32_t n, uint32_t type,
                    uint64_t *added);
/* Moves the bitmaps' access pointer (after a page-table switch). */
void nx_pmm_relocate(struct nx_pmm *p, void *storage);
/* Returns the physical address of a free page, or 0 when none is left. */
uint64_t nx_pmm_alloc(struct nx_pmm *p);
int nx_pmm_free(struct nx_pmm *p, uint64_t phys);
int nx_pmm_is_free(const struct nx_pmm *p, uint64_t phys);
const char *nx_pmm_strerror(int status);

#endif
