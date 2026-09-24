/*
 * NANOX system call ABI, version 1 (M2, extended in M3 by calls 14-21 and
 * in M4 by calls 22-25).  Shared by the kernel and user programs; normative
 * description in docs/m2-kernel.md (calls 1-13), docs/m3-core.md (calls
 * 14-21) and docs/m4-store.md (calls 22-25).
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
    /* M3: control interface of the Cognitive Core executor. */
    NX_SYS_SLEEP = 14,       /* (ticks <= NX_SLEEP_MAX) -> 0 */
    NX_SYS_SYS_INFO = 15,    /* (info*) -> 0; struct nx_sys_info */
    NX_SYS_SOV_TASK_SPAWN = 16, /* (sovereign h, path*, path_len, args*) -> task handle */
    NX_SYS_TASK_INFO = 17,   /* (task h, info*) -> 0; struct nx_task_info, needs INSPECT */
    NX_SYS_SOV_TASK_LIST = 18, /* (sovereign h, info*, cap) -> number of tasks */
    NX_SYS_SOV_EVENT_READ = 19, /* (sovereign h, since, ev*, cap) -> events copied */
    NX_SYS_CHAN_READ = 20,   /* (channel h, buf, cap, timeout ticks) -> bytes (0: timeout) */
    NX_SYS_CHAN_WRITE = 21,  /* (channel h, buf, len) -> len */
    /* M4: block device (4096-byte blocks). */
    NX_SYS_BLK_INFO = 22,    /* (blk h, info*) -> 0; struct nx_blk_info, needs READ */
    NX_SYS_BLK_READ = 23,    /* (blk h, block, count <= NX_BLK_IO_MAX, buf) -> count; READ */
    NX_SYS_BLK_WRITE = 24,   /* (blk h, block, count <= NX_BLK_IO_MAX, buf) -> count; WRITE */
    NX_SYS_BLK_FLUSH = 25,   /* (blk h) -> 0 once every completed write is durable; WRITE */
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
    NX_ENOENT = 12,    /* M3: no such program in the initramfs */
    NX_EIO = 13,       /* M4: the device reported an error or did not complete */
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

/* ---- M3 (docs/m3-core.md) ------------------------------------------------ */

#define NX_SLEEP_MAX 1000u        /* ticks, 10 s at 100 Hz */
#define NX_SPAWN_PATH_MAX 64u
#define NX_CHAN_IO_MAX 512u
#define NX_CHAN_TIMEOUT_MAX 6000u /* ticks */

/* Task states as reported by NX_SYS_TASK_INFO (same values as the kernel's). */
enum nx_task_state_abi {
    NX_TS_READY = 1,
    NX_TS_RUNNING = 2,
    NX_TS_BLOCKED = 3, /* waiting, sleeping, or created and not started */
    NX_TS_DEAD = 4,    /* ended, resources not yet released */
    NX_TS_REAPED = 5,  /* ended and released */
};

/* How a task ended (nx_task_info.end). */
enum nx_task_end_abi {
    NX_TE_NONE = 0,
    NX_TE_EXIT = 1,
    NX_TE_FAULT = 2,
    NX_TE_KILLED = 3, /* NX_SYS_TASK_KILL */
    NX_TE_KERNEL = 4, /* terminated by the kernel */
};

#define NX_TASK_INFO_USER 1u /* flags: user task (else kernel thread) */

struct nx_task_info {
    uint32_t id;    /* unique within a boot, never reused */
    uint32_t state; /* NX_TS_* */
    uint32_t end;   /* NX_TE_* */
    uint32_t flags; /* NX_TASK_INFO_* */
    int64_t exit_code;
    uint64_t ticks;    /* timer ticks spent running */
    uint64_t syscalls;
    uint64_t rev;      /* bumped on every lifecycle change (one event each) */
    uint64_t created_tick, ended_tick;
    uint32_t killer_id;
    uint32_t reserved;
    char name[16];     /* NUL-terminated */
};

struct nx_sys_info {
    uint64_t boot_id;  /* random per boot: identifiers of earlier boots are stale */
    uint64_t ticks;    /* timer ticks since the scheduler started counting */
    uint32_t hz;
    uint32_t tasks;    /* live task slots */
    uint64_t free_pages, managed_pages, page_tables;
    uint64_t events_next; /* sequence number the next event will get */
    uint32_t next_task_id; /* ids below it were issued in this boot */
    uint32_t reserved;
};

/* Kernel state events (NX_SYS_SOV_EVENT_READ). */
enum nx_event_type {
    NX_EV_TASK_CREATED = 1,
    NX_EV_TASK_STARTED = 2,
    NX_EV_TASK_EXITED = 3, /* arg = exit code */
    NX_EV_TASK_KILLED = 4, /* arg = killer task id (0: kernel) */
    NX_EV_TASK_FAULTED = 5, /* arg = vector */
    NX_EV_TASK_REAPED = 6,
};

struct nx_event {
    uint64_t seq; /* 1, 2, 3, ... without gaps */
    uint64_t tick;
    uint32_t type; /* NX_EV_* */
    uint32_t task;
    int64_t arg;
};

/* ---- M4 (docs/m4-store.md) ------------------------------------------------ */

#define NX_BLK_SIZE 4096u
#define NX_BLK_IO_MAX 8u

#define NX_BLK_INFO_READ_ONLY 1u
#define NX_BLK_INFO_FLUSH 2u     /* the device has a volatile cache and a flush command */
#define NX_BLK_INFO_TEST_CACHE 4u /* test layer: emulated volatile cache (M4 crash tests) */

struct nx_blk_info {
    uint64_t blocks;     /* capacity in NX_BLK_SIZE blocks */
    uint32_t block_size; /* NX_BLK_SIZE */
    uint32_t flags;      /* NX_BLK_INFO_* */
    uint64_t reads, writes, flushes;
    char serial[24];     /* virtio-blk GET_ID, NUL-terminated */
};

/* Filled by NX_SYS_IPC_RECV.  sender_task is set by the kernel. */
struct nx_ipc_info {
    uint32_t len;
    uint32_t handle; /* 0: no handle transferred */
    uint32_t rights;
    uint32_t sender_task;
};

#endif
