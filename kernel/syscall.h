/*
 * System call layer (kernel/syscall.c).  nx_syscall itself is declared in
 * task.h; this header only carries the fault-injection switches used by the
 * negative M2 scenarios (docs/m2-kernel.md).  Both are 0 in every normal
 * boot and are set only by the corresponding nanox.test= mode.
 */
#ifndef NANOX_KERNEL_SYSCALL_H
#define NANOX_KERNEL_SYSCALL_H

/* Copy user buffers without range/page-table checks (m2-uaccess-unchecked). */
extern int nx_inject_uaccess_unchecked;
/* Deliver transferred handles with all of the sender's rights
 * (m2-ipc-overgrant). */
extern int nx_inject_ipc_overgrant;

#endif
