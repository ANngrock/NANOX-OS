/* Host tests for the physical page allocator (kernel/mm/pmm.c). */
#include <stdint.h>
#include <string.h>

#include "mm/pmm.h"
#include "test.h"

static const struct nx_mem_region MAP[] = {
    {0x000000, 0x0A0000, NX_MEM_USABLE, 0}, /* below 1 MiB: never managed */
    {0x100000, 0x100000, NX_MEM_USABLE, 0}, {0x200000, 0x010000, NX_MEM_KERNEL_IMAGE, 0},
    {0x210000, 0x1F0000, NX_MEM_USABLE, 0}, {0x400000, 0x001000, NX_MEM_BOOT_RECLAIMABLE, 0},
    {0x401000, 0x0FF000, NX_MEM_USABLE, 0}, {0x500000, 0x100000, NX_MEM_RESERVED, 0},
    {0xFFC00000, 0x400000, NX_MEM_MMIO, 0},
};
#define N (sizeof(MAP) / sizeof(MAP[0]))
#define USABLE_PAGES (255 + 496 + 255) /* minus the storage page at 0x100000 */

static uint8_t storage[4096];
static uint8_t storage2[4096];

static int allowed(uint64_t p)
{
    return (p >= 0x101000 && p < 0x200000) || (p >= 0x210000 && p < 0x400000) ||
           (p >= 0x401000 && p < 0x500000);
}

static void init(struct nx_pmm *p)
{
    uint64_t bytes = nx_pmm_storage_bytes(nx_pmm_span_pages(MAP, N));
    CHECK_EQ_INT(bytes, 4096);
    CHECK_EQ_INT(nx_pmm_find_storage(MAP, N, bytes), 0x100000);
    memset(storage, 0x5A, sizeof(storage));
    CHECK_EQ_INT(nx_pmm_init(p, MAP, N, storage, 0x100000, bytes), NX_PMM_OK);
}

void test_pmm(void)
{
    struct nx_pmm p;
    CHECK_EQ_INT(nx_pmm_span_pages(MAP, N), 0x500000 / 4096);
    init(&p);
    CHECK_EQ_INT(p.managed_pages, USABLE_PAGES);
    CHECK_EQ_INT(p.free_pages, USABLE_PAGES);
    CHECK(!nx_pmm_is_free(&p, 0x100000)); /* storage */
    CHECK(!nx_pmm_is_free(&p, 0x1000));   /* below 1 MiB */
    CHECK(!nx_pmm_is_free(&p, 0x200000)); /* kernel image */
    CHECK(nx_pmm_is_free(&p, 0x101000));

    /* Exhaust the allocator: every page is distinct and allowed. */
    static uint64_t got[USABLE_PAGES];
    int bad = 0;
    for (int i = 0; i < USABLE_PAGES; i++) {
        got[i] = nx_pmm_alloc(&p);
        if (!got[i] || (got[i] & 0xFFF) || !allowed(got[i]))
            bad++;
        for (int j = 0; j < i; j++)
            bad += got[j] == got[i];
    }
    CHECK_EQ_INT(bad, 0);
    CHECK_EQ_INT(p.free_pages, 0);
    CHECK_EQ_INT(nx_pmm_alloc(&p), 0);

    /* Free / reuse and the error paths. */
    CHECK_EQ_INT(nx_pmm_free(&p, got[10]), NX_PMM_OK);
    CHECK_EQ_INT(nx_pmm_free(&p, got[10]), NX_PMM_E_DOUBLE_FREE);
    CHECK_EQ_INT(nx_pmm_alloc(&p), got[10]);
    CHECK_EQ_INT(nx_pmm_free(&p, got[11] + 0x10), NX_PMM_E_ALIGN);
    CHECK_EQ_INT(nx_pmm_free(&p, 0x200000), NX_PMM_E_UNMANAGED);
    CHECK_EQ_INT(nx_pmm_free(&p, 0x100000), NX_PMM_E_UNMANAGED);
    CHECK_EQ_INT(nx_pmm_free(&p, 0x1000), NX_PMM_E_UNMANAGED);
    CHECK_EQ_INT(nx_pmm_free(&p, 0x500000), NX_PMM_E_UNMANAGED);
    CHECK_EQ_INT(nx_pmm_free(&p, 0xFFFFF000ull), NX_PMM_E_UNMANAGED);
    bad = 0;
    for (int i = 0; i < USABLE_PAGES; i++)
        bad += nx_pmm_free(&p, got[i]) != NX_PMM_OK;
    CHECK_EQ_INT(bad, 0);
    CHECK_EQ_INT(p.free_pages, USABLE_PAGES);

    /* Reclaim a type once; usable memory and the storage are not re-added. */
    uint64_t added;
    CHECK_EQ_INT(nx_pmm_add_type(&p, MAP, N, NX_MEM_BOOT_RECLAIMABLE, &added), NX_PMM_OK);
    CHECK_EQ_INT(added, 1);
    CHECK(nx_pmm_is_free(&p, 0x400000));
    CHECK_EQ_INT(nx_pmm_add_type(&p, MAP, N, NX_MEM_BOOT_RECLAIMABLE, &added), NX_PMM_OK);
    CHECK_EQ_INT(added, 0);
    CHECK_EQ_INT(nx_pmm_add_type(&p, MAP, N, NX_MEM_USABLE, &added), NX_PMM_OK);
    CHECK_EQ_INT(added, 0);
    CHECK_EQ_INT(nx_pmm_add_type(&p, MAP, N, NX_MEM_KERNEL_IMAGE, &added), NX_PMM_OK);
    CHECK_EQ_INT(added, 0); /* never manageable */
    CHECK_EQ_INT(p.managed_pages, USABLE_PAGES + 1);

    /* Relocation keeps the state. */
    memcpy(storage2, storage, sizeof(storage));
    nx_pmm_relocate(&p, storage2);
    CHECK(nx_pmm_is_free(&p, 0x400000));
    CHECK(!nx_pmm_is_free(&p, 0x100000));

    /* init errors */
    struct nx_pmm q;
    CHECK_EQ_INT(nx_pmm_init(&q, MAP, N, storage, 0x100000, 100), NX_PMM_E_STORAGE);
    CHECK_EQ_INT(nx_pmm_init(&q, MAP, N, storage, 0x100800, 4096), NX_PMM_E_STORAGE);
    CHECK_EQ_INT(nx_pmm_init(&q, MAP, N, NULL, 0x100000, 4096), NX_PMM_E_STORAGE);
    static const struct nx_mem_region RES[] = {{0x100000, 0x100000, NX_MEM_RESERVED, 0}};
    CHECK_EQ_INT(nx_pmm_init(&q, RES, 1, storage, 0x100000, 4096), NX_PMM_E_EMPTY);
    static const struct nx_mem_region LOW[] = {{0x0, 0xA0000, NX_MEM_USABLE, 0}};
    CHECK_EQ_INT(nx_pmm_find_storage(LOW, 1, 4096), 0);
    CHECK_EQ_INT(nx_pmm_init(&q, LOW, 1, storage, 0x1000, 4096), NX_PMM_E_EMPTY);

    /* Random alloc/free against a reference model. */
    init(&p);
    static uint8_t held[0x500000 / 4096];
    memset(held, 0, sizeof(held));
    uint64_t x = 0xD1B54A32D192ED03ull;
    int nheld = 0, errors = 0;
    for (int it = 0; it < 20000; it++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if ((x & 3) != 0 || nheld == 0) {
            uint64_t a = nx_pmm_alloc(&p);
            if (nheld == USABLE_PAGES) {
                errors += a != 0;
                continue;
            }
            if (!a || !allowed(a) || held[a / 4096])
                errors++;
            else {
                held[a / 4096] = 1;
                nheld++;
            }
        } else {
            uint64_t idx = (x >> 8) % (sizeof(held));
            int want = held[idx]             ? NX_PMM_OK
                       : allowed(idx * 4096) ? NX_PMM_E_DOUBLE_FREE
                                             : NX_PMM_E_UNMANAGED;
            errors += nx_pmm_free(&p, idx * 4096) != want;
            if (held[idx]) {
                held[idx] = 0;
                nheld--;
            }
        }
        errors += p.free_pages != (uint64_t)(USABLE_PAGES - nheld);
    }
    CHECK_EQ_INT(errors, 0);
}
