/*
 * Kernel state event log (M3).  A ring of the last NX_EVLOG_SIZE events with
 * gapless sequence numbers starting at 1.  A reader asks for the events
 * from a sequence number on; if some of them were already overwritten, it
 * gets the oldest ones still stored and can see the gap in `seq`.  Pure
 * code, compiled into the kernel and into tests/host.  Design:
 * docs/m3-core.md.
 */
#ifndef NANOX_KERNEL_OBJ_EVENT_H
#define NANOX_KERNEL_OBJ_EVENT_H

#include <stdint.h>

#include <nanox/syscall.h>

#define NX_EVLOG_SIZE 128u

struct nx_evlog {
    struct nx_event ev[NX_EVLOG_SIZE];
    uint64_t next; /* sequence number of the next event (first event: 1) */
    int disabled;  /* fault injection: drop every event (nanox.test=m3-events-off) */
};

void nx_evlog_init(struct nx_evlog *l);
/* Appends an event and returns its sequence number (0 when disabled). */
uint64_t nx_evlog_push(struct nx_evlog *l, uint64_t tick, uint32_t type, uint32_t task,
                       int64_t arg);
/* Copies up to `cap` events with seq >= since (oldest first) to out and
 * returns how many.  Events older than the ring are skipped; the caller
 * detects the loss as out[0].seq > since. */
uint32_t nx_evlog_read(const struct nx_evlog *l, uint64_t since, struct nx_event *out,
                       uint32_t cap);
/* Oldest sequence number still stored (== next when empty). */
uint64_t nx_evlog_oldest(const struct nx_evlog *l);

#endif
