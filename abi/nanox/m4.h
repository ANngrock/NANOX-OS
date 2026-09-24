/*
 * Agreement between the kernel's M4 test modes (kernel/m4test.c,
 * nanox.test=m4-*), bin/core (user/core/persist.c) and the host tools that
 * read the data disk (tools/store/nxstore.py).  Not part of the system
 * call ABI.  Design: docs/m4-store.md.
 *
 * bin/core arguments in M4 modes: a0 = Sovereign handle, a1 = bridge
 * channel handle (0: no bridge), a2 = NX_M3_CORE_* / NX_M4_CORE_* flags,
 * a3 = block device handle of the data disk (0: no store, as in M3).
 */
#ifndef NANOX_ABI_M4_H
#define NANOX_ABI_M4_H

#include <stdint.h>

#define NX_M4_CORE_WORKLOAD 2u /* run the built-in workload (no bridge), then exit */
#define NX_M4_CORE_CHECK 4u    /* mount, recover, print the state, check it, exit */

/* bin/core exit code (in addition to abi/nanox/m3.h). */
#define NX_M4_CORE_STORE_FAILED 7 /* the store could not be mounted or checked */

/* Object kinds and names in the store (lib/store.c treats both as opaque). */
#define NX_M4_KIND_CONFIG 1u /* "cfg/<key>": the value bytes */
#define NX_M4_KIND_BLOB 2u   /* "blob/<name>": test data, see nx_m4_blob_byte */
#define NX_M4_KIND_TASKS 3u  /* "core/tasks": the task-engine records below */

#define NX_M4_TASKS_NAME "core/tasks"
#define NX_M4_TASKS_MAGIC "NXTASKS1"
#define NX_M4_TASKS_MAX 32u
#define NX_M4_RES_MAX 232u

/* Persisted task-engine record.  state: the engine's ACT_* value
 * (user/core/engine.h); a record found in a non-final state after a
 * restart becomes OUTCOME_UNKNOWN. */
struct nx_m4_task_rec {
    char id[33];
    char op[33];
    uint8_t state;
    uint8_t flags; /* NX_M4_REC_* */
    uint32_t res_len;
    uint64_t fp;      /* request fingerprint (nci_fingerprint) */
    uint64_t boot_id; /* boot in which the request was executed */
    char res[NX_M4_RES_MAX]; /* first response line ("RES <id> <STATE> ..."), not NUL-terminated */
};

#define NX_M4_REC_TRUNCATED 1u /* the response line was longer than NX_M4_RES_MAX */

struct nx_m4_tasks_hdr {
    char magic[8];
    uint32_t count;
    uint32_t rec_size; /* sizeof(struct nx_m4_task_rec) */
};

_Static_assert(sizeof(struct nx_m4_task_rec) == 320, "task record layout");
_Static_assert(sizeof(struct nx_m4_tasks_hdr) == 16, "tasks header layout");

/* Content of a blob of `size` bytes: byte i. */
static inline uint8_t nx_m4_blob_byte(uint32_t name_hash, uint32_t size, uint32_t i)
{
    uint32_t x = name_hash ^ (size * 2654435761u) ^ (i * 40503u);
    x ^= x >> 15;
    x *= 0x2C1B3C6Du;
    x ^= x >> 12;
    return (uint8_t)x;
}

/* FNV-1a of the blob name (without the "blob/" prefix). */
static inline uint32_t nx_m4_name_hash(const char *s)
{
    uint32_t h = 2166136261u;
    while (*s)
        h = (h ^ (uint8_t)*s++) * 16777619u;
    return h;
}

#endif
