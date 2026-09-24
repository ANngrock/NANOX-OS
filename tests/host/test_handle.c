/* Host tests for the handle table and object references (kernel/obj/handle.c). */
#include <stdint.h>
#include <string.h>

#include "obj/handle.h"
#include "test.h"

static int destroyed;
static struct nx_object *last_destroyed;

static void on_destroy(struct nx_object *o)
{
    destroyed++;
    last_destroyed = o;
}

static struct nx_object make(uint32_t type)
{
    struct nx_object o = {type, 1, on_destroy}; /* the creator's reference */
    return o;
}

static uint32_t slot_of(uint32_t h)
{
    return h & 0xFF;
}

static const uint32_t VMO_ALL = NX_RIGHT_READ | NX_RIGHT_WRITE | NX_RIGHT_MAP |
                                NX_RIGHT_TRANSFER | NX_RIGHT_DUPLICATE;

static void test_install_lookup(void)
{
    /* A live-looking entry right behind the table: a lookup that indexed
     * past the last slot would find it. */
    static struct {
        struct nx_handle_table t;
        struct nx_handle_entry beyond;
    } w;
    struct nx_handle_table *tp = &w.t;
#define t (*tp)
    struct nx_object vmo = make(NX_OBJ_VMO), *o = 0;
    uint32_t h = 0, rights = 0;
    nx_ht_init(&t);
    w.beyond.obj = &vmo;
    w.beyond.rights = NX_RIGHTS_ALL;
    w.beyond.gen = 1;
    CHECK_EQ_INT(nx_ht_count(&t), 0);

    CHECK_EQ_INT(nx_ht_install(&t, &vmo, VMO_ALL, &h), NX_OK);
    CHECK(h != 0);
    CHECK_EQ_INT(h >> 31, 0);
    CHECK_EQ_INT(vmo.refs, 2);
    CHECK_EQ_INT(nx_ht_count(&t), 1);

    CHECK_EQ_INT(nx_ht_lookup(&t, h, NX_OBJ_VMO, NX_RIGHT_READ | NX_RIGHT_MAP, &o, &rights),
                 NX_OK);
    CHECK(o == &vmo);
    CHECK_EQ_INT(rights, VMO_ALL);
    CHECK_EQ_INT(nx_ht_lookup(&t, h, NX_OBJ_ANY, 0, 0, 0), NX_OK);
    CHECK_EQ_INT(nx_ht_lookup(&t, h, NX_OBJ_ENDPOINT, 0, 0, 0), NX_EWRONGTYPE);
    CHECK_EQ_INT(nx_ht_lookup(&t, h, NX_OBJ_VMO, NX_RIGHT_SEND, 0, 0), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_lookup(&t, h, NX_OBJ_VMO, NX_RIGHT_READ | NX_RIGHT_SEND, 0, 0), NX_EACCESS);

    /* Values that were never issued. */
    CHECK_EQ_INT(nx_ht_lookup(&t, 0, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_lookup(&t, h ^ 0x100, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE); /* generation */
    CHECK_EQ_INT(nx_ht_lookup(&t, h + 1, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);     /* free slot */
    CHECK_EQ_INT(nx_ht_lookup(&t, 1u << 8 | (NX_HANDLE_SLOTS + 1), NX_OBJ_ANY, 0, 0, 0),
                 NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_lookup(&t, (h & ~0xFFu) | 0xFF, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_lookup(&t, h | 0x80000000u, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_lookup(&t, 0xFFFFFFFFu, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);

    /* Rights outside the defined set are refused without side effects. */
    uint32_t h2 = 0;
    CHECK_EQ_INT(nx_ht_install(&t, &vmo, NX_RIGHTS_ALL + 1, &h2), NX_EINVAL);
    CHECK_EQ_INT(nx_ht_install(&t, &vmo, 1u << 31, &h2), NX_EINVAL);
    CHECK_EQ_INT(vmo.refs, 2);
    CHECK_EQ_INT(nx_ht_count(&t), 1);

    CHECK_EQ_INT(nx_ht_close(&t, h), NX_OK);
    CHECK_EQ_INT(vmo.refs, 1);
    CHECK_EQ_INT(destroyed, 0); /* the creator still holds a reference */
    CHECK_EQ_INT(nx_ht_close(&t, h), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_lookup(&t, h, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);
#undef t
}

static void test_dup_rights(void)
{
    static struct nx_handle_table t;
    struct nx_object vmo = make(NX_OBJ_VMO);
    uint32_t h, ro, same, rights;
    nx_ht_init(&t);
    CHECK_EQ_INT(nx_ht_install(&t, &vmo, VMO_ALL, &h), NX_OK);

    /* Duplicating with fewer rights: a new, different handle to the same object. */
    CHECK_EQ_INT(nx_ht_dup(&t, h, NX_RIGHT_READ | NX_RIGHT_MAP, &ro), NX_OK);
    CHECK(ro != h);
    CHECK_EQ_INT(vmo.refs, 3);
    CHECK_EQ_INT(nx_ht_lookup(&t, ro, NX_OBJ_VMO, 0, 0, &rights), NX_OK);
    CHECK_EQ_INT(rights, NX_RIGHT_READ | NX_RIGHT_MAP);
    CHECK_EQ_INT(nx_ht_lookup(&t, ro, NX_OBJ_VMO, NX_RIGHT_WRITE, 0, 0), NX_EACCESS);

    /* Keeping all rights is allowed, adding one is not. */
    CHECK_EQ_INT(nx_ht_dup(&t, h, VMO_ALL, &same), NX_OK);
    CHECK_EQ_INT(nx_ht_close(&t, same), NX_OK);
    uint32_t out = 0xABCD;
    CHECK_EQ_INT(nx_ht_dup(&t, h, VMO_ALL | NX_RIGHT_SEND, &out), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_dup(&t, h, NX_RIGHT_SOVEREIGN, &out), NX_EACCESS);
    CHECK_EQ_INT(out, 0xABCD);
    /* A handle without DUPLICATE cannot be duplicated at all, not even to
     * fewer rights. */
    CHECK_EQ_INT(nx_ht_dup(&t, ro, NX_RIGHT_READ, &out), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_dup(&t, 0, NX_RIGHT_READ, &out), NX_EBADHANDLE);
    CHECK_EQ_INT(vmo.refs, 3);
    CHECK_EQ_INT(nx_ht_count(&t), 2);

    /* Closing one handle leaves the other usable. */
    CHECK_EQ_INT(nx_ht_close(&t, h), NX_OK);
    CHECK_EQ_INT(nx_ht_lookup(&t, ro, NX_OBJ_VMO, NX_RIGHT_READ, 0, 0), NX_OK);
    nx_ht_clear(&t);
    CHECK_EQ_INT(vmo.refs, 1);
}

static void test_take(void)
{
    static struct nx_handle_table a, b;
    struct nx_object vmo = make(NX_OBJ_VMO), *o = 0;
    uint32_t h, ro, hb, rights;
    nx_ht_init(&a);
    nx_ht_init(&b);
    CHECK_EQ_INT(nx_ht_install(&a, &vmo, VMO_ALL, &h), NX_OK);
    CHECK_EQ_INT(nx_ht_dup(&a, h, NX_RIGHT_READ, &ro), NX_OK); /* no TRANSFER */

    /* Refused transfers leave the handle in place. */
    CHECK_EQ_INT(nx_ht_take(&a, ro, NX_RIGHT_READ, &o), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_take(&a, h, VMO_ALL | NX_RIGHT_RECV, &o), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_lookup(&a, h, NX_OBJ_VMO, VMO_ALL, 0, 0), NX_OK);
    CHECK_EQ_INT(nx_ht_take(&a, h ^ 0x100, NX_RIGHT_READ, &o), NX_EBADHANDLE);
    CHECK_EQ_INT(vmo.refs, 3);

    /* A transfer moves the reference: the sender's handle is gone, the
     * reference count does not change until the receiver installs it. */
    CHECK_EQ_INT(nx_ht_take(&a, h, NX_RIGHT_READ | NX_RIGHT_MAP, &o), NX_OK);
    CHECK(o == &vmo);
    CHECK_EQ_INT(vmo.refs, 3);
    CHECK_EQ_INT(nx_ht_lookup(&a, h, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_close(&a, h), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_count(&a), 1);
    /* The receiving side (as in sys_ipc_recv): install, then drop the
     * message's reference. */
    CHECK_EQ_INT(nx_ht_install(&b, o, NX_RIGHT_READ | NX_RIGHT_MAP, &hb), NX_OK);
    nx_obj_unref(o);
    CHECK_EQ_INT(vmo.refs, 3);
    CHECK_EQ_INT(nx_ht_lookup(&b, hb, NX_OBJ_VMO, 0, 0, &rights), NX_OK);
    CHECK_EQ_INT(rights, NX_RIGHT_READ | NX_RIGHT_MAP);
    CHECK_EQ_INT(nx_ht_lookup(&b, hb, NX_OBJ_VMO, NX_RIGHT_WRITE, 0, 0), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_dup(&b, hb, NX_RIGHT_READ, &ro), NX_EACCESS);
    CHECK_EQ_INT(nx_ht_take(&b, hb, NX_RIGHT_READ, &o), NX_EACCESS); /* no TRANSFER either */
    nx_ht_clear(&a);
    nx_ht_clear(&b);
    CHECK_EQ_INT(vmo.refs, 1);
}

static void test_generations(void)
{
    static struct nx_handle_table t;
    struct nx_object x = make(NX_OBJ_VMO), y = make(NX_OBJ_ENDPOINT);
    uint32_t hx, hy, first, h;
    nx_ht_init(&t);

    /* A closed handle stays invalid after its slot is reused, even by an
     * object of another type. */
    CHECK_EQ_INT(nx_ht_install(&t, &x, NX_RIGHT_READ, &hx), NX_OK);
    CHECK_EQ_INT(nx_ht_close(&t, hx), NX_OK);
    CHECK_EQ_INT(nx_ht_install(&t, &y, NX_RIGHT_SEND, &hy), NX_OK);
    CHECK_EQ_INT(slot_of(hy), slot_of(hx));
    CHECK(hy != hx);
    CHECK_EQ_INT(nx_ht_lookup(&t, hx, NX_OBJ_ANY, 0, 0, 0), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_close(&t, hx), NX_EBADHANDLE);
    CHECK_EQ_INT(nx_ht_lookup(&t, hy, NX_OBJ_ENDPOINT, NX_RIGHT_SEND, 0, 0), NX_OK);
    CHECK_EQ_INT(nx_ht_close(&t, hy), NX_OK);

    /* 1000 install/close cycles on one slot: every value is new. */
    enum { N = 1000 };
    static uint32_t seen[N];
    int dups = 0;
    first = 0;
    int failed_calls = 0;
    for (int i = 0; i < N; i++) {
        failed_calls += nx_ht_install(&t, &x, NX_RIGHT_READ, &h) != NX_OK;
        if (i == 0)
            first = slot_of(h);
        dups += slot_of(h) != first;
        for (int j = 0; j < i; j++)
            dups += seen[j] == h;
        seen[i] = h;
        failed_calls += nx_ht_close(&t, h) != NX_OK;
    }
    CHECK_EQ_INT(failed_calls, 0);
    CHECK_EQ_INT(dups, 0);
    CHECK_EQ_INT(x.refs, 1);

    /* The generation wraps from NX_HANDLE_GEN_MAX to 1: a handle value is
     * never 0 and never has bit 31 set. */
    t.e[0].gen = NX_HANDLE_GEN_MAX;
    CHECK_EQ_INT(nx_ht_install(&t, &x, NX_RIGHT_READ, &h), NX_OK);
    CHECK_EQ_INT(h, NX_HANDLE_GEN_MAX << 8 | 1u);
    CHECK_EQ_INT(h >> 31, 0);
    CHECK_EQ_INT(nx_ht_close(&t, h), NX_OK);
    CHECK_EQ_INT(t.e[0].gen, 1);
    CHECK_EQ_INT(nx_ht_install(&t, &x, NX_RIGHT_READ, &h), NX_OK);
    CHECK_EQ_INT(h, 1u << 8 | 1u);
    CHECK_EQ_INT(nx_ht_close(&t, h), NX_OK);
}

static void test_full_and_clear(void)
{
    static struct nx_handle_table t;
    struct nx_object x = make(NX_OBJ_VMO);
    uint32_t h[NX_HANDLE_SLOTS], extra = 0x1234;
    nx_ht_init(&t);
    int failed_calls = 0;
    for (uint32_t i = 0; i < NX_HANDLE_SLOTS; i++)
        failed_calls += nx_ht_install(&t, &x, NX_RIGHT_READ | NX_RIGHT_DUPLICATE, &h[i]) != NX_OK;
    CHECK_EQ_INT(failed_calls, 0);
    CHECK_EQ_INT(nx_ht_count(&t), NX_HANDLE_SLOTS);
    CHECK_EQ_INT(x.refs, 1 + NX_HANDLE_SLOTS);
    CHECK_EQ_INT(nx_ht_install(&t, &x, NX_RIGHT_READ, &extra), NX_ENOMEM);
    CHECK_EQ_INT(nx_ht_dup(&t, h[0], NX_RIGHT_READ, &extra), NX_ENOMEM);
    CHECK_EQ_INT(extra, 0x1234);
    CHECK_EQ_INT(x.refs, 1 + NX_HANDLE_SLOTS);
    /* A freed slot is found again. */
    CHECK_EQ_INT(nx_ht_close(&t, h[17]), NX_OK);
    CHECK_EQ_INT(nx_ht_install(&t, &x, NX_RIGHT_READ, &extra), NX_OK);
    CHECK_EQ_INT(slot_of(extra), slot_of(h[17]));

    /* Clearing the table (task teardown) drops every reference once; the
     * object is destroyed when its last reference goes. */
    destroyed = 0;
    nx_ht_clear(&t);
    CHECK_EQ_INT(nx_ht_count(&t), 0);
    CHECK_EQ_INT(x.refs, 1);
    CHECK_EQ_INT(destroyed, 0);
    int still_valid = 0;
    for (uint32_t i = 0; i < NX_HANDLE_SLOTS; i++)
        still_valid += nx_ht_lookup(&t, h[i], NX_OBJ_ANY, 0, 0, 0) != NX_EBADHANDLE;
    CHECK_EQ_INT(still_valid, 0);
    nx_obj_unref(&x);
    CHECK_EQ_INT(destroyed, 1);
    CHECK(last_destroyed == &x);
    nx_ht_clear(&t); /* empty table: nothing happens */
    CHECK_EQ_INT(destroyed, 1);
}

static void test_refcounts(void)
{
    static struct nx_handle_table t;
    struct nx_object x = make(NX_OBJ_TASK);
    uint32_t h;
    nx_ht_init(&t);
    destroyed = 0;
    CHECK_EQ_INT(nx_ht_install(&t, &x, NX_RIGHT_MANAGE, &h), NX_OK);
    nx_obj_unref(&x); /* creator lets go: the handle is the only reference */
    CHECK_EQ_INT(destroyed, 0);
    CHECK_EQ_INT(nx_ht_close(&t, h), NX_OK);
    CHECK_EQ_INT(destroyed, 1);
    CHECK_EQ_INT(x.refs, 0);

    /* Objects with a zero count and no destructor (the static Sovereign
     * object) survive any number of unrefs. */
    struct nx_object sov = {NX_OBJ_SOVEREIGN, 0, 0};
    nx_obj_unref(&sov);
    CHECK_EQ_INT(sov.refs, 0);
    CHECK_EQ_INT(nx_ht_install(&t, &sov, NX_RIGHT_SOVEREIGN, &h), NX_OK);
    CHECK_EQ_INT(nx_ht_close(&t, h), NX_OK);
    CHECK_EQ_INT(sov.refs, 0);
}

void test_handle(void)
{
    test_install_lookup();
    test_dup_rights();
    test_take();
    test_generations();
    test_full_and_clear();
    test_refcounts();
}
