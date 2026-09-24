/*
 * M2 test modes (nanox.test=m2-*): user tasks, preemptive scheduling and
 * IPC.  Each mode starts the scheduler, runs test programs from the
 * initramfs as user tasks, checks the outcome in the kernel and ends the
 * run with TEST PASS or TEST FAIL.  Scenarios: docs/m2-kernel.md.
 */
#ifndef NANOX_KERNEL_M2TEST_H
#define NANOX_KERNEL_M2TEST_H

#include <stdint.h>

/* Initramfs that holds the programs (validated by kernel_main). */
void nx_m2_set_initramfs(const uint8_t *base, uint64_t size);
/* The same archive for other users (M3 task spawn); base NULL before boot set it. */
void nx_initramfs_get(const uint8_t **base, uint64_t *size);

__attribute__((noreturn)) void nx_m2_user(void);
__attribute__((noreturn)) void nx_m2_sched(void);
__attribute__((noreturn)) void nx_m2_sched_nopreempt(void);
__attribute__((noreturn)) void nx_m2_ipc(void);
__attribute__((noreturn)) void nx_m2_ipc_overgrant(void);

/* Helpers of the test controllers, shared with kernel/m3test.c. */
struct nx_task;
struct nx_test_snapshot {
    uint64_t free_pages, tables;
    uint32_t tasks, vmos, endpoints;
};
__attribute__((noreturn)) void nx_test_pass(void);
void nx_test_snapshot(struct nx_test_snapshot *out);
/* Creates (does not start) a user task from an initramfs program and keeps a
 * reference to it; TEST FAIL if that is impossible. */
struct nx_task *nx_test_spawn(const char *mode, const char *name, const char *path,
                              const uint64_t args[4]);

#endif
