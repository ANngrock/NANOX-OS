#include <nanox/string.h>

#include "mm/mm.h"
#include "vmo.h"

struct nx_ep_slot {
    struct nx_endpoint ep;
    uint32_t used;
};

static struct nx_vmo vmos[NX_VMO_POOL];
static struct nx_ep_slot eps[NX_EP_POOL];
static struct nx_object sovereign = {NX_OBJ_SOVEREIGN, 0, 0}; /* refs 0: never freed */

static void vmo_destroy(struct nx_object *o)
{
    struct nx_vmo *v = (struct nx_vmo *)o;
    for (uint32_t i = 0; i < v->pages; i++)
        nx_page_free(v->phys[i]);
    v->pages = 0;
    v->used = 0;
}

struct nx_vmo *nx_vmo_create(uint32_t pages, int *err)
{
    if (pages == 0 || pages > NX_VMO_MAX_PAGES) {
        *err = NX_EINVAL;
        return 0;
    }
    struct nx_vmo *v = 0;
    for (uint32_t i = 0; i < NX_VMO_POOL && !v; i++)
        if (!vmos[i].used)
            v = &vmos[i];
    if (!v) {
        *err = NX_ENOMEM;
        return 0;
    }
    memset(v, 0, sizeof(*v));
    v->used = 1;
    v->base.type = NX_OBJ_VMO;
    v->base.refs = 1;
    v->base.destroy = vmo_destroy;
    for (; v->pages < pages; v->pages++) {
        uint64_t pa = nx_pmm_alloc(&nx_pmm);
        if (!pa) {
            vmo_destroy(&v->base);
            *err = NX_ENOMEM;
            return 0;
        }
        memset(nx_phys_to_virt(pa), 0, NX_PAGE_4K);
        v->phys[v->pages] = pa;
    }
    return v;
}

static void ep_destroy(struct nx_object *o)
{
    struct nx_ep_slot *s = (struct nx_ep_slot *)o;
    nx_ep_drain(&s->ep);
    s->used = 0;
}

struct nx_endpoint *nx_ep_create(void)
{
    for (uint32_t i = 0; i < NX_EP_POOL; i++) {
        struct nx_ep_slot *s = &eps[i];
        if (s->used)
            continue;
        memset(s, 0, sizeof(*s));
        s->used = 1;
        nx_ep_init(&s->ep);
        s->ep.base.type = NX_OBJ_ENDPOINT;
        s->ep.base.refs = 1;
        s->ep.base.destroy = ep_destroy;
        return &s->ep;
    }
    return 0;
}

struct nx_object *nx_sovereign(void)
{
    return &sovereign;
}

uint32_t nx_vmo_live(void)
{
    uint32_t n = 0;
    for (uint32_t i = 0; i < NX_VMO_POOL; i++)
        n += vmos[i].used;
    return n;
}

uint32_t nx_ep_live(void)
{
    uint32_t n = 0;
    for (uint32_t i = 0; i < NX_EP_POOL; i++)
        n += eps[i].used;
    return n;
}
