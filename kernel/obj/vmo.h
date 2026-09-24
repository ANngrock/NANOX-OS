/*
 * Kernel object pools (M2): memory objects (VMO), IPC endpoints and the
 * single Sovereign object.  Pools are fixed-size arrays; an object returns to
 * its pool when the last reference (handle, mapping, queued message) is
 * dropped.  Design: docs/m2-kernel.md.
 */
#ifndef NANOX_KERNEL_OBJ_VMO_H
#define NANOX_KERNEL_OBJ_VMO_H

#include <stdint.h>

#include <nanox/syscall.h>

#include "handle.h"
#include "ipc.h"

#define NX_VMO_POOL 32u
#define NX_EP_POOL 16u

struct nx_vmo {
    struct nx_object base; /* type NX_OBJ_VMO */
    uint32_t used;
    uint32_t pages;
    uint64_t phys[NX_VMO_MAX_PAGES]; /* zeroed pages, freed on destroy */
};

/* New zero-filled memory object with one reference; NULL and *err on failure
 * (NX_EINVAL for 0 or more than NX_VMO_MAX_PAGES pages, NX_ENOMEM). */
struct nx_vmo *nx_vmo_create(uint32_t pages, int *err);
/* New empty endpoint with one reference, or NULL when the pool is empty. */
struct nx_endpoint *nx_ep_create(void);
/* The Sovereign object (created once, never destroyed). */
struct nx_object *nx_sovereign(void);

/* Objects currently allocated from the pools (leak checks). */
uint32_t nx_vmo_live(void);
uint32_t nx_ep_live(void);

#endif
