/*
 * Kernel objects and per-task handle tables (M2).  Pure code (no scheduler,
 * no allocator), compiled into the kernel and into tests/host.
 *
 * Handle value: bits 0-7 = slot index + 1 (0 is never a valid handle),
 * bits 8-30 = generation of the slot, bit 31 = 0.  Closing or moving a
 * handle bumps the slot generation, so an old value is rejected with
 * NX_EBADHANDLE even after the slot is reused (ARCHITECTURE.md §6: an old
 * identifier must not reach a new object).  Rights can only be kept or
 * dropped when a handle is duplicated or transferred, never added.
 */
#ifndef NANOX_KERNEL_OBJ_HANDLE_H
#define NANOX_KERNEL_OBJ_HANDLE_H

#include <stdint.h>

#include <nanox/syscall.h>

#define NX_HANDLE_SLOTS 32u
#define NX_HANDLE_GEN_MAX 0x7FFFFFu

enum nx_obj_type {
    NX_OBJ_ANY = 0, /* lookup wildcard */
    NX_OBJ_TASK = 1,
    NX_OBJ_ENDPOINT = 2,
    NX_OBJ_VMO = 3,
    NX_OBJ_SOVEREIGN = 4,
};

struct nx_object {
    uint32_t type;
    uint32_t refs;
    /* Called when the last reference goes away (may be NULL). */
    void (*destroy)(struct nx_object *o);
};

void nx_obj_ref(struct nx_object *o);
void nx_obj_unref(struct nx_object *o);

struct nx_handle_entry {
    struct nx_object *obj; /* NULL: free */
    uint32_t rights;
    uint32_t gen; /* 1..NX_HANDLE_GEN_MAX */
};

struct nx_handle_table {
    struct nx_handle_entry e[NX_HANDLE_SLOTS];
};

void nx_ht_init(struct nx_handle_table *t);
/* Installs obj (taking a reference) with `rights`.  NX_OK or NX_ENOMEM. */
int nx_ht_install(struct nx_handle_table *t, struct nx_object *obj, uint32_t rights, uint32_t *h);
/* Resolves h: NX_EBADHANDLE, NX_EWRONGTYPE (type != NX_OBJ_ANY), NX_EACCESS
 * (missing any of `need`).  No reference is taken. */
int nx_ht_lookup(const struct nx_handle_table *t, uint32_t h, uint32_t type, uint32_t need,
                 struct nx_object **obj, uint32_t *rights);
int nx_ht_close(struct nx_handle_table *t, uint32_t h);
/* New handle to the same object with `rights` (subset, needs DUPLICATE). */
int nx_ht_dup(struct nx_handle_table *t, uint32_t h, uint32_t rights, uint32_t *out);
/* Removes h for transfer (needs TRANSFER, `rights` subset); the caller now
 * owns the reference returned in *obj. */
int nx_ht_take(struct nx_handle_table *t, uint32_t h, uint32_t rights, struct nx_object **obj);
/* Closes every handle. */
void nx_ht_clear(struct nx_handle_table *t);
uint32_t nx_ht_count(const struct nx_handle_table *t);

#endif
