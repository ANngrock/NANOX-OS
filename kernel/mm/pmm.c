#include "pmm.h"

#define PAGE 4096ull

static int bit(const uint8_t *m, uint64_t i)
{
    return m[i >> 3] >> (i & 7) & 1;
}

static void set_bit(uint8_t *m, uint64_t i)
{
    m[i >> 3] |= (uint8_t)(1u << (i & 7));
}

static void clear_bit(uint8_t *m, uint64_t i)
{
    m[i >> 3] &= (uint8_t) ~(1u << (i & 7));
}

static int manageable(uint32_t type)
{
    return type == NX_MEM_USABLE || type == NX_MEM_BOOT_RECLAIMABLE ||
           type == NX_MEM_KERNEL_STACK || type == NX_MEM_INITRD || type == NX_MEM_ACPI_RECLAIMABLE;
}

uint64_t nx_pmm_span_pages(const struct nx_mem_region *rg, uint32_t n)
{
    uint64_t end = 0;
    for (uint32_t i = 0; i < n; i++)
        if (manageable(rg[i].type) && rg[i].base + rg[i].length > end)
            end = rg[i].base + rg[i].length;
    return end / PAGE;
}

uint64_t nx_pmm_storage_bytes(uint64_t npages)
{
    uint64_t one = (npages + 7) / 8;
    return (2 * one + PAGE - 1) & ~(PAGE - 1);
}

uint64_t nx_pmm_find_storage(const struct nx_mem_region *rg, uint32_t n, uint64_t bytes)
{
    for (uint32_t i = 0; i < n; i++) {
        if (rg[i].type != NX_MEM_USABLE)
            continue;
        uint64_t base = rg[i].base < NX_PMM_MIN_PHYS ? NX_PMM_MIN_PHYS : rg[i].base;
        uint64_t end = rg[i].base + rg[i].length;
        if (base < end && end - base >= bytes)
            return base;
    }
    return 0;
}

/* Marks pages [first, last) of a region free and managed. */
static uint64_t add_pages(struct nx_pmm *p, uint64_t base, uint64_t end)
{
    uint64_t added = 0;
    if (base < NX_PMM_MIN_PHYS)
        base = NX_PMM_MIN_PHYS;
    uint64_t first = (base + PAGE - 1) / PAGE, last = end / PAGE;
    if (last > p->npages)
        last = p->npages;
    for (uint64_t i = first; i < last; i++) {
        if (bit(p->managed, i) || (i >= p->storage_first && i < p->storage_last))
            continue;
        set_bit(p->managed, i);
        clear_bit(p->used, i);
        added++;
    }
    p->managed_pages += added;
    p->free_pages += added;
    return added;
}

int nx_pmm_init(struct nx_pmm *p, const struct nx_mem_region *rg, uint32_t n, void *storage,
                uint64_t storage_phys, uint64_t storage_bytes)
{
    p->npages = nx_pmm_span_pages(rg, n);
    if (p->npages == 0)
        return NX_PMM_E_EMPTY;
    uint64_t one = (p->npages + 7) / 8;
    if (!storage || (storage_phys & (PAGE - 1)) || storage_bytes < 2 * one ||
        storage_phys > UINT64_MAX - storage_bytes - PAGE)
        return NX_PMM_E_STORAGE;
    p->used = storage;
    p->managed = p->used + one;
    for (uint64_t i = 0; i < one; i++) {
        p->used[i] = 0xFF;
        p->managed[i] = 0;
    }
    p->managed_pages = p->free_pages = 0;
    p->next = 0;
    /* The bitmaps live in usable memory: their pages are never managed. */
    p->storage_first = storage_phys / PAGE;
    p->storage_last = (storage_phys + storage_bytes + PAGE - 1) / PAGE;
    uint64_t added;
    nx_pmm_add_type(p, rg, n, NX_MEM_USABLE, &added);
    return p->managed_pages ? NX_PMM_OK : NX_PMM_E_EMPTY;
}

int nx_pmm_add_type(struct nx_pmm *p, const struct nx_mem_region *rg, uint32_t n, uint32_t type,
                    uint64_t *added)
{
    *added = 0;
    for (uint32_t i = 0; i < n; i++)
        if (rg[i].type == type && manageable(type))
            *added += add_pages(p, rg[i].base, rg[i].base + rg[i].length);
    return NX_PMM_OK;
}

void nx_pmm_relocate(struct nx_pmm *p, void *storage)
{
    uint64_t one = (p->npages + 7) / 8;
    p->used = storage;
    p->managed = p->used + one;
}

uint64_t nx_pmm_alloc(struct nx_pmm *p)
{
    if (p->free_pages == 0)
        return 0;
    for (uint64_t k = 0; k < p->npages; k++) {
        uint64_t i = (p->next + k) % p->npages;
        if (bit(p->managed, i) && !bit(p->used, i)) {
            set_bit(p->used, i);
            p->free_pages--;
            p->next = (i + 1) % p->npages;
            return i * PAGE;
        }
    }
    return 0; /* unreachable while free_pages is consistent */
}

int nx_pmm_free(struct nx_pmm *p, uint64_t phys)
{
    if (phys & (PAGE - 1))
        return NX_PMM_E_ALIGN;
    uint64_t i = phys / PAGE;
    if (i >= p->npages || !bit(p->managed, i))
        return NX_PMM_E_UNMANAGED;
    if (!bit(p->used, i))
        return NX_PMM_E_DOUBLE_FREE;
    clear_bit(p->used, i);
    p->free_pages++;
    return NX_PMM_OK;
}

int nx_pmm_is_free(const struct nx_pmm *p, uint64_t phys)
{
    uint64_t i = phys / PAGE;
    return i < p->npages && bit(p->managed, i) && !bit(p->used, i);
}

const char *nx_pmm_strerror(int st)
{
    static const char *const names[NX_PMM_E__COUNT] = {
        [NX_PMM_OK] = "ok",
        [NX_PMM_E_ALIGN] = "E_ALIGN",
        [NX_PMM_E_UNMANAGED] = "E_UNMANAGED",
        [NX_PMM_E_DOUBLE_FREE] = "E_DOUBLE_FREE",
        [NX_PMM_E_STORAGE] = "E_STORAGE",
        [NX_PMM_E_EMPTY] = "E_EMPTY",
    };
    if (st < 0 || st >= NX_PMM_E__COUNT)
        return "E_UNKNOWN";
    return names[st];
}
