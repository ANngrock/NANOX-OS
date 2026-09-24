#include "pt.h"

#define ENTRIES 512u

static unsigned index_of(uint64_t va, unsigned level) /* level 3 = PML4 ... 0 = PT */
{
    return (unsigned)(va >> (12 + 9 * level)) & (ENTRIES - 1);
}

int nx_pt_is_canonical(uint64_t va)
{
    uint64_t top = va >> 47;
    return top == 0 || top == 0x1FFFF;
}

static uint64_t *table(const struct nx_pt_env *env, uint64_t phys)
{
    return (uint64_t *)env->virt(env->ctx, phys);
}

static int new_table(const struct nx_pt_env *env, uint64_t *phys)
{
    uint64_t p = env->alloc(env->ctx);
    if (p == 0 || (p & (NX_PAGE_4K - 1)))
        return NX_PT_E_NOMEM;
    uint64_t *t = table(env, p);
    for (unsigned i = 0; i < ENTRIES; i++)
        t[i] = 0;
    *phys = p;
    return NX_PT_OK;
}

int nx_pt_new_root(const struct nx_pt_env *env, uint64_t *root)
{
    return new_table(env, root);
}

/* Returns the table that holds the level-`level` entries for va (1 = PD,
 * 0 = PT), creating intermediate tables as needed. */
static int walk_create(const struct nx_pt_env *env, uint64_t root, uint64_t va, unsigned level,
                       uint64_t user, uint64_t **out)
{
    uint64_t *t = table(env, root);
    for (unsigned l = 3; l > level; l--) {
        uint64_t *e = &t[index_of(va, l)];
        if (*e & NX_PTE_P) {
            if (*e & NX_PTE_PS)
                return NX_PT_E_EXISTS; /* a large page covers va */
            *e |= user;                /* user leaves need U on every level */
        } else {
            uint64_t p;
            int st = new_table(env, &p);
            if (st != NX_PT_OK)
                return st;
            *e = p | NX_PTE_P | NX_PTE_W | user;
        }
        t = table(env, *e & NX_PTE_ADDR);
    }
    *out = t;
    return NX_PT_OK;
}

int nx_pt_map(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t pa, uint64_t size,
              uint64_t flags)
{
    if (size != NX_PAGE_4K && size != NX_PAGE_2M)
        return NX_PT_E_SIZE;
    if ((va | pa) & (size - 1))
        return NX_PT_E_ALIGN;
    if (!nx_pt_is_canonical(va))
        return NX_PT_E_NONCANONICAL;
    if (pa >= NX_PHYS_MAX)
        return NX_PT_E_RANGE;
    if (flags & ~NX_PT_LEAF_FLAGS)
        return NX_PT_E_FLAGS;
    unsigned level = size == NX_PAGE_2M ? 1 : 0;
    uint64_t *t;
    int st = walk_create(env, root, va, level, flags & NX_PTE_U, &t);
    if (st != NX_PT_OK)
        return st;
    uint64_t *e = &t[index_of(va, level)];
    if (*e & NX_PTE_P)
        return NX_PT_E_EXISTS; /* a leaf or (for 2 MiB) a page table is there */
    *e = pa | flags | NX_PTE_P | (level == 1 ? NX_PTE_PS : 0);
    return NX_PT_OK;
}

int nx_pt_map_range(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t pa,
                    uint64_t len, uint64_t flags, int allow_2m)
{
    if ((va | pa | len) & (NX_PAGE_4K - 1))
        return NX_PT_E_ALIGN;
    if (len && (va > UINT64_MAX - len || pa > UINT64_MAX - len))
        return NX_PT_E_RANGE;
    while (len) {
        uint64_t size = NX_PAGE_4K;
        if (allow_2m && !((va | pa) & (NX_PAGE_2M - 1)) && len >= NX_PAGE_2M)
            size = NX_PAGE_2M;
        int st = nx_pt_map(env, root, va, pa, size, flags);
        if (st != NX_PT_OK)
            return st;
        va += size;
        pa += size;
        len -= size;
    }
    return NX_PT_OK;
}

/* Finds the leaf entry for va; sets *size to its page size. */
static int find_leaf(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t **leaf,
                     uint64_t *size)
{
    if (!nx_pt_is_canonical(va))
        return NX_PT_E_NONCANONICAL;
    uint64_t *t = table(env, root);
    for (unsigned l = 3;; l--) {
        uint64_t *e = &t[index_of(va, l)];
        if (!(*e & NX_PTE_P))
            return NX_PT_E_NOT_MAPPED;
        if (l == 0 || ((*e & NX_PTE_PS) && (l == 1 || l == 2))) {
            *leaf = e;
            *size = l == 0 ? NX_PAGE_4K : l == 1 ? NX_PAGE_2M : NX_PAGE_1G;
            return NX_PT_OK;
        }
        t = table(env, *e & NX_PTE_ADDR);
    }
}

int nx_pt_unmap(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t *pa,
                uint64_t *size)
{
    uint64_t *e;
    int st = find_leaf(env, root, va, &e, size);
    if (st != NX_PT_OK)
        return st;
    *pa = *e & NX_PTE_ADDR & ~(*size - 1);
    *e = 0;
    return NX_PT_OK;
}

int nx_pt_query(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t *pa,
                uint64_t *flags, uint64_t *size)
{
    uint64_t *e;
    int st = find_leaf(env, root, va, &e, size);
    if (st != NX_PT_OK)
        return st;
    uint64_t base = *e & NX_PTE_ADDR & ~(*size - 1);
    *pa = base + (va & (*size - 1));
    *flags = *e & ~NX_PTE_ADDR;
    return NX_PT_OK;
}

void nx_pt_destroy_slots(const struct nx_pt_env *env, uint64_t root, unsigned first,
                         unsigned last,
                         void (*leaf)(void *ctx, uint64_t pa, uint64_t flags, uint64_t size),
                         void *leaf_ctx)
{
    uint64_t *pml4 = table(env, root);
    for (unsigned i = first; i <= last && i < ENTRIES; i++) {
        if (!(pml4[i] & NX_PTE_P))
            continue;
        uint64_t pdpt_pa = pml4[i] & NX_PTE_ADDR;
        uint64_t *pdpt = table(env, pdpt_pa);
        for (unsigned j = 0; j < ENTRIES; j++) {
            uint64_t e3 = pdpt[j];
            if (!(e3 & NX_PTE_P))
                continue;
            if (e3 & NX_PTE_PS) {
                leaf(leaf_ctx, e3 & NX_PTE_ADDR & ~(NX_PAGE_1G - 1), e3 & ~NX_PTE_ADDR, NX_PAGE_1G);
                continue;
            }
            uint64_t *pd = table(env, e3 & NX_PTE_ADDR);
            for (unsigned k = 0; k < ENTRIES; k++) {
                uint64_t e2 = pd[k];
                if (!(e2 & NX_PTE_P))
                    continue;
                if (e2 & NX_PTE_PS) {
                    leaf(leaf_ctx, e2 & NX_PTE_ADDR & ~(NX_PAGE_2M - 1), e2 & ~NX_PTE_ADDR,
                         NX_PAGE_2M);
                    continue;
                }
                uint64_t *pt = table(env, e2 & NX_PTE_ADDR);
                for (unsigned l = 0; l < ENTRIES; l++)
                    if (pt[l] & NX_PTE_P)
                        leaf(leaf_ctx, pt[l] & NX_PTE_ADDR, pt[l] & ~NX_PTE_ADDR, NX_PAGE_4K);
                env->free(env->ctx, e2 & NX_PTE_ADDR);
            }
            env->free(env->ctx, e3 & NX_PTE_ADDR);
        }
        env->free(env->ctx, pdpt_pa);
        pml4[i] = 0;
    }
}

const char *nx_pt_strerror(int st)
{
    static const char *const names[NX_PT_E__COUNT] = {
        [NX_PT_OK] = "ok",
        [NX_PT_E_ALIGN] = "E_ALIGN",
        [NX_PT_E_NONCANONICAL] = "E_NONCANONICAL",
        [NX_PT_E_EXISTS] = "E_EXISTS",
        [NX_PT_E_NOMEM] = "E_NOMEM",
        [NX_PT_E_NOT_MAPPED] = "E_NOT_MAPPED",
        [NX_PT_E_FLAGS] = "E_FLAGS",
        [NX_PT_E_SIZE] = "E_SIZE",
        [NX_PT_E_RANGE] = "E_RANGE",
    };
    if (st < 0 || st >= NX_PT_E__COUNT)
        return "E_UNKNOWN";
    return names[st];
}
