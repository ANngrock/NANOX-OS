#include "handle.h"

void nx_obj_ref(struct nx_object *o)
{
    o->refs++;
}

void nx_obj_unref(struct nx_object *o)
{
    if (o->refs == 0)
        return; /* static objects that are never counted down to zero */
    if (--o->refs == 0 && o->destroy)
        o->destroy(o);
}

static uint32_t encode(uint32_t index, uint32_t gen)
{
    return gen << 8 | (index + 1);
}

static void bump(struct nx_handle_entry *e)
{
    e->gen = e->gen >= NX_HANDLE_GEN_MAX ? 1 : e->gen + 1;
}

void nx_ht_init(struct nx_handle_table *t)
{
    for (uint32_t i = 0; i < NX_HANDLE_SLOTS; i++) {
        t->e[i].obj = 0;
        t->e[i].rights = 0;
        t->e[i].gen = 1;
    }
}

int nx_ht_install(struct nx_handle_table *t, struct nx_object *obj, uint32_t rights, uint32_t *h)
{
    if (rights & ~NX_RIGHTS_ALL)
        return NX_EINVAL;
    for (uint32_t i = 0; i < NX_HANDLE_SLOTS; i++) {
        if (t->e[i].obj)
            continue;
        nx_obj_ref(obj);
        t->e[i].obj = obj;
        t->e[i].rights = rights;
        *h = encode(i, t->e[i].gen);
        return NX_OK;
    }
    return NX_ENOMEM;
}

static struct nx_handle_entry *entry(const struct nx_handle_table *t, uint32_t h)
{
    uint32_t slot = h & 0xFF, gen = h >> 8;
    if (slot == 0 || slot > NX_HANDLE_SLOTS || (h >> 31))
        return 0;
    const struct nx_handle_entry *e = &t->e[slot - 1];
    if (!e->obj || e->gen != gen)
        return 0;
    return (struct nx_handle_entry *)e;
}

int nx_ht_lookup(const struct nx_handle_table *t, uint32_t h, uint32_t type, uint32_t need,
                 struct nx_object **obj, uint32_t *rights)
{
    struct nx_handle_entry *e = entry(t, h);
    if (!e)
        return NX_EBADHANDLE;
    if (type != NX_OBJ_ANY && e->obj->type != type)
        return NX_EWRONGTYPE;
    if ((e->rights & need) != need)
        return NX_EACCESS;
    if (obj)
        *obj = e->obj;
    if (rights)
        *rights = e->rights;
    return NX_OK;
}

int nx_ht_close(struct nx_handle_table *t, uint32_t h)
{
    struct nx_handle_entry *e = entry(t, h);
    if (!e)
        return NX_EBADHANDLE;
    struct nx_object *o = e->obj;
    e->obj = 0;
    e->rights = 0;
    bump(e);
    nx_obj_unref(o);
    return NX_OK;
}

int nx_ht_dup(struct nx_handle_table *t, uint32_t h, uint32_t rights, uint32_t *out)
{
    struct nx_object *o;
    uint32_t have;
    int st = nx_ht_lookup(t, h, NX_OBJ_ANY, NX_RIGHT_DUPLICATE, &o, &have);
    if (st != NX_OK)
        return st;
    if (rights & ~have)
        return NX_EACCESS; /* rights are never added */
    return nx_ht_install(t, o, rights, out);
}

int nx_ht_take(struct nx_handle_table *t, uint32_t h, uint32_t rights, struct nx_object **obj)
{
    uint32_t have;
    int st = nx_ht_lookup(t, h, NX_OBJ_ANY, NX_RIGHT_TRANSFER, obj, &have);
    if (st != NX_OK)
        return st;
    if (rights & ~have)
        return NX_EACCESS;
    struct nx_handle_entry *e = entry(t, h);
    e->obj = 0;
    e->rights = 0;
    bump(e);
    return NX_OK; /* the table's reference moves to the caller */
}

void nx_ht_clear(struct nx_handle_table *t)
{
    for (uint32_t i = 0; i < NX_HANDLE_SLOTS; i++) {
        if (!t->e[i].obj)
            continue;
        nx_ht_close(t, encode(i, t->e[i].gen));
    }
}

uint32_t nx_ht_count(const struct nx_handle_table *t)
{
    uint32_t n = 0;
    for (uint32_t i = 0; i < NX_HANDLE_SLOTS; i++)
        n += t->e[i].obj != 0;
    return n;
}
