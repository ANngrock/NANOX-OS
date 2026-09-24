/*
 * M4 test modes (nanox.test=m4-*): virtio-blk, the persistent store of
 * bin/core, crash points.  Scenarios: docs/m4-store.md.
 */
#ifndef NANOX_KERNEL_M4TEST_H
#define NANOX_KERNEL_M4TEST_H

__attribute__((noreturn)) void nx_m4_blk(void);
__attribute__((noreturn)) void nx_m4_serve(void);
__attribute__((noreturn)) void nx_m4_work(void);
__attribute__((noreturn)) void nx_m4_check(void);

#endif
