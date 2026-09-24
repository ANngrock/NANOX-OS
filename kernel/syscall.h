/*
 * System call layer (kernel/syscall.c).  nx_syscall itself is declared in
 * task.h; this header carries the switches set by the M2 test modes
 * (docs/m2-kernel.md).  All are 0 unless a nanox.test= mode sets them.
 */
#ifndef NANOX_KERNEL_SYSCALL_H
#define NANOX_KERNEL_SYSCALL_H

/* Fault injection: copy user buffers without range/page-table checks.  No
 * test mode sets it yet (syscall validation, M2 criterion 4, is not
 * covered by scenarios). */
extern int nx_inject_uaccess_unchecked;
/* Fault injection: deliver transferred handles with all of the sender's
 * rights (negative control nanox.test=m2-ipc-overgrant). */
extern int nx_inject_ipc_overgrant;
/* Fault injection: NX_SYS_TASK_KILL reports success without terminating
 * the task (negative control nanox.test=m3-kill-noop). */
extern int nx_inject_kill_noop;
/* Print one serial line per IPC send/receive (nanox.test=m2-ipc*). */
extern int nx_trace_ipc;

#endif
