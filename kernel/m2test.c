/*
 * M2 test modes (see m2test.h and docs/m2-kernel.md).
 *
 *   m2-user              bin/hello in ring 3, own address space, reaped;
 *                        allocator and page-table counters return to the
 *                        values before the task existed
 *   m2-sched             three bin/spin tasks that never yield, interleaved
 *                        by the timer; all finish with the expected result
 *   m2-sched-nopreempt   negative control: the same with preemption off must
 *                        fail the interleaving check
 *   m2-ipc               bin/ipc-send sends a message and a memory-object
 *                        handle with reduced rights to bin/ipc-recv
 *   m2-ipc-overgrant     negative control: the kernel deliberately delivers
 *                        the handle with all of the sender's rights; the
 *                        receiver must notice and the run must fail
 *
 * The kernel thread "kmain" (the boot context) is the test controller: it
 * creates the tasks, blocks until each has ended, checks the result and the
 * resource counters, and prints the verdict.
 */
#include <stdarg.h>
#include <stdint.h>

#include <nanox/diag.h>
#include <nanox/m2test.h>
#include <nanox/printf.h>
#include <nanox/syscall.h>

#include "arch/x86_64/timer.h"
#include "initramfs.h"
#include "kernel.h"
#include "m2test.h"
#include "mm/mm.h"
#include "obj/ipc.h"
#include "obj/vmo.h"
#include "syscall.h"
#include "task.h"

/* Watchdogs (timer ticks at NX_TIMER_HZ): a hang is reported as TEST FAIL
 * naming the scenario instead of running into the harness timeout. */
#define WATCHDOG_SHORT (20u * NX_TIMER_HZ)
#define WATCHDOG_SCHED (60u * NX_TIMER_HZ)

/* bin/spin workload: iterations per task.  Long enough for many time
 * slices per quarter (the check needs more than one), short enough for a
 * few seconds per scenario under TCG. */
#define SPIN_TASKS 3u
#define SPIN_ITERATIONS ((uint64_t)200000000u)

static const uint8_t *initrd_base;
static uint64_t initrd_size;

void nx_m2_set_initramfs(const uint8_t *base, uint64_t size)
{
    initrd_base = base;
    initrd_size = size;
}

__attribute__((noreturn, format(printf, 1, 2))) static void fail(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    nx_printf("NANOX: TEST FAIL ");
    nx_vprintf(fmt, ap);
    nx_printf("\n");
    va_end(ap);
    nx_debug_exit(NX_EXIT_TEST_FAIL);
}

__attribute__((noreturn)) static void pass(void)
{
    nx_printf("NANOX: TEST PASS\n");
    nx_debug_exit(NX_EXIT_TEST_PASS);
}

/* Kernel resources that every scenario must give back. */
struct snapshot {
    uint64_t free_pages; /* physical allocator */
    uint64_t tables;     /* page-table pages */
    uint32_t tasks;      /* live task slots */
    uint32_t vmos, endpoints;
};

static struct snapshot snap(void)
{
    struct snapshot s = {nx_pmm.free_pages, nx_vmm_tables(), nx_task_live_count(), nx_vmo_live(),
                         nx_ep_live()};
    return s;
}

static void print_snap(const char *mode, const char *when, const struct snapshot *s)
{
    nx_printf("NANOX: %s resources %s free_pages=%" NX_PRIu64 " tables=%" NX_PRIu64
              " tasks=%u vmos=%u endpoints=%u\n",
              mode, when, s->free_pages, s->tables, s->tasks, s->vmos, s->endpoints);
}

static void expect_same(const char *mode, const struct snapshot *a, const struct snapshot *b)
{
    if (a->free_pages != b->free_pages || a->tables != b->tables || a->tasks != b->tasks ||
        a->vmos != b->vmos || a->endpoints != b->endpoints)
        fail("%s: resources not returned: free_pages %" NX_PRIu64 "->%" NX_PRIu64
             " tables %" NX_PRIu64 "->%" NX_PRIu64 " tasks %u->%u vmos %u->%u endpoints %u->%u",
             mode, a->free_pages, b->free_pages, a->tables, b->tables, a->tasks, b->tasks,
             a->vmos, b->vmos, a->endpoints, b->endpoints);
}

/* Creates a user task from an initramfs program.  The controller keeps a
 * reference to the task object, so the slot and its statistics stay
 * readable after the task is reaped; release() drops it. */
static struct nx_task *spawn(const char *mode, const char *name, const char *path,
                             const uint64_t args[4])
{
    struct nx_cpio_entry e;
    int st = nx_cpio_find(initrd_base, initrd_size, path, &e);
    if (st != NX_CPIO_OK || (e.mode & NX_CPIO_MODE_TYPE) != NX_CPIO_MODE_REG)
        fail("%s: %s not found in the initramfs (%s)", mode, path, nx_cpio_strerror(st));
    int err = 0;
    struct nx_task *t = nx_utask_create(name, e.data, e.size, args, &err);
    if (!t)
        fail("%s: cannot create task %s from %s (error %d)", mode, name, path, err);
    nx_obj_ref(&t->base);
    nx_printf("NANOX: %s spawn %s#%u path=%s size=%" NX_PRIu64 " entry=0x%016" NX_PRIx64 "\n",
              mode, t->name, t->id, path, e.size, t->entry);
    return t;
}

static void release(struct nx_task *t)
{
    nx_obj_unref(&t->base);
}

/* Waits for t, reaps it and requires a normal exit with code 0. */
static void wait_ok(const char *mode, struct nx_task *t)
{
    int64_t code = nx_task_wait_reap(t);
    if (t->end != NX_END_EXIT || code != 0)
        fail("%s: task %s#%u did not exit normally with code 0 (end=%d code=%" NX_PRIu64 ")",
             mode, t->name, t->id, t->end, (uint64_t)code);
}

static const char *perms(uint64_t flags)
{
    static const char *const names[] = {"r-x", "rwx", "r--", "rw-"};
    return names[((flags & NX_PTE_NX) ? 2 : 0) | ((flags & NX_PTE_W) ? 1 : 0)];
}

/* ---- m2-user ------------------------------------------------------------ */

void nx_m2_user(void)
{
    const char *mode = "m2-user";
    nx_sched_init(1);
    struct snapshot before = snap();
    print_snap(mode, "before", &before);

    const uint64_t args[4] = {NX_M2_HELLO_ARG0, NX_M2_HELLO_ARG1, NX_M2_HELLO_ARG2,
                              NX_M2_HELLO_ARG3};
    struct nx_task *t = spawn(mode, "hello", "bin/hello", args);

    /* Its own address space: a root of its own; the program and its stack
     * are mapped there for user mode, and not at all in the kernel's. */
    uint64_t pa, code_flags, stack_flags, flags, size;
    uint64_t stack_va = NX_USER_STACK_TOP - 8;
    if (t->cr3 == nx_kernel_root)
        fail("%s: task shares the kernel page tables", mode);
    if (nx_vmm_query_in(t->cr3, t->entry, &pa, &code_flags, &size) != NX_PT_OK ||
        !(code_flags & NX_PTE_U) || (code_flags & (NX_PTE_W | NX_PTE_NX)))
        fail("%s: entry 0x%" NX_PRIx64 " not mapped user r-x in the task", mode, t->entry);
    if (nx_vmm_query_in(t->cr3, stack_va, &pa, &stack_flags, &size) != NX_PT_OK ||
        !(stack_flags & NX_PTE_U) || !(stack_flags & NX_PTE_W) || !(stack_flags & NX_PTE_NX))
        fail("%s: stack not mapped user rw- in the task", mode);
    if (nx_vmm_query_in(nx_kernel_root, t->entry, &pa, &flags, &size) != NX_PT_E_NOT_MAPPED ||
        nx_vmm_query_in(nx_kernel_root, stack_va, &pa, &flags, &size) != NX_PT_E_NOT_MAPPED)
        fail("%s: user pages are visible in the kernel address space", mode);
    struct snapshot loaded = snap();
    nx_printf("NANOX: %s address space hello#%u root=0x%" NX_PRIx64 " kernel_root=0x%" NX_PRIx64
              " code=user,%s stack=user,%s kernel_root_maps_them=no pages_in_use=%" NX_PRIu64 "\n",
              mode, t->id, t->cr3, nx_kernel_root, perms(code_flags), perms(stack_flags),
              before.free_pages - loaded.free_pages);

    nx_sched_watchdog(WATCHDOG_SHORT, mode);
    nx_task_start(t);
    wait_ok(mode, t);
    nx_sched_watchdog(0, 0);
    if (t->writes == 0)
        fail("%s: hello#%u printed nothing", mode, t->id);
    uint64_t syscalls = t->syscalls;
    release(t);

    struct snapshot after = snap();
    print_snap(mode, "after", &after);
    expect_same(mode, &before, &after);
    nx_printf("NANOX: %s ok task=hello#%u exit=0 syscalls=%" NX_PRIu64 " pages_in_use=%" NX_PRIu64
              " free_pages_restored=%" NX_PRIu64 "\n",
              mode, t->id, syscalls, before.free_pages - loaded.free_pages, after.free_pages);
    pass();
}

/* ---- m2-sched ----------------------------------------------------------- */

__attribute__((noreturn)) static void sched_test(const char *mode, int preempt)
{
    static const char *const names[SPIN_TASKS] = {"spin-a", "spin-b", "spin-c"};
    struct nx_task *t[SPIN_TASKS];
    nx_sched_init(preempt);
    struct snapshot before = snap();
    print_snap(mode, "before", &before);

    for (uint32_t i = 0; i < SPIN_TASKS; i++) {
        /* Independent oracle: the kernel runs the same loop itself. */
        uint64_t seed = 0x9E3779B97F4A7C15ull * (i + 1), x = seed;
        for (uint64_t n = 0; n < SPIN_ITERATIONS; n++)
            x = nx_m2_spin_step(x);
        const uint64_t args[4] = {i, SPIN_ITERATIONS, seed, x};
        t[i] = spawn(mode, names[i], "bin/spin", args);
        nx_printf("NANOX: %s expect %s#%u iterations=%" NX_PRIu64 " result=0x%016" NX_PRIx64 "\n",
                  mode, t[i]->name, t[i]->id, SPIN_ITERATIONS, x);
    }
    uint64_t switches0 = nx_sched_switches, preemptions0 = nx_sched_preemptions;
    nx_sched_watchdog(WATCHDOG_SCHED, mode);
    for (uint32_t i = 0; i < SPIN_TASKS; i++)
        nx_task_start(t[i]);
    for (uint32_t i = 0; i < SPIN_TASKS; i++)
        wait_ok(mode, t[i]);
    nx_sched_watchdog(0, 0);

    /* The programs neither yield nor block; if every task printed its
     * first progress line before any task printed its last one, the tasks
     * ran interleaved, and only the timer can have switched between them. */
    uint64_t max_first = 0, min_last = UINT64_MAX, min_preempted = UINT64_MAX;
    for (uint32_t i = 0; i < SPIN_TASKS; i++) {
        if (t[i]->yields != 0)
            fail("%s: %s#%u called yield", mode, t[i]->name, t[i]->id);
        if (t[i]->writes != NX_M2_SPIN_REPORTS + 1)
            fail("%s: %s#%u wrote %" NX_PRIu64 " lines, expected %u", mode, t[i]->name, t[i]->id,
                 t[i]->writes, NX_M2_SPIN_REPORTS + 1);
        if (t[i]->first_write_seq > max_first)
            max_first = t[i]->first_write_seq;
        if (t[i]->last_write_seq < min_last)
            min_last = t[i]->last_write_seq;
        if (t[i]->preempted < min_preempted)
            min_preempted = t[i]->preempted;
    }
    nx_printf("NANOX: %s progress order latest_first=%" NX_PRIu64 " earliest_last=%" NX_PRIu64
              " preempted=%" NX_PRIu64 ",%" NX_PRIu64 ",%" NX_PRIu64 "\n",
              mode, max_first, min_last, t[0]->preempted, t[1]->preempted, t[2]->preempted);
    if (max_first >= min_last)
        fail("%s: tasks did not interleave: a task finished before another reported progress"
             " (latest_first=%" NX_PRIu64 " earliest_last=%" NX_PRIu64 ")",
             mode, max_first, min_last);
    if (min_preempted == 0)
        fail("%s: a task was never preempted", mode);
    uint64_t switches = nx_sched_switches - switches0,
             preemptions = nx_sched_preemptions - preemptions0;
    for (uint32_t i = 0; i < SPIN_TASKS; i++)
        release(t[i]);

    struct snapshot after = snap();
    print_snap(mode, "after", &after);
    expect_same(mode, &before, &after);
    nx_printf("NANOX: %s ok tasks=%u exited=%u yields=0 interleaved=yes switches=%" NX_PRIu64
              " preemptions=%" NX_PRIu64 "\n",
              mode, SPIN_TASKS, SPIN_TASKS, switches, preemptions);
    pass();
}

void nx_m2_sched(void)
{
    sched_test("m2-sched", 1);
}

void nx_m2_sched_nopreempt(void)
{
    sched_test("m2-sched-nopreempt", 0);
}

/* ---- m2-ipc ------------------------------------------------------------- */

__attribute__((noreturn)) static void ipc_test(const char *mode, int overgrant)
{
    nx_sched_init(1);
    nx_trace_ipc = 1;
    if (overgrant) {
        nx_inject_ipc_overgrant = 1;
        nx_printf("NANOX: %s fault injection: transferred handles keep all sender rights\n",
                  mode);
    }
    struct snapshot before = snap();
    print_snap(mode, "before", &before);

    struct nx_endpoint *ep = nx_ep_create();
    if (!ep)
        fail("%s: no endpoint", mode);
    const uint64_t none[4] = {0, 0, 0, 0};
    struct nx_task *recv = spawn(mode, "ipc-recv", "bin/ipc-recv", none);
    struct nx_task *send = spawn(mode, "ipc-send", "bin/ipc-send", none);
    uint32_t hs, hr;
    if (nx_ht_install(&send->handles, &ep->base, NX_RIGHT_SEND, &hs) != NX_OK ||
        nx_ht_install(&recv->handles, &ep->base, NX_RIGHT_RECV, &hr) != NX_OK)
        fail("%s: cannot install the endpoint handles", mode);
    nx_obj_unref(&ep->base); /* from now on the two handles keep it alive */
    nx_task_set_arg(send, 0, hs);
    nx_task_set_arg(recv, 0, hr);
    nx_task_set_arg(recv, 1, send->id);
    nx_printf("NANOX: %s endpoint %s#%u handle=0x%x rights=0x%x, %s#%u handle=0x%x rights=0x%x\n",
              mode, send->name, send->id, hs, NX_RIGHT_SEND, recv->name, recv->id, hr,
              NX_RIGHT_RECV);

    /* The receiver runs first and must block in receive (the message does
     * not exist yet); only then is the sender started. */
    nx_sched_watchdog(WATCHDOG_SHORT, mode);
    nx_task_start(recv);
    for (unsigned i = 0; i < 1000 && recv->state != NX_TASK_BLOCKED; i++)
        nx_yield();
    if (recv->state != NX_TASK_BLOCKED || recv->blocked_on != ep)
        fail("%s: %s#%u did not block in receive (state %d)", mode, recv->name, recv->id,
             recv->state);
    nx_printf("NANOX: %s %s#%u blocked in receive on the endpoint, starting %s#%u\n", mode,
              recv->name, recv->id, send->name, send->id);
    nx_task_start(send);
    wait_ok(mode, send);
    wait_ok(mode, recv);
    nx_sched_watchdog(0, 0);
    release(send);
    release(recv);

    struct snapshot after = snap();
    print_snap(mode, "after", &after);
    expect_same(mode, &before, &after);
    nx_printf("NANOX: %s ok message and handle delivered, rights reduced to 0x%x\n", mode,
              NX_M2_IPC_GRANT);
    pass();
}

void nx_m2_ipc(void)
{
    ipc_test("m2-ipc", 0);
}

void nx_m2_ipc_overgrant(void)
{
    ipc_test("m2-ipc-overgrant", 1);
}
