/* Host tests for the page-table builder (kernel/mm/pt.c) on a fake arena. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "mm/pt.h"
#include "test.h"

#define ARENA_PAGES 1024
#define ARENA_BASE 0x10000000ull

static _Alignas(4096) uint8_t arena[ARENA_PAGES * 4096];
static unsigned used_pages;
static unsigned alloc_limit;

static uint64_t fake_alloc(void *ctx)
{
    (void)ctx;
    if (used_pages >= alloc_limit || used_pages >= ARENA_PAGES)
        return 0;
    return ARENA_BASE + 4096ull * used_pages++;
}

static void *fake_virt(void *ctx, uint64_t phys)
{
    (void)ctx;
    if (phys < ARENA_BASE || phys >= ARENA_BASE + sizeof(arena))
        abort(); /* the builder must only touch pages it allocated */
    return arena + (phys - ARENA_BASE);
}

static const struct nx_pt_env ENV = {fake_alloc, fake_virt, 0, 0};

static uint64_t fresh_root(unsigned limit)
{
    memset(arena, 0xCC, sizeof(arena)); /* new tables must be zeroed by the builder */
    used_pages = 0;
    alloc_limit = limit;
    uint64_t root = 0;
    CHECK_EQ_INT(nx_pt_new_root(&ENV, &root), NX_PT_OK);
    return root;
}

static void expect_query(uint64_t root, uint64_t va, uint64_t pa, uint64_t size, uint64_t set)
{
    uint64_t qpa = 0, qflags = 0, qsize = 0;
    CHECK_EQ_INT(nx_pt_query(&ENV, root, va, &qpa, &qflags, &qsize), NX_PT_OK);
    CHECK_EQ_INT(qpa, pa);
    CHECK_EQ_INT(qsize, size);
    CHECK_EQ_INT(qflags & set, set);
}

void test_pt(void)
{
    uint64_t root = fresh_root(ARENA_PAGES), pa, flags, size;
    const uint64_t RWNX = NX_PTE_W | NX_PTE_NX;

    /* 4 KiB mapping and translation with offset. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x200000, 0x5000, NX_PAGE_4K, RWNX), NX_PT_OK);
    CHECK_EQ_INT(used_pages, 4); /* PML4, PDPT, PD, PT */
    expect_query(root, 0x200123, 0x5123, NX_PAGE_4K, NX_PTE_P | RWNX);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x201000, &pa, &flags, &size), NX_PT_E_NOT_MAPPED);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0, &pa, &flags, &size), NX_PT_E_NOT_MAPPED);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x200000, 0x6000, NX_PAGE_4K, 0), NX_PT_E_EXISTS);
    /* Read-only, executable leaf: no W, no NX. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x201000, 0x7000, NX_PAGE_4K, 0), NX_PT_OK);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x201000, &pa, &flags, &size), NX_PT_OK);
    CHECK_EQ_INT(flags & (NX_PTE_W | NX_PTE_NX), 0);

    /* 2 MiB pages and conflicts with 4 KiB tables. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x40000000, 0x80000000, NX_PAGE_2M, RWNX), NX_PT_OK);
    expect_query(root, 0x40123456, 0x80123456, NX_PAGE_2M, NX_PTE_PS);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x40001000, 0x1000, NX_PAGE_4K, 0), NX_PT_E_EXISTS);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x200000, 0x400000, NX_PAGE_2M, 0), NX_PT_E_EXISTS);

    /* Argument validation. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x300001, 0x1000, NX_PAGE_4K, 0), NX_PT_E_ALIGN);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, NX_PAGE_2M, 0), NX_PT_E_ALIGN);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, 0x2000, 0), NX_PT_E_SIZE);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x0000800000000000ull, 0x1000, NX_PAGE_4K, 0),
                 NX_PT_E_NONCANONICAL);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0xFFFF7FFFFFFFF000ull, &pa, &flags, &size),
                 NX_PT_E_NONCANONICAL);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 1ull << 52, NX_PAGE_4K, 0), NX_PT_E_RANGE);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, NX_PAGE_4K, NX_PTE_PS), NX_PT_E_FLAGS);
    /* Bit 9 is NX_PTE_OWNED (M2); the other software bits stay reserved. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, NX_PAGE_4K, 1ull << 10), NX_PT_E_FLAGS);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, NX_PAGE_4K, 1ull << 11), NX_PT_E_FLAGS);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, NX_PAGE_4K, 1ull << 52), NX_PT_E_FLAGS);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x600000, 0x1000, NX_PAGE_4K, NX_PTE_OWNED | NX_PTE_U),
                 NX_PT_OK);
    expect_query(root, 0x600000, 0x1000, NX_PAGE_4K, NX_PTE_OWNED | NX_PTE_U);

    /* Top of the canonical higher half. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0xFFFFFFFFFFFFF000ull, 0x9000, NX_PAGE_4K, RWNX), NX_PT_OK);
    expect_query(root, 0xFFFFFFFFFFFFFFFFull, 0x9FFF, NX_PAGE_4K, RWNX);

    /* Unmap. */
    uint64_t upa, usize;
    CHECK_EQ_INT(nx_pt_unmap(&ENV, root, 0x200abc, &upa, &usize), NX_PT_OK);
    CHECK_EQ_INT(upa, 0x5000);
    CHECK_EQ_INT(usize, NX_PAGE_4K);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x200000, &pa, &flags, &size), NX_PT_E_NOT_MAPPED);
    CHECK_EQ_INT(nx_pt_unmap(&ENV, root, 0x200000, &upa, &usize), NX_PT_E_NOT_MAPPED);
    CHECK_EQ_INT(nx_pt_unmap(&ENV, root, 0x40100000, &upa, &usize), NX_PT_OK);
    CHECK_EQ_INT(upa, 0x80000000);
    CHECK_EQ_INT(usize, NX_PAGE_2M);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x200000, 0x5000, NX_PAGE_4K, 0), NX_PT_OK); /* reuse */

    /* Ranges: 4 KiB up to the first 2 MiB boundary, then 2 MiB pages. */
    root = fresh_root(ARENA_PAGES);
    const uint64_t PHYSMAP = 0xFFFF800000000000ull;
    CHECK_EQ_INT(nx_pt_map_range(&ENV, root, PHYSMAP + 0x100000, 0x100000, 0x500000, RWNX, 1),
                 NX_PT_OK);
    expect_query(root, PHYSMAP + 0x1FF000, 0x1FF000, NX_PAGE_4K, RWNX);
    expect_query(root, PHYSMAP + 0x200000, 0x200000, NX_PAGE_2M, RWNX);
    expect_query(root, PHYSMAP + 0x5FFFFF, 0x5FFFFF, NX_PAGE_2M, RWNX);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, PHYSMAP + 0x600000, &pa, &flags, &size),
                 NX_PT_E_NOT_MAPPED);
    CHECK_EQ_INT(nx_pt_map_range(&ENV, root, 0x800000, 0x800000, 0x400000, 0, 0), NX_PT_OK);
    expect_query(root, 0xA00000, 0xA00000, NX_PAGE_4K, 0); /* 2 MiB pages disabled */
    CHECK_EQ_INT(nx_pt_map_range(&ENV, root, 0x1000, 0x1000, 0x1800, 0, 1), NX_PT_E_ALIGN);
    CHECK_EQ_INT(nx_pt_map_range(&ENV, root, 0xFFFFFFFFFFFFF000ull, 0, 0x2000, 0, 1),
                 NX_PT_E_RANGE);

    /* User leaves propagate U to every level; supervisor leaves do not set it. */
    root = fresh_root(ARENA_PAGES);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x400000, 0x1000, NX_PAGE_4K, 0), NX_PT_OK);
    const uint64_t *pml4 = fake_virt(0, root);
    CHECK_EQ_INT(pml4[0] & NX_PTE_U, 0);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x401000, 0x2000, NX_PAGE_4K, NX_PTE_U), NX_PT_OK);
    CHECK_EQ_INT(pml4[0] & NX_PTE_U, NX_PTE_U);

    /* Allocation failure is reported, not ignored. */
    root = fresh_root(2); /* root + PDPT only */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x200000, 0x5000, NX_PAGE_4K, 0), NX_PT_E_NOMEM);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x200000, &pa, &flags, &size), NX_PT_E_NOT_MAPPED);

    /* Differential test: random 4 KiB mappings against a reference list. */
    enum { N = 400 };
    static uint64_t vas[N], pas[N];
    root = fresh_root(ARENA_PAGES);
    uint64_t x = 0x9E3779B97F4A7C15ull;
    int mapped = 0, bad = 0;
    for (int i = 0; i < N; i++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        uint64_t va = (x & 0x00007FFFFFFFF000ull) >> (x % 20); /* clustered and sparse */
        va &= ~0xFFFull;
        if (x >> 63)
            va |= 0xFFFF800000000000ull;
        uint64_t p = (x >> 12) & 0x000FFFFFFFFFF000ull;
        int dup = 0;
        for (int j = 0; j < mapped; j++)
            dup |= vas[j] == va;
        int st = nx_pt_map(&ENV, root, va, p, NX_PAGE_4K, NX_PTE_W);
        if (dup ? st != NX_PT_E_EXISTS : st != NX_PT_OK)
            bad++;
        if (!dup && st == NX_PT_OK) {
            vas[mapped] = va;
            pas[mapped] = p;
            mapped++;
        }
    }
    for (int j = 0; j < mapped; j++) {
        if (nx_pt_query(&ENV, root, vas[j] + 0x10, &pa, &flags, &size) != NX_PT_OK ||
            pa != pas[j] + 0x10)
            bad++;
    }
    CHECK_EQ_INT(bad, 0);
    CHECK(mapped > N / 2);
}
