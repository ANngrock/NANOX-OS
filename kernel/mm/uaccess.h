/*
 * Access to user memory from the kernel (M2).  The kernel never dereferences
 * a user pointer: it checks that [va, va+len) lies in the user half, walks
 * the task's page tables for every page (present, user, writable when
 * writing) and copies through env->virt (the physmap).  Pure code; tested
 * in tests/host.
 */
#ifndef NANOX_KERNEL_MM_UACCESS_H
#define NANOX_KERNEL_MM_UACCESS_H

#include <stdint.h>

#include "pt.h"

/* Returns NX_OK or NX_EFAULT (abi/nanox/syscall.h). */
int nx_uaccess_check(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t len,
                     int write);
int nx_uaccess_copy_in(const struct nx_pt_env *env, uint64_t root, void *dst, uint64_t va,
                       uint64_t len);
int nx_uaccess_copy_out(const struct nx_pt_env *env, uint64_t root, uint64_t va,
                        const void *src, uint64_t len);
/* [va, va+len) inside [NX_USER_BASE, NX_USER_TOP) without wrapping. */
int nx_user_range_ok(uint64_t va, uint64_t len);

#endif
