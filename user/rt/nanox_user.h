/*
 * Minimal runtime of NANOX user programs (M2): system call wrappers
 * (ABI: abi/nanox/syscall.h, docs/m2-kernel.md) and a small formatter that
 * writes through NX_SYS_DEBUG_WRITE.  Freestanding: no libc.
 */
#ifndef NANOX_USER_H
#define NANOX_USER_H

#include <stdint.h>

#include <nanox/syscall.h>

/* Program entry, called by _start (start.S) with the four arguments the
 * kernel passed in RDI, RSI, RDX, RCX; the result is the exit code. */
int64_t umain(uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3);

/* int $0x80: RAX = number, arguments in RDI, RSI, RDX, R10, R8, result in
 * RAX.  The kernel restores every other register. */
static inline int64_t nx_sys(uint64_t n, uint64_t a, uint64_t b, uint64_t c, uint64_t d,
                             uint64_t e)
{
    register uint64_t r10 __asm__("r10") = d;
    register uint64_t r8 __asm__("r8") = e;
    int64_t ret;
    __asm__ volatile("int $0x80"
                     : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8)
                     : "memory");
    return ret;
}

static inline int64_t nx_debug_write(const void *buf, uint64_t len)
{
    return nx_sys(NX_SYS_DEBUG_WRITE, (uint64_t)(uintptr_t)buf, len, 0, 0, 0);
}

__attribute__((noreturn)) static inline void nx_exit(int64_t code)
{
    nx_sys(NX_SYS_EXIT, (uint64_t)code, 0, 0, 0, 0);
    __builtin_unreachable();
}

static inline int64_t nx_yield(void)
{
    return nx_sys(NX_SYS_YIELD, 0, 0, 0, 0, 0);
}

static inline int64_t nx_task_self(void)
{
    return nx_sys(NX_SYS_TASK_SELF, 0, 0, 0, 0, 0);
}

static inline int64_t nx_handle_close(uint64_t h)
{
    return nx_sys(NX_SYS_HANDLE_CLOSE, h, 0, 0, 0, 0);
}

static inline int64_t nx_handle_dup(uint64_t h, uint64_t rights)
{
    return nx_sys(NX_SYS_HANDLE_DUP, h, rights, 0, 0, 0);
}

static inline int64_t nx_vmo_create(uint64_t pages)
{
    return nx_sys(NX_SYS_VMO_CREATE, pages, 0, 0, 0, 0);
}

static inline int64_t nx_vmo_map(uint64_t h, uint64_t va, uint64_t prot)
{
    return nx_sys(NX_SYS_VMO_MAP, h, va, prot, 0, 0);
}

static inline int64_t nx_ipc_send(uint64_t ep, const void *buf, uint64_t len, uint64_t h,
                                  uint64_t rights)
{
    return nx_sys(NX_SYS_IPC_SEND, ep, (uint64_t)(uintptr_t)buf, len, h, rights);
}

static inline int64_t nx_ipc_recv(uint64_t ep, void *buf, uint64_t cap, struct nx_ipc_info *info)
{
    return nx_sys(NX_SYS_IPC_RECV, ep, (uint64_t)(uintptr_t)buf, cap,
                  (uint64_t)(uintptr_t)info, 0);
}

static inline int64_t nx_sov_task_open(uint64_t sov, uint64_t id)
{
    return nx_sys(NX_SYS_SOV_TASK_OPEN, sov, id, 0, 0, 0);
}

static inline int64_t nx_task_kill(uint64_t th)
{
    return nx_sys(NX_SYS_TASK_KILL, th, 0, 0, 0, 0);
}

/* ---- M3 ---- */

static inline int64_t nx_sleep(uint64_t ticks)
{
    return nx_sys(NX_SYS_SLEEP, ticks, 0, 0, 0, 0);
}

static inline int64_t nx_sys_info(struct nx_sys_info *info)
{
    return nx_sys(NX_SYS_SYS_INFO, (uint64_t)(uintptr_t)info, 0, 0, 0, 0);
}

static inline int64_t nx_sov_task_spawn(uint64_t sov, const char *path, uint64_t len,
                                        const uint64_t args[4])
{
    return nx_sys(NX_SYS_SOV_TASK_SPAWN, sov, (uint64_t)(uintptr_t)path, len,
                  (uint64_t)(uintptr_t)args, 0);
}

static inline int64_t nx_task_info(uint64_t th, struct nx_task_info *info)
{
    return nx_sys(NX_SYS_TASK_INFO, th, (uint64_t)(uintptr_t)info, 0, 0, 0);
}

static inline int64_t nx_sov_task_list(uint64_t sov, struct nx_task_info *list, uint64_t cap)
{
    return nx_sys(NX_SYS_SOV_TASK_LIST, sov, (uint64_t)(uintptr_t)list, cap, 0, 0);
}

static inline int64_t nx_sov_event_read(uint64_t sov, uint64_t since, struct nx_event *ev,
                                        uint64_t cap)
{
    return nx_sys(NX_SYS_SOV_EVENT_READ, sov, since, (uint64_t)(uintptr_t)ev, cap, 0);
}

static inline int64_t nx_chan_read(uint64_t ch, void *buf, uint64_t cap, uint64_t timeout)
{
    return nx_sys(NX_SYS_CHAN_READ, ch, (uint64_t)(uintptr_t)buf, cap, timeout, 0);
}

static inline int64_t nx_chan_write(uint64_t ch, const void *buf, uint64_t len)
{
    return nx_sys(NX_SYS_CHAN_WRITE, ch, (uint64_t)(uintptr_t)buf, len, 0, 0);
}

/* Current privilege level (low bits of CS). */
static inline uint64_t u_cpl(void)
{
    uint16_t cs;
    __asm__ volatile("mov %%cs, %0" : "=r"(cs));
    return cs & 3u;
}

/* printf subset: %s %c %d %u %x (with l / ll), '0' flag and width.  One
 * call is one NX_SYS_DEBUG_WRITE (at most NX_DEBUG_WRITE_MAX bytes; longer
 * output is truncated).  Returns the result of the system call. */
__attribute__((format(printf, 1, 2))) int64_t u_printf(const char *fmt, ...);
uint64_t u_strlen(const char *s);
int u_memeq(const void *a, const void *b, uint64_t n);
/* Name of an NX_E* code for messages ("EACCESS" for -NX_EACCESS, "ok" for 0). */
const char *u_err(int64_t r);

#endif
