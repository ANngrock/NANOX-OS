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

__attribute__((noreturn)) void nx_m2_user(void);
__attribute__((noreturn)) void nx_m2_sched(void);
__attribute__((noreturn)) void nx_m2_sched_nopreempt(void);
__attribute__((noreturn)) void nx_m2_ipc(void);
__attribute__((noreturn)) void nx_m2_ipc_overgrant(void);

#endif
