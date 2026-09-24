/*
 * x86-64 4-level page-table builder.  Pure code: page-table pages are
 * obtained and accessed only through the callbacks in struct nx_pt_env, so
 * the same code runs in the kernel and in tests/host (fake physical arena).
 * Only 4 KiB and 2 MiB leaves are created; 1 GiB leaves are understood by
 * nx_pt_query.
 */
#ifndef NANOX_KERNEL_MM_PT_H
#define NANOX_KERNEL_MM_PT_H

#include <stdint.h>

#define NX_PTE_P (1ull << 0)
#define NX_PTE_W (1ull << 1)
#define NX_PTE_U (1ull << 2)
#define NX_PTE_PWT (1ull << 3)
#define NX_PTE_PCD (1ull << 4)
#define NX_PTE_A (1ull << 5)
#define NX_PTE_D (1ull << 6)
#define NX_PTE_PS (1ull << 7)
#define NX_PTE_G (1ull << 8)
/* Software bit (AVL): the page belongs to the address space and is freed
 * with it (as opposed to pages of a shared memory object). */
#define NX_PTE_OWNED (1ull << 9)
#define NX_PTE_NX (1ull << 63)
#define NX_PTE_ADDR 0x000FFFFFFFFFF000ull
/* Flags a caller may request for a leaf (P is implied). */
#define NX_PT_LEAF_FLAGS                                                                           \
    (NX_PTE_W | NX_PTE_U | NX_PTE_PWT | NX_PTE_PCD | NX_PTE_G | NX_PTE_OWNED | NX_PTE_NX)

#define NX_PAGE_4K 0x1000ull
#define NX_PAGE_2M 0x200000ull
#define NX_PAGE_1G 0x40000000ull
#define NX_PHYS_MAX (1ull << 52)

struct nx_pt_env {
    /* Returns the physical address of a free page, or 0. */
    uint64_t (*alloc)(void *ctx);
    /* Returns a pointer through which the page at `phys` can be accessed. */
    void *(*virt)(void *ctx, uint64_t phys);
    /* Returns a page-table page (used by nx_pt_destroy_slots only). */
    void (*free)(void *ctx, uint64_t phys);
    void *ctx;
};

enum nx_pt_status {
    NX_PT_OK = 0,
    NX_PT_E_ALIGN,        /* va/pa/len not aligned to the page size */
    NX_PT_E_NONCANONICAL, /* va is not a canonical 48-bit address */
    NX_PT_E_EXISTS,       /* va (or part of a large page) already mapped */
    NX_PT_E_NOMEM,        /* alloc callback returned 0 */
    NX_PT_E_NOT_MAPPED,   /* nothing mapped at va */
    NX_PT_E_FLAGS,        /* unsupported leaf flag bits */
    NX_PT_E_SIZE,         /* page size is not 4 KiB or 2 MiB */
    NX_PT_E_RANGE,        /* pa beyond 52 bits or va range wraps */
    NX_PT_E__COUNT
};

int nx_pt_new_root(const struct nx_pt_env *env, uint64_t *root);
int nx_pt_map(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t pa,
              uint64_t size, uint64_t flags);
/* Maps [va, va+len) -> [pa, pa+len) with 4 KiB pages, using 2 MiB pages where
 * both addresses are 2 MiB aligned and allow_2m is set.  Stops at the first
 * error (earlier pages stay mapped). */
int nx_pt_map_range(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t pa,
                    uint64_t len, uint64_t flags, int allow_2m);
/* Removes the leaf that maps va; returns its physical base and size. */
int nx_pt_unmap(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t *pa,
                uint64_t *size);
/* Translates va: physical address (with offset), leaf flags, leaf size. */
int nx_pt_query(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t *pa,
                uint64_t *flags, uint64_t *size);
int nx_pt_is_canonical(uint64_t va);
/* Tears down PML4 slots [first, last]: calls `leaf` for every mapped leaf
 * (physical base, leaf flags, size), frees every table page below those
 * slots through env->free and clears the PML4 entries. */
void nx_pt_destroy_slots(const struct nx_pt_env *env, uint64_t root, unsigned first,
                         unsigned last,
                         void (*leaf)(void *ctx, uint64_t pa, uint64_t flags, uint64_t size),
                         void *leaf_ctx);
const char *nx_pt_strerror(int status);

#endif
