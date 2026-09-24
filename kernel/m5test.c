/*
 * M5 test modes (see m5test.h and docs/m5-net.md).
 *
 *   m5-serve     bin/core with console (COM2), data disk, virtio-net and
 *                virtio-rng; what happens is chosen by the harness
 *                scenario (console script, provider policy, link state)
 *   m5-noretx    negative control: bin/core's TCP never retransmits
 *   m5-noretry   negative control: the provider client does not retry
 *   m5-flattel   negative control: telemetry does not classify failures
 *
 * Parameter of the network test layer (any M5 mode):
 *   nanox.m5.loss=rx:<N>,tx:<M>   drop every N-th received and every M-th
 *                                 sent frame (either part may be omitted)
 */
#include <stdarg.h>
#include <stdint.h>

#include <nanox/diag.h>
#include <nanox/m3.h>
#include <nanox/m4.h>
#include <nanox/m5.h>
#include <nanox/printf.h>
#include <nanox/string.h>

#include "arch/x86_64/timer.h"
#include "dev/blk.h"
#include "dev/rtc.h"
#include "dev/virtio_net.h"
#include "kernel.h"
#include "m3test.h"
#include "m5test.h"

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

/* "rx:N,tx:M" (either part optional, N, M in 2..1000). */
static void parse_loss(const char *mode, struct nx_netdev *n)
{
    const char *v;
    uint32_t len;
    if (!nx_cmdline_value("nanox.m5.loss", &v, &len))
        return;
    uint32_t i = 0;
    while (i < len) {
        uint32_t *dst = 0;
        if (len - i >= 3 && memcmp(v + i, "rx:", 3) == 0)
            dst = &n->loss_rx;
        else if (len - i >= 3 && memcmp(v + i, "tx:", 3) == 0)
            dst = &n->loss_tx;
        else
            fail("%s: bad nanox.m5.loss value", mode);
        i += 3;
        uint32_t k = 0, digits = 0;
        while (i < len && v[i] >= '0' && v[i] <= '9' && digits < 5) {
            k = k * 10 + (uint32_t)(v[i++] - '0');
            digits++;
        }
        if (!digits || k < 2 || k > 1000 || (i < len && v[i] != ','))
            fail("%s: bad nanox.m5.loss value", mode);
        *dst = k;
        if (i < len)
            i++;
    }
}

__attribute__((noreturn)) static void run(const char *mode, uint64_t flags)
{
    struct nx_netdev *n = nx_net_open();
    if (!n)
        fail("%s: no virtio-net device", mode);
    parse_loss(mode, n);
    if (!nx_rng_open())
        fail("%s: no virtio-rng device (entropy source)", mode);
    nx_rtc_init();
    struct nx_blkdev *b = nx_blk_open_data();
    if (!b)
        fail("%s: no data disk", mode);
    uint64_t now = nx_rtc_now(nx_timer_ticks, NX_TIMER_HZ);
    nx_printf("NANOX: m5 net=virtio-net link=%s loss_rx=%u loss_tx=%u rng=virtio-rng"
              " clock=rtc unix=%" NX_PRIu64 " flags=0x%" NX_PRIx64 "\n",
              nx_net_link_up(n) ? "up" : "down", n->loss_rx, n->loss_tx, now, flags);
    struct nx_core_opts o = {mode, flags | NX_M5_CORE_NET, 1, b, n};
    nx_core_session(&o);
}

void nx_m5_serve(void)
{
    run("m5-serve", 0);
}

void nx_m5_noretx(void)
{
    run("m5-noretx", NX_M5_CORE_NORETX);
}

void nx_m5_noretry(void)
{
    run("m5-noretry", NX_M5_CORE_NORETRY);
}

void nx_m5_flattel(void)
{
    run("m5-flattel", NX_M5_CORE_FLATTEL);
}
