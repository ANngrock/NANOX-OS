/*
 * System call dispatch (M2).  ABI: abi/nanox/syscall.h, rules:
 * docs/m2-kernel.md.
 *
 * Entered through the DPL-3 interrupt gate 0x80, so interrupts are disabled
 * for the whole call and the kernel is never preempted inside it.  Every
 * argument is treated as hostile: numbers are compared as full 64-bit
 * values, sizes are bounded before use, handles are resolved through the
 * caller's own table (type and rights checked), and user memory is only
 * touched through nx_uaccess_* (range check + page-table walk of the
 * caller's address space).  A failed call changes no kernel state.
 */
#include <nanox/printf.h>
#include <nanox/serial.h>
#include <nanox/string.h>
#include <nanox/syscall.h>

#include "arch/x86_64/cpu.h"
#include "arch/x86_64/timer.h"
#include "arch/x86_64/trap.h"
#include "chan.h"
#include "initramfs.h"
#include "kernel.h"
#include "m2test.h"
#include "mm/mm.h"
#include "mm/uaccess.h"
#include "obj/event.h"
#include "obj/ipc.h"
#include "obj/vmo.h"
#include "syscall.h"
#include "task.h"

int nx_inject_uaccess_unchecked;
int nx_inject_ipc_overgrant;
int nx_inject_kill_noop;
int nx_trace_ipc;

/* Rights a task handle opened through the Sovereign object carries. */
#define SOV_TASK_RIGHTS                                                                            \
    (NX_RIGHT_INSPECT | NX_RIGHT_MANAGE | NX_RIGHT_DUPLICATE | NX_RIGHT_TRANSFER)
#define TASK_READ_MAX 4096u

/* Bounce buffer for task_read: the kernel is single-CPU and not preempted
 * inside a system call, so one static buffer suffices. */
static uint8_t bounce[TASK_READ_MAX];

static int copy_in(struct nx_task *t, void *dst, uint64_t va, uint64_t len)
{
    if (nx_inject_uaccess_unchecked) {
        /* Deliberately broken variant that trusts the user pointer, meant
         * for a negative control of syscall validation (M2 criterion 4);
         * no test mode enables it yet. */
        memcpy(dst, (const void *)(uintptr_t)va, len);
        return NX_OK;
    }
    return nx_uaccess_copy_in(nx_vmm_env(), t->cr3, dst, va, len);
}

static int copy_out(struct nx_task *t, uint64_t va, const void *src, uint64_t len)
{
    return nx_uaccess_copy_out(nx_vmm_env(), t->cr3, va, src, len);
}

static int check_out(struct nx_task *t, uint64_t va, uint64_t len)
{
    return nx_uaccess_check(nx_vmm_env(), t->cr3, va, len, 1);
}

/* Global sequence of debug_write calls; every task records the positions of
 * its first and last call (task.h), which lets a test check that several
 * tasks made progress interleaved. */
uint64_t nx_debug_write_seq;

/* Each call becomes one or more serial lines "NANOX: USER <name>#<id>: <text>":
 * the text is split at '\n' (a trailing fragment without '\n' is a line of
 * its own) and bytes outside printable ASCII are replaced by '?'.  A user
 * program therefore cannot produce a line that starts with a kernel marker
 * such as "NANOX: TEST PASS".  The whole call is printed with interrupts
 * disabled, so lines of different tasks never mix. */
static int64_t sys_debug_write(struct nx_task *t, uint64_t buf, uint64_t len)
{
    uint8_t tmp[NX_DEBUG_WRITE_MAX];
    if (len > NX_DEBUG_WRITE_MAX)
        return -NX_EINVAL;
    int st = copy_in(t, tmp, buf, len);
    if (st != NX_OK)
        return -st;
    uint64_t start = 0;
    for (uint64_t i = 0; i <= len; i++) {
        if (i < len && tmp[i] != '\n')
            continue;
        if (i == len && start == len)
            break; /* nothing after the last '\n' */
        nx_printf("NANOX: USER %s#%u: ", t->name, t->id);
        for (uint64_t j = start; j < i; j++)
            nx_serial_putc(tmp[j] >= 0x20 && tmp[j] < 0x7F ? (char)tmp[j] : '?');
        nx_serial_putc('\n');
        start = i + 1;
    }
    nx_debug_write_seq++;
    t->writes++;
    if (!t->first_write_seq)
        t->first_write_seq = nx_debug_write_seq;
    t->last_write_seq = nx_debug_write_seq;
    return (int64_t)len;
}

static int64_t sys_vmo_create(struct nx_task *t, uint64_t pages)
{
    if (pages == 0 || pages > NX_VMO_MAX_PAGES)
        return -NX_EINVAL;
    int err;
    struct nx_vmo *v = nx_vmo_create((uint32_t)pages, &err);
    if (!v)
        return -err;
    uint32_t h;
    int st = nx_ht_install(&t->handles, &v->base,
                           NX_RIGHT_READ | NX_RIGHT_WRITE | NX_RIGHT_MAP | NX_RIGHT_TRANSFER |
                               NX_RIGHT_DUPLICATE,
                           &h);
    nx_obj_unref(&v->base); /* the handle holds the only reference (or none: freed) */
    return st == NX_OK ? (int64_t)h : -st;
}

static int64_t sys_vmo_map(struct nx_task *t, uint64_t h, uint64_t va, uint64_t prot)
{
    if (prot == 0 || (prot & ~(uint64_t)(NX_PROT_READ | NX_PROT_WRITE)))
        return -NX_EINVAL;
    uint32_t need = NX_RIGHT_MAP | NX_RIGHT_READ | (prot & NX_PROT_WRITE ? NX_RIGHT_WRITE : 0);
    struct nx_object *o;
    int st = h > UINT32_MAX ? NX_EBADHANDLE
                            : nx_ht_lookup(&t->handles, (uint32_t)h, NX_OBJ_VMO, need, &o, 0);
    if (st != NX_OK)
        return -st;
    struct nx_vmo *v = (struct nx_vmo *)o;
    uint64_t len = (uint64_t)v->pages * NX_PAGE_4K;
    if (va & (NX_PAGE_4K - 1))
        return -NX_EINVAL;
    if (!nx_user_range_ok(va, len))
        return -NX_ERANGE;
    uint32_t slot = NX_TASK_MAPS;
    for (uint32_t i = 0; i < NX_TASK_MAPS; i++)
        if (!t->maps[i].vmo) {
            slot = i;
            break;
        }
    if (slot == NX_TASK_MAPS)
        return -NX_ENOMEM;
    for (uint32_t i = 0; i < v->pages; i++) {
        uint64_t pa, fl, sz;
        if (nx_vmm_query_in(t->cr3, va + i * NX_PAGE_4K, &pa, &fl, &sz) == NX_PT_OK)
            return -NX_EEXISTS;
    }
    uint64_t flags = NX_PTE_U | NX_PTE_NX | (prot & NX_PROT_WRITE ? NX_PTE_W : 0);
    for (uint32_t i = 0; i < v->pages; i++) {
        if (nx_vmm_map_page_in(t->cr3, va + i * NX_PAGE_4K, v->phys[i], flags) != NX_PT_OK) {
            /* Out of page-table pages: undo (tables stay until the task ends). */
            for (uint32_t j = 0; j < i; j++) {
                uint64_t pa, size;
                nx_pt_unmap(nx_vmm_env(), t->cr3, va + j * NX_PAGE_4K, &pa, &size);
                nx_invlpg(va + j * NX_PAGE_4K); /* t is the current task */
            }
            return -NX_ENOMEM;
        }
    }
    nx_obj_ref(&v->base);
    t->maps[slot].vmo = v;
    t->maps[slot].va = va;
    t->maps[slot].pages = v->pages;
    return 0;
}

static int64_t sys_ipc_send(struct nx_task *t, uint64_t eh, uint64_t buf, uint64_t len,
                            uint64_t h, uint64_t rights)
{
    struct nx_object *o;
    int st = eh > UINT32_MAX ? NX_EBADHANDLE
                             : nx_ht_lookup(&t->handles, (uint32_t)eh, NX_OBJ_ENDPOINT,
                                            NX_RIGHT_SEND, &o, 0);
    if (st != NX_OK)
        return -st;
    struct nx_endpoint *ep = (struct nx_endpoint *)o;
    if (len > NX_IPC_MSG_MAX)
        return -NX_EINVAL;
    if (ep->count == NX_IPC_QUEUE)
        return -NX_EFULL;
    static struct nx_ipc_msg m; /* not on the 16 KiB kernel stack */
    memset(&m, 0, sizeof(m));
    m.len = (uint32_t)len;
    m.sender_task = t->id;
    st = copy_in(t, m.data, buf, len);
    if (st != NX_OK)
        return -st;
    if (h) {
        struct nx_object *xo;
        uint32_t have;
        if (h > UINT32_MAX || rights > UINT32_MAX)
            return h > UINT32_MAX ? -NX_EBADHANDLE : -NX_EACCESS;
        st = nx_ht_lookup(&t->handles, (uint32_t)h, NX_OBJ_ANY, NX_RIGHT_TRANSFER, &xo, &have);
        if (st != NX_OK)
            return -st;
        if ((uint32_t)rights & ~have)
            return -NX_EACCESS; /* rights can be dropped, never added */
        if (xo->type != NX_OBJ_VMO && xo->type != NX_OBJ_TASK)
            return -NX_EINVAL; /* endpoints/sovereign are not transferable in M2 */
        st = nx_ht_take(&t->handles, (uint32_t)h, (uint32_t)rights, &xo);
        if (st != NX_OK)
            return -st;
        m.obj = xo;
        /* Deliberately broken variant (nanox.test=m2-ipc-overgrant): the
         * receiver gets every right of the sender's handle. */
        m.rights = nx_inject_ipc_overgrant ? have : (uint32_t)rights;
        if (nx_trace_ipc)
            nx_printf("NANOX: ipc send by %s#%u len=%u handle=%s sender_rights=0x%x"
                      " requested=0x%x queued=0x%x\n",
                      t->name, t->id, m.len, xo->type == NX_OBJ_VMO ? "vmo" : "task", have,
                      (uint32_t)rights, m.rights);
    } else if (nx_trace_ipc) {
        nx_printf("NANOX: ipc send by %s#%u len=%u handle=none\n", t->name, t->id, m.len);
    }
    nx_ep_push(ep, &m); /* cannot fail: size and space checked above */
    if (ep->waiter) {
        struct nx_task *w = ep->waiter;
        ep->waiter = 0;
        nx_task_wake(w);
    }
    return 0;
}

static int64_t sys_ipc_recv(struct nx_task *t, uint64_t eh, uint64_t buf, uint64_t cap,
                            uint64_t info_va)
{
    struct nx_object *o;
    for (;;) {
        /* Re-resolved after every wake-up. */
        int st = eh > UINT32_MAX ? NX_EBADHANDLE
                                 : nx_ht_lookup(&t->handles, (uint32_t)eh, NX_OBJ_ENDPOINT,
                                                NX_RIGHT_RECV, &o, 0);
        if (st != NX_OK)
            return -st;
        struct nx_endpoint *ep = (struct nx_endpoint *)o;
        struct nx_ipc_msg *m = nx_ep_peek(ep);
        if (!m) {
            if (ep->waiter && ep->waiter != t)
                return -NX_EFULL; /* one receiver at a time */
            ep->waiter = t;
            nx_task_block(ep);
            continue;
        }
        /* The message stays queued if any check fails. */
        if (m->len > cap)
            return -NX_EINVAL;
        st = check_out(t, buf, m->len);
        if (st == NX_OK)
            st = check_out(t, info_va, sizeof(struct nx_ipc_info));
        if (st != NX_OK)
            return -st;
        struct nx_ipc_info info = {m->len, 0, 0, m->sender_task};
        if (m->obj) {
            st = nx_ht_install(&t->handles, m->obj, m->rights, &info.handle);
            if (st != NX_OK)
                return -st;
            info.rights = m->rights;
            nx_obj_unref(m->obj); /* the message's reference now belongs to the handle */
        }
        copy_out(t, buf, m->data, m->len);
        copy_out(t, info_va, &info, sizeof(info));
        if (nx_trace_ipc)
            nx_printf("NANOX: ipc recv by %s#%u len=%u from=#%u handle=0x%x rights=0x%x\n",
                      t->name, t->id, info.len, info.sender_task, info.handle, info.rights);
        nx_ep_pop(ep);
        return info.len;
    }
}

static int open_task(struct nx_task *t, uint64_t h, uint32_t need, struct nx_task **out)
{
    struct nx_object *o;
    int st = h > UINT32_MAX
                 ? NX_EBADHANDLE
                 : nx_ht_lookup(&t->handles, (uint32_t)h, NX_OBJ_TASK, need, &o, 0);
    if (st != NX_OK)
        return st;
    struct nx_task *target = (struct nx_task *)o;
    if (target->state == NX_TASK_DEAD || target->state == NX_TASK_REAPED)
        return NX_EDEAD;
    *out = target;
    return NX_OK;
}

static int64_t sys_sov_task_open(struct nx_task *t, uint64_t sh, uint64_t id)
{
    int st = sh > UINT32_MAX ? NX_EBADHANDLE
                             : nx_ht_lookup(&t->handles, (uint32_t)sh, NX_OBJ_SOVEREIGN,
                                            NX_RIGHT_SOVEREIGN, 0, 0);
    if (st != NX_OK)
        return -st;
    struct nx_task *target = id <= UINT32_MAX ? nx_task_by_id((uint32_t)id) : 0;
    if (!target || !target->user)
        return -NX_EINVAL; /* no such user task (kernel threads are not tasks here) */
    if (target->state == NX_TASK_DEAD || target->state == NX_TASK_REAPED)
        return -NX_EDEAD;
    uint32_t h;
    st = nx_ht_install(&t->handles, &target->base, SOV_TASK_RIGHTS, &h);
    return st == NX_OK ? (int64_t)h : -st;
}

static int64_t sys_task_read(struct nx_task *t, uint64_t th, uint64_t va, uint64_t buf,
                             uint64_t len)
{
    struct nx_task *target;
    int st = open_task(t, th, NX_RIGHT_INSPECT, &target);
    if (st != NX_OK)
        return -st;
    if (len > TASK_READ_MAX)
        return -NX_EINVAL;
    st = check_out(t, buf, len);
    if (st != NX_OK)
        return -st;
    st = nx_uaccess_copy_in(nx_vmm_env(), target->cr3, bounce, va, len);
    if (st != NX_OK)
        return -st;
    copy_out(t, buf, bounce, len);
    return (int64_t)len;
}

static int64_t sys_task_kill(struct nx_task *t, uint64_t th)
{
    struct nx_task *target;
    int st = open_task(t, th, NX_RIGHT_MANAGE, &target);
    if (st != NX_OK)
        return -st;
    if (nx_inject_kill_noop)
        return 0; /* deliberately broken (nanox.test=m3-kill-noop): success, no effect */
    nx_task_terminate(target, NX_END_KILLED, t->id); /* does not return if target == t */
    return 0;
}

/* ---- M3: control interface of the Cognitive Core executor --------------- */

static int sovereign_check(struct nx_task *t, uint64_t sh)
{
    return sh > UINT32_MAX ? NX_EBADHANDLE
                           : nx_ht_lookup(&t->handles, (uint32_t)sh, NX_OBJ_SOVEREIGN,
                                          NX_RIGHT_SOVEREIGN, 0, 0);
}

static int64_t sys_sleep(uint64_t ticks)
{
    if (ticks > NX_SLEEP_MAX)
        return -NX_EINVAL;
    if (ticks == 0)
        nx_yield();
    else
        nx_task_sleep(ticks);
    return 0;
}

static int64_t sys_sys_info(struct nx_task *t, uint64_t va)
{
    struct nx_sys_info info;
    memset(&info, 0, sizeof(info));
    info.boot_id = nx_boot_id;
    info.ticks = nx_timer_ticks;
    info.hz = NX_TIMER_HZ;
    info.tasks = nx_task_live_count();
    info.free_pages = nx_pmm.free_pages;
    info.managed_pages = nx_pmm.managed_pages;
    info.page_tables = nx_vmm_tables();
    info.events_next = nx_events.next;
    info.next_task_id = nx_task_next_id();
    int st = copy_out(t, va, &info, sizeof(info));
    return st == NX_OK ? 0 : -st;
}

/* Creates a user task from an initramfs program under bin/ and starts it;
 * the caller gets a task handle with the rights of SOV_TASK_OPEN. */
static int64_t sys_sov_task_spawn(struct nx_task *t, uint64_t sh, uint64_t path_va,
                                  uint64_t path_len, uint64_t args_va)
{
    int st = sovereign_check(t, sh);
    if (st != NX_OK)
        return -st;
    if (path_len < 5 || path_len > NX_SPAWN_PATH_MAX)
        return -NX_EINVAL;
    char path[NX_SPAWN_PATH_MAX + 1];
    uint64_t args[4] = {0, 0, 0, 0};
    st = copy_in(t, path, path_va, path_len);
    if (st == NX_OK && args_va)
        st = copy_in(t, args, args_va, sizeof(args));
    if (st != NX_OK)
        return -st;
    path[path_len] = 0;
    if (memcmp(path, "bin/", 4) != 0)
        return -NX_EINVAL;
    for (uint64_t i = 4; i < path_len; i++)
        if (!((path[i] >= 'a' && path[i] <= 'z') || (path[i] >= '0' && path[i] <= '9') ||
              path[i] == '-' || path[i] == '_'))
            return -NX_EINVAL;
    const uint8_t *base;
    uint64_t size;
    nx_initramfs_get(&base, &size);
    struct nx_cpio_entry e;
    if (!base || nx_cpio_find(base, size, path, &e) != NX_CPIO_OK ||
        (e.mode & NX_CPIO_MODE_TYPE) != NX_CPIO_MODE_REG)
        return -NX_ENOENT;
    if (nx_ht_count(&t->handles) >= NX_HANDLE_SLOTS)
        return -NX_ENOMEM; /* checked first: nothing to undo later */
    int err = 0;
    struct nx_task *n = nx_utask_create(path + 4, e.data, e.size, args, &err);
    if (!n)
        return -err;
    uint32_t h;
    if (nx_ht_install(&t->handles, &n->base, SOV_TASK_RIGHTS, &h) != NX_OK)
        nx_panic("spawn: handle slot vanished");
    nx_task_start(n);
    return (int64_t)h;
}

static int64_t sys_task_info(struct nx_task *t, uint64_t th, uint64_t va)
{
    struct nx_object *o;
    int st = th > UINT32_MAX ? NX_EBADHANDLE
                             : nx_ht_lookup(&t->handles, (uint32_t)th, NX_OBJ_TASK,
                                            NX_RIGHT_INSPECT, &o, 0);
    if (st != NX_OK)
        return -st;
    struct nx_task_info info;
    nx_task_fill_info((struct nx_task *)o, &info);
    st = copy_out(t, va, &info, sizeof(info));
    return st == NX_OK ? 0 : -st;
}

static int64_t sys_sov_task_list(struct nx_task *t, uint64_t sh, uint64_t va, uint64_t cap)
{
    static struct nx_task_info list[NX_TASK_MAX];
    int st = sovereign_check(t, sh);
    if (st != NX_OK)
        return -st;
    if (cap > NX_TASK_MAX)
        return -NX_EINVAL;
    uint32_t n = nx_task_list(list, NX_TASK_MAX);
    uint32_t copy = n < cap ? n : (uint32_t)cap;
    st = copy ? copy_out(t, va, list, copy * sizeof(list[0])) : NX_OK;
    return st == NX_OK ? (int64_t)n : -st;
}

#define EVENT_READ_MAX 32u

static int64_t sys_sov_event_read(struct nx_task *t, uint64_t sh, uint64_t since, uint64_t va,
                                  uint64_t cap)
{
    static struct nx_event ev[EVENT_READ_MAX];
    int st = sovereign_check(t, sh);
    if (st != NX_OK)
        return -st;
    if (cap == 0 || cap > EVENT_READ_MAX)
        return -NX_EINVAL;
    st = check_out(t, va, cap * sizeof(ev[0]));
    if (st != NX_OK)
        return -st;
    uint32_t n = nx_evlog_read(&nx_events, since, ev, (uint32_t)cap);
    if (n)
        copy_out(t, va, ev, n * sizeof(ev[0]));
    return n;
}

static struct nx_chan *chan_lookup(struct nx_task *t, uint64_t ch, uint32_t need, int *st)
{
    struct nx_object *o = 0;
    *st = ch > UINT32_MAX ? NX_EBADHANDLE
                          : nx_ht_lookup(&t->handles, (uint32_t)ch, NX_OBJ_CHANNEL, need, &o, 0);
    return (struct nx_chan *)o;
}

/* Polled receive: spin briefly (the host may be refilling the UART FIFO),
 * then sleep one tick between polls until `timeout` ticks have passed. */
#define CHAN_SPIN_POLLS 20000u

static int64_t sys_chan_read(struct nx_task *t, uint64_t ch, uint64_t va, uint64_t cap,
                             uint64_t timeout)
{
    static uint8_t tmp[NX_CHAN_IO_MAX];
    int st;
    if (!chan_lookup(t, ch, NX_RIGHT_READ, &st))
        return -st;
    if (cap == 0 || cap > NX_CHAN_IO_MAX || timeout > NX_CHAN_TIMEOUT_MAX)
        return -NX_EINVAL;
    st = check_out(t, va, cap);
    if (st != NX_OK)
        return -st;
    for (uint64_t waited = 0;; waited++) {
        struct nx_chan *c = chan_lookup(t, ch, NX_RIGHT_READ, &st); /* again after sleeping */
        if (!c)
            return -st;
        uint32_t n = 0;
        for (uint32_t i = 0; i < CHAN_SPIN_POLLS && n == 0; i++)
            n = nx_chan_poll(c, tmp, (uint32_t)cap);
        if (n) {
            copy_out(t, va, tmp, n);
            return n;
        }
        if (waited >= timeout)
            return 0;
        nx_task_sleep(1);
    }
}

static int64_t sys_chan_write(struct nx_task *t, uint64_t ch, uint64_t va, uint64_t len)
{
    static uint8_t tmp[NX_CHAN_IO_MAX];
    int st;
    struct nx_chan *c = chan_lookup(t, ch, NX_RIGHT_WRITE, &st);
    if (!c)
        return -st;
    if (len > NX_CHAN_IO_MAX)
        return -NX_EINVAL;
    st = copy_in(t, tmp, va, len);
    if (st != NX_OK)
        return -st;
    nx_chan_write(c, tmp, (uint32_t)len);
    return (int64_t)len;
}

void nx_syscall(struct nx_trap_frame *f)
{
    struct nx_task *t = nx_current;
    uint64_t a = f->rdi, b = f->rsi, c = f->rdx, d = f->r10, e = f->r8;
    int64_t r;
    t->syscalls++;
    switch (f->rax) {
    case NX_SYS_DEBUG_WRITE: r = sys_debug_write(t, a, b); break;
    case NX_SYS_EXIT: nx_task_exit_current((int64_t)a);
    case NX_SYS_YIELD:
        t->yields++;
        nx_yield();
        r = 0;
        break;
    case NX_SYS_TASK_SELF: r = t->id; break;
    case NX_SYS_HANDLE_CLOSE:
        r = a > UINT32_MAX ? -NX_EBADHANDLE : -nx_ht_close(&t->handles, (uint32_t)a);
        break;
    case NX_SYS_HANDLE_DUP: {
        uint32_t h;
        int st = a > UINT32_MAX ? NX_EBADHANDLE
                 : b > UINT32_MAX ? NX_EACCESS
                                  : nx_ht_dup(&t->handles, (uint32_t)a, (uint32_t)b, &h);
        r = st == NX_OK ? (int64_t)h : -st;
        break;
    }
    case NX_SYS_VMO_CREATE: r = sys_vmo_create(t, a); break;
    case NX_SYS_VMO_MAP: r = sys_vmo_map(t, a, b, c); break;
    case NX_SYS_IPC_SEND: r = sys_ipc_send(t, a, b, c, d, e); break;
    case NX_SYS_IPC_RECV: r = sys_ipc_recv(t, a, b, c, d); break;
    case NX_SYS_SOV_TASK_OPEN: r = sys_sov_task_open(t, a, b); break;
    case NX_SYS_TASK_READ: r = sys_task_read(t, a, b, c, d); break;
    case NX_SYS_TASK_KILL: r = sys_task_kill(t, a); break;
    case NX_SYS_SLEEP: r = sys_sleep(a); break;
    case NX_SYS_SYS_INFO: r = sys_sys_info(t, a); break;
    case NX_SYS_SOV_TASK_SPAWN: r = sys_sov_task_spawn(t, a, b, c, d); break;
    case NX_SYS_TASK_INFO: r = sys_task_info(t, a, b); break;
    case NX_SYS_SOV_TASK_LIST: r = sys_sov_task_list(t, a, b, c); break;
    case NX_SYS_SOV_EVENT_READ: r = sys_sov_event_read(t, a, b, c, d); break;
    case NX_SYS_CHAN_READ: r = sys_chan_read(t, a, b, c, d); break;
    case NX_SYS_CHAN_WRITE: r = sys_chan_write(t, a, b, c); break;
    default: r = -NX_ENOSYS; break;
    }
    f->rax = (uint64_t)r;
}
