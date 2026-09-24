/*
 * NANOX system call ABI, version 1 (M2).  Shared by the kernel and user
 * programs; normative description in docs/m2-kernel.md.
 *
 * Entry: `int $0x80`.  RAX = call number, arguments in RDI, RSI, RDX, R10,
 * R8; result in RAX (>= 0 success, < 0 one of NX_E*).  All other registers
 * are preserved.  The kernel never dereferences a user pointer directly: it
 * checks the range and walks the caller's page tables first, so a bad
 * pointer yields NX_EFAULT instead of a kernel fault.
 */
#ifndef NANOX_ABI_SYSCALL_H
#define NANOX_ABI_SYSCALL_H

#include <stdint.h>

#define NX_SYSCALL_VECTOR 0x80

/* User half: everything below NX_USER_BASE (PML4 slot 0) belongs to the
 * kernel image, everything from NX_USER_TOP up is non-canonical or kernel. */
#define NX_USER_BASE 0x0000008000000000ull
#define NX_USER_TOP 0x0000800000000000ull
#define NX_USER_STACK_TOP 0x00007FFFFFFF0000ull
#define NX_USER_STACK_PAGES 16u

enum nx_syscall {
    NX_SYS_DEBUG_WRITE = 1,  /* (buf, len) -> len; appends to the task log */
    NX_SYS_EXIT = 2,         /* (code) -> does not return */
    NX_SYS_YIELD = 3,        /* () -> 0 */
    NX_SYS_TASK_SELF = 4,    /* () -> task id */
    NX_SYS_HANDLE_CLOSE = 5, /* (h) -> 0 */
    NX_SYS_HANDLE_DUP = 6,   /* (h, rights) -> new handle, rights must be a subset */
    NX_SYS_VMO_CREATE = 7,   /* (pages) -> handle with all memory rights */
    NX_SYS_VMO_MAP = 8,      /* (h, va, prot) -> 0; prot: NX_PROT_* */
    NX_SYS_IPC_SEND = 9,     /* (ep, buf, len, h, rights) -> 0; h == 0: no handle */
    NX_SYS_IPC_RECV = 10,    /* (ep, buf, cap, info*) -> message length; blocks */
    NX_SYS_SOV_TASK_OPEN = 11, /* (sovereign h, task id) -> task handle */
    NX_SYS_TASK_READ = 12,   /* (task h, va, buf, len) -> len */
    NX_SYS_TASK_KILL = 13,   /* (task h) -> 0 */
    NX_SYS__COUNT
};

/* Errors (negated in RAX). */
enum nx_error {
    NX_OK = 0,
    NX_ENOSYS = 1,     /* unknown call number */
    NX_EINVAL = 2,     /* malformed argument (size, alignment, flags) */
    NX_EFAULT = 3,     /* user buffer not mapped / not accessible as required */
    NX_EBADHANDLE = 4, /* no such handle (never issued, closed, or stale generation) */
    NX_EWRONGTYPE = 5, /* handle refers to an object of another type */
    NX_EACCESS = 6,    /* handle lacks a required right */
    NX_ENOMEM = 7,     /* out of memory or table slots */
    NX_EFULL = 8,      /* endpoint queue full */
    NX_EEXISTS = 9,    /* address range already mapped */
    NX_ERANGE = 10,    /* address outside the user half */
    NX_EDEAD = 11,     /* target task has exited */
    NX_E__COUNT
};

/* Handle rights.  A right can be dropped (dup, transfer) but never added. */
#define NX_RIGHT_READ (1u << 0)      /* read object contents / map readable */
#define NX_RIGHT_WRITE (1u << 1)     /* write object contents / map writable */
#define NX_RIGHT_MAP (1u << 2)       /* map a memory object */
#define NX_RIGHT_TRANSFER (1u << 3)  /* send the handle through IPC */
#define NX_RIGHT_DUPLICATE (1u << 4) /* create a handle with fewer rights */
#define NX_RIGHT_SEND (1u << 5)      /* endpoint: send */
#define NX_RIGHT_RECV (1u << 6)      /* endpoint: receive */
#define NX_RIGHT_INSPECT (1u << 7)   /* task: read its memory */
#define NX_RIGHT_MANAGE (1u << 8)    /* task: terminate */
#define NX_RIGHT_SOVEREIGN (1u << 9) /* sovereign object: open any task */
#define NX_RIGHTS_ALL 0x3FFu

#define NX_PROT_READ 1u
#define NX_PROT_WRITE 2u

#define NX_IPC_MSG_MAX 128u
#define NX_DEBUG_WRITE_MAX 256u
#define NX_VMO_MAX_PAGES 64u

/* Filled by NX_SYS_IPC_RECV.  sender_task is set by the kernel. */
struct nx_ipc_info {
    uint32_t len;
    uint32_t handle; /* 0: no handle transferred */
    uint32_t rights;
    uint32_t sender_task;
};

#endif
