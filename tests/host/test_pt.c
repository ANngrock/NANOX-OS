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
/* Pages handed back through env->free (bump allocator: never reused). */
static uint8_t freed[ARENA_PAGES];
static unsigned freed_count;

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
    unsigned page = (unsigned)((phys - ARENA_BASE) / 4096);
    if (page >= used_pages || freed[page])
        abort(); /* never allocated, or used after it was freed */
    return arena + (phys - ARENA_BASE);
}

static void fake_free(void *ctx, uint64_t phys)
{
    (void)ctx;
    if (phys < ARENA_BASE || phys >= ARENA_BASE + sizeof(arena) || (phys & 0xFFF))
        abort();
    unsigned page = (unsigned)((phys - ARENA_BASE) / 4096);
    if (page >= used_pages || freed[page])
        abort(); /* not a table page, or a double free */
    freed[page] = 1;
    freed_count++;
}

static const struct nx_pt_env ENV = {fake_alloc, fake_virt, fake_free, 0};

static uint64_t fresh_root(unsigned limit)
{
    memset(arena, 0xCC, sizeof(arena)); /* new tables must be zeroed by the builder */
    memset(freed, 0, sizeof(freed));
    freed_count = 0;
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

static void test_destroy_slots(void);

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

    test_destroy_slots();
}

/* ---- nx_pt_destroy_slots / address-space teardown (M2) -------------------- */

#define LEAF_MAX 64
static struct {
    uint64_t pa, flags, size;
} leaves[LEAF_MAX];
static unsigned leaf_count;

static void record_leaf(void *ctx, uint64_t pa, uint64_t flags, uint64_t size)
{
    CHECK(ctx == &leaf_count);
    if (leaf_count < LEAF_MAX) {
        leaves[leaf_count].pa = pa;
        leaves[leaf_count].flags = flags;
        leaves[leaf_count].size = size;
    }
    leaf_count++;
}

static int leaf_seen(uint64_t pa, uint64_t size, uint64_t must_set)
{
    int n = 0;
    for (unsigned i = 0; i < leaf_count && i < LEAF_MAX; i++)
        n += leaves[i].pa == pa && leaves[i].size == size &&
             (leaves[i].flags & (must_set | NX_PTE_P)) == (must_set | NX_PTE_P);
    return n;
}

static unsigned page_of(uint64_t phys)
{
    return (unsigned)((phys - ARENA_BASE) / 4096);
}

static void test_destroy_slots(void)
{
    const uint64_t RW = NX_PTE_W | NX_PTE_NX, USER = NX_PTE_U | NX_PTE_OWNED;
    uint64_t pa, flags, size;

    /* A "kernel" root with mappings in slot 0 (image), 256 (physmap) and 511. */
    uint64_t kroot = fresh_root(ARENA_PAGES);
    CHECK_EQ_INT(nx_pt_map(&ENV, kroot, 0x200000, 0x200000, NX_PAGE_4K, 0), NX_PT_OK);
    CHECK_EQ_INT(nx_pt_map(&ENV, kroot, 0xFFFF800000000000ull, 0, NX_PAGE_2M, RW), NX_PT_OK);
    CHECK_EQ_INT(nx_pt_map(&ENV, kroot, 0xFFFFFFFFFFFFF000ull, 0x9000, NX_PAGE_4K, RW), NX_PT_OK);
    unsigned kernel_pages = used_pages;

    /* An address space as nx_as_create builds it: own root, kernel slots
     * 0 and 256..511 shared by copying the PML4 entries. */
    uint64_t root;
    CHECK_EQ_INT(nx_pt_new_root(&ENV, &root), NX_PT_OK);
    uint64_t *pml4 = fake_virt(0, root);
    const uint64_t *kpml4 = fake_virt(0, kroot);
    pml4[0] = kpml4[0];
    for (unsigned i = 256; i < 512; i++)
        pml4[i] = kpml4[i];
    unsigned before_user = used_pages;

    /* User mappings: program pages (slot 1), a 2 MiB page (slot 1), a
     * shared page that the address space does not own (slot 2) and the
     * stack at the top of slot 255. */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x8000000000ull, 0x100000, NX_PAGE_4K, USER), NX_PT_OK);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x8000001000ull, 0x101000, NX_PAGE_4K, USER | NX_PTE_NX),
                 NX_PT_OK);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x8000002000ull, 0x102000, NX_PAGE_4K, USER | RW),
                 NX_PT_OK);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x8000200000ull, 0x400000, NX_PAGE_2M, USER | RW),
                 NX_PT_OK);
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x10000000000ull, 0x500000, NX_PAGE_4K, NX_PTE_U | RW),
                 NX_PT_OK);
    for (uint64_t i = 1; i <= 3; i++) /* like the user stack below NX_USER_STACK_TOP */
        CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x7FFFFFFF0000ull - i * 0x1000, 0x600000 + i * 0x1000,
                               NX_PAGE_4K, USER | RW),
                     NX_PT_OK);
    /* Tables for the user half: slot 1 PDPT+PD+PT, slot 2 PDPT+PD+PT,
     * slot 255 PDPT+PD+PT; the 2 MiB page shares slot 1's PD. */
    unsigned user_tables = used_pages - before_user;
    CHECK_EQ_INT(user_tables, 9);
    CHECK_EQ_INT(pml4[1] & NX_PTE_U, NX_PTE_U);
    CHECK_EQ_INT(kpml4[0] & NX_PTE_U, 0); /* user mappings never touch the shared slots */

    /* Destroying slot 2 alone frees exactly its three tables. */
    leaf_count = 0;
    nx_pt_destroy_slots(&ENV, root, 2, 2, record_leaf, &leaf_count);
    CHECK_EQ_INT(leaf_count, 1);
    CHECK_EQ_INT(leaf_seen(0x500000, NX_PAGE_4K, NX_PTE_U), 1);
    CHECK_EQ_INT(leaves[0].flags & NX_PTE_OWNED, 0); /* the caller decides not to free it */
    CHECK_EQ_INT(freed_count, 3);
    CHECK_EQ_INT(pml4[2], 0);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x10000000000ull, &pa, &flags, &size),
                 NX_PT_E_NOT_MAPPED);
    expect_query(root, 0x8000000000ull, 0x100000, NX_PAGE_4K, NX_PTE_U); /* slot 1 untouched */

    /* Full teardown of the user half (NX_AS_USER_SLOT_FIRST..LAST). */
    leaf_count = 0;
    nx_pt_destroy_slots(&ENV, root, 1, 255, record_leaf, &leaf_count);
    CHECK_EQ_INT(leaf_count, 7);
    CHECK_EQ_INT(leaf_seen(0x100000, NX_PAGE_4K, USER), 1);
    CHECK_EQ_INT(leaf_seen(0x101000, NX_PAGE_4K, USER | NX_PTE_NX), 1);
    CHECK_EQ_INT(leaf_seen(0x102000, NX_PAGE_4K, USER | RW), 1);
    CHECK_EQ_INT(leaf_seen(0x400000, NX_PAGE_2M, USER | RW | NX_PTE_PS), 1);
    for (uint64_t i = 1; i <= 3; i++)
        CHECK_EQ_INT(leaf_seen(0x600000 + i * 0x1000, NX_PAGE_4K, USER | RW), 1);
    CHECK_EQ_INT(freed_count, user_tables); /* every user table, each exactly once */
    unsigned kernel_freed = 0, user_left = 0;
    for (unsigned p = 0; p < kernel_pages; p++)
        kernel_freed += freed[p]; /* shared kernel tables and the kernel root survive */
    CHECK_EQ_INT(kernel_freed, 0);
    CHECK_EQ_INT(freed[page_of(root)], 0); /* the root itself is the caller's to free */
    for (unsigned i = 1; i <= 255; i++)
        user_left += pml4[i] != 0;
    CHECK_EQ_INT(user_left, 0);
    CHECK_EQ_INT(pml4[0], kpml4[0]);
    CHECK_EQ_INT(pml4[256], kpml4[256]);
    CHECK_EQ_INT(pml4[511], kpml4[511]);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x8000000000ull, &pa, &flags, &size), NX_PT_E_NOT_MAPPED);
    CHECK_EQ_INT(nx_pt_query(&ENV, root, 0x8000200000ull, &pa, &flags, &size), NX_PT_E_NOT_MAPPED);
    /* Kernel mappings are still reachable through both roots. */
    expect_query(root, 0x200000, 0x200000, NX_PAGE_4K, NX_PTE_P);
    expect_query(root, 0xFFFF800000001234ull, 0x1234, NX_PAGE_2M, RW);
    expect_query(kroot, 0xFFFFFFFFFFFFF000ull, 0x9000, NX_PAGE_4K, RW);

    /* Idempotent: nothing left to free or report. */
    leaf_count = 0;
    nx_pt_destroy_slots(&ENV, root, 1, 255, record_leaf, &leaf_count);
    CHECK_EQ_INT(leaf_count, 0);
    CHECK_EQ_INT(freed_count, user_tables);

    /* The address space can be rebuilt afterwards (tables are re-created). */
    CHECK_EQ_INT(nx_pt_map(&ENV, root, 0x8000000000ull, 0x700000, NX_PAGE_4K, USER), NX_PT_OK);
    expect_query(root, 0x8000000000ull, 0x700000, NX_PAGE_4K, USER);
    leaf_count = 0;
    nx_pt_destroy_slots(&ENV, root, 1, 255, record_leaf, &leaf_count);
    CHECK_EQ_INT(leaf_count, 1);
    CHECK_EQ_INT(freed_count, user_tables + 3);

    /* Out-of-range slot numbers are clamped to the table. */
    leaf_count = 0;
    nx_pt_destroy_slots(&ENV, root, 600, 700, record_leaf, &leaf_count);
    CHECK_EQ_INT(leaf_count, 0);
    CHECK_EQ_INT(freed_count, user_tables + 3);
}
