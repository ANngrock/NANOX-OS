/*
 * M3 test modes (nanox.test=m3-*): the Cognitive Core executor bin/core
 * serves NCI requests of the host bridge over COM2.  The kernel thread
 * kmain starts it with the Sovereign handle and the channel handle, waits
 * for it to end, cleans up and checks the resources, and prints the
 * verdict.  Scenarios: docs/m3-core.md.
 */
#ifndef NANOX_KERNEL_M3TEST_H
#define NANOX_KERNEL_M3TEST_H

__attribute__((noreturn)) void nx_m3_serve(void);
__attribute__((noreturn)) void nx_m3_kill_noop(void);
__attribute__((noreturn)) void nx_m3_events_off(void);
__attribute__((noreturn)) void nx_m3_nodedup(void);

/* The controller shared by the M3 and M4 modes: starts bin/core with the
 * Sovereign handle, the bridge channel (if `bridge`) and the data disk (if
 * `blk`), waits for it, cleans up, checks the resources, prints the
 * verdict from its exit code. */
struct nx_blkdev;
struct nx_core_opts {
    const char *mode;
    uint64_t flags; /* bin/core flags (abi/nanox/m3.h, m4.h) */
    int bridge;
    struct nx_blkdev *blk;
};
__attribute__((noreturn)) void nx_core_session(const struct nx_core_opts *o);

#endif
