/*
 * M3 test modes (see m3test.h and docs/m3-core.md).
 *
 *   m3-serve         bin/core serves the host bridge; what the host asks
 *                    for (agent loop, protocol checks, fault cases) is
 *                    chosen by the harness scenario, not by the kernel
 *   m3-kill-noop     negative control: NX_SYS_TASK_KILL reports success
 *                    without an effect; the executor's end-state check must
 *                    catch it and the run must fail
 *   m3-events-off    negative control: the kernel records no state events;
 *                    the executor's event checks must fail
 *   m3-nodedup       negative control: bin/core does not deduplicate
 *                    requests; the host's retry check must fail
 */
#include <stdarg.h>
#include <stdint.h>

#include <nanox/diag.h>
#include <nanox/m3.h>
#include <nanox/m4.h>
#include <nanox/printf.h>
#include <nanox/syscall.h>

#include "arch/x86_64/cpu.h"
#include "arch/x86_64/timer.h"
#include "chan.h"
#include "dev/blk.h"
#include "dev/virtio_net.h"
#include "kernel.h"
#include "m2test.h"
#include "m3test.h"
#include "obj/event.h"
#include "obj/vmo.h"
#include "syscall.h"
#include "task.h"

/* The whole bridge session must end within this time (the harness timeout
 * of the M3 scenarios is longer, so a hang is reported by the kernel). */
#define WATCHDOG_M3 (150u * NX_TIMER_HZ)

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

static void print_snap(const char *mode, const char *when, const struct nx_test_snapshot *s)
{
    nx_printf("NANOX: %s resources %s free_pages=%" NX_PRIu64 " tables=%" NX_PRIu64
              " tasks=%u vmos=%u endpoints=%u\n",
              mode, when, s->free_pages, s->tables, s->tasks, s->vmos, s->endpoints);
}

/* splitmix64 finaliser over the TSC: under TCG the TSC follows the host
 * clock, so two boots practically never share an identifier. */
static uint64_t make_boot_id(void)
{
    uint64_t z = nx_rdtsc() + 0x9E3779B97F4A7C15ull;
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
    z ^= z >> 31;
    return z ? z : 1;
}

static const char *core_verdict(int64_t code)
{
    switch (code) {
    case NX_M3_CORE_OK: return 0;
    case NX_M3_CORE_VERIFY_FAILED: return "the executor's end-state verification failed";
    case NX_M3_CORE_HOST_FAILED: return "the host bridge reported failed host-side checks";
    case NX_M3_CORE_BRIDGE_LOST: return "the host bridge went silent";
    case NX_M4_CORE_STORE_FAILED: return "the store could not be mounted or checked";
    default: return "bin/core failed";
    }
}

/* Ends every user task that is still alive (the executor should have
 * stopped everything it started) and waits until the reaper released them.
 * Returns how many there were. */
static uint32_t cleanup_leftovers(const char *mode)
{
    static struct nx_task_info list[NX_TASK_MAX];
    uint32_t n = nx_task_list(list, NX_TASK_MAX), left = 0;
    for (uint32_t i = 0; i < n; i++) {
        if (!(list[i].flags & NX_TASK_INFO_USER) || list[i].state == NX_TS_DEAD ||
            list[i].state == NX_TS_REAPED)
            continue;
        struct nx_task *t = nx_task_by_id(list[i].id);
        nx_printf("NANOX: %s leftover task %s#%u state=%u: terminating\n", mode, t->name, t->id,
                  list[i].state);
        nx_task_terminate(t, NX_END_KERNEL, 0);
        left++;
    }
    for (unsigned i = 0; i < 100; i++) {
        int dead = 0;
        n = nx_task_list(list, NX_TASK_MAX);
        for (uint32_t j = 0; j < n; j++)
            dead += list[j].state == NX_TS_DEAD;
        if (!dead)
            break;
        nx_task_sleep(1);
    }
    return left;
}

__attribute__((noreturn)) void nx_core_session(const struct nx_core_opts *o)
{
    const char *mode = o->mode;
    uint64_t core_flags = o->flags;
    nx_sched_init(1);
    nx_boot_id = make_boot_id();
    nx_reaper_start();
    struct nx_chan *chan = 0;
    if (o->bridge) {
        chan = nx_chan_init();
        if (!chan)
            fail("%s: no UART at COM2: the host bridge channel is missing", mode);
    }
    if (!o->blk)
        nx_printf("NANOX: m3 boot_id=%016" NX_PRIx64 " bridge=com2 port=0x2f8 events=%s"
                  " kill=%s dedup=%s\n",
                  nx_boot_id, nx_events.disabled ? "off" : "on",
                  nx_inject_kill_noop ? "noop" : "on",
                  core_flags & NX_M3_CORE_NODEDUP ? "off" : "on");
    else
        nx_printf("NANOX: m4 boot_id=%016" NX_PRIx64 " bridge=%s store=virtio-blk serial=%s"
                  " blocks=%" NX_PRIu64 " cache=%s flush=%s crash=%u lose=%s\n",
                  nx_boot_id, chan ? "com2" : "none", o->blk->dev->serial, o->blk->blocks,
                  nx_blk_test.volatile_cache ? "volatile-test" : "device",
                  nx_blk_test.flush_noop ? "noop" : "device", nx_blk_test.crash_at,
                  nx_blk_lose_name(nx_blk_test.lose));
    struct nx_test_snapshot before, after;
    nx_test_snapshot(&before);
    print_snap(mode, "before", &before);

    const uint64_t none[4] = {0, 0, 0, 0};
    struct nx_task *core = nx_test_spawn(mode, "core", "bin/core", none);
    uint32_t sh, ch = 0, bh = 0, nh = 0;
    if (nx_ht_install(&core->handles, nx_sovereign(), NX_RIGHT_SOVEREIGN, &sh) != NX_OK ||
        (o->net && nx_ht_install(&core->handles, &o->net->base, NX_RIGHT_READ | NX_RIGHT_WRITE,
                                 &nh) != NX_OK) ||
        (chan && nx_ht_install(&core->handles, &chan->base, NX_RIGHT_READ | NX_RIGHT_WRITE,
                               &ch) != NX_OK) ||
        (o->blk && nx_ht_install(&core->handles, &o->blk->base, NX_RIGHT_READ | NX_RIGHT_WRITE,
                                 &bh) != NX_OK))
        fail("%s: cannot install the core's handles", mode);
    nx_task_set_arg(core, 0, sh);
    nx_task_set_arg(core, 1, ch);
    nx_task_set_arg(core, 2, core_flags | (uint64_t)nh << 32);
    nx_task_set_arg(core, 3, bh);
    nx_printf("NANOX: %s core %s#%u sovereign=0x%x rights=0x%x channel=0x%x rights=0x%x"
              " flags=0x%" NX_PRIx64 "%s\n",
              mode, core->name, core->id, sh, NX_RIGHT_SOVEREIGN, ch,
              chan ? NX_RIGHT_READ | NX_RIGHT_WRITE : 0, core_flags,
              o->blk ? " blk=present" : "");
    if (o->blk)
        nx_printf("NANOX: %s core blk=0x%x rights=0x%x\n", mode, bh,
                  NX_RIGHT_READ | NX_RIGHT_WRITE);
    if (o->net)
        nx_printf("NANOX: %s core net=0x%x rights=0x%x\n", mode, nh,
                  NX_RIGHT_READ | NX_RIGHT_WRITE);

    nx_sched_watchdog(WATCHDOG_M3, mode);
    nx_task_start(core);
    int64_t code = nx_task_wait_reap(core);
    int core_end = core->end;
    uint32_t core_id = core->id;
    nx_obj_unref(&core->base);
    uint32_t leftovers = cleanup_leftovers(mode);
    nx_sched_watchdog(0, 0);
    if (chan)
        nx_printf("NANOX: %s bridge rx_bytes=%" NX_PRIu64 " tx_bytes=%" NX_PRIu64
                  " events=%" NX_PRIu64 "\n",
                  mode, chan->rx_bytes, chan->tx_bytes, nx_events.next - 1);
    if (o->net) {
        struct nx_net_info ni;
        nx_net_info(o->net, &ni);
        nx_printf("NANOX: %s net rx_frames=%" NX_PRIu64 " tx_frames=%" NX_PRIu64
                  " rx_test_drops=%" NX_PRIu64 " tx_test_drops=%" NX_PRIu64
                  " tx_link_down=%" NX_PRIu64 " link=%s\n",
                  mode, ni.rx_frames, ni.tx_frames, ni.rx_test_drops, ni.tx_test_drops,
                  ni.tx_link_down, ni.flags & NX_NET_INFO_LINK_UP ? "up" : "down");
    }
    if (o->blk)
        nx_printf("NANOX: %s blk reads=%" NX_PRIu64 " writes=%" NX_PRIu64 " flushes=%" NX_PRIu64
                  " ops=%u pending=%u\n",
                  mode, o->blk->reads, o->blk->writes, o->blk->flushes, nx_blk_test.ops,
                  nx_blk_pending());

    nx_test_snapshot(&after);
    print_snap(mode, "after", &after);
    if (core_end != NX_END_EXIT)
        fail("%s: core#%u did not exit normally (end=%d)", mode, core_id, core_end);
    const char *why = core_verdict(code);
    if (why)
        fail("%s: core#%u exited with code %" NX_PRIu64 ": %s", mode, core_id, (uint64_t)code,
             why);
    if (leftovers)
        fail("%s: %u task(s) started through NCI were still running after the session", mode,
             leftovers);
    if (before.free_pages != after.free_pages || before.tables != after.tables ||
        before.tasks != after.tasks || before.vmos != after.vmos ||
        before.endpoints != after.endpoints)
        fail("%s: resources not returned", mode);
    nx_printf("NANOX: %s ok core#%u exit=0 resources_restored=yes\n", mode, core_id);
    nx_test_pass();
}

__attribute__((noreturn)) static void m3_run(const char *mode, uint64_t core_flags)
{
    struct nx_core_opts o = {mode, core_flags, 1, 0, 0};
    nx_core_session(&o);
}

void nx_m3_serve(void)
{
    m3_run("m3-serve", 0);
}

void nx_m3_kill_noop(void)
{
    nx_inject_kill_noop = 1;
    m3_run("m3-kill-noop", 0);
}

void nx_m3_events_off(void)
{
    nx_events.disabled = 1;
    m3_run("m3-events-off", 0);
}

void nx_m3_nodedup(void)
{
    m3_run("m3-nodedup", NX_M3_CORE_NODEDUP);
}
