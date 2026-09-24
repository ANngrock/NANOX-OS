/*
 * M4 test modes (see m4test.h and docs/m4-store.md).
 *
 *   m4-blk     the virtio-blk driver alone: both devices found, the boot
 *              disk read (GPT) and refused for writing, the data disk
 *              written, flushed, read back and restored, the emulated
 *              volatile cache of the test layer checked
 *   m4-serve   bin/core with the data disk serves the host bridge
 *   m4-work    bin/core runs its built-in store workload (no bridge)
 *   m4-check   bin/core mounts the store, recovers, prints and checks it
 *
 * Parameters of the crash-test layer (kernel/dev/blk.h), any M4 mode:
 *   nanox.m4.cache=device|volatile   emulate a volatile write cache
 *   nanox.m4.crash=<K>               stop before write/flush request K
 *   nanox.m4.lose=all|none|torn|reorder
 *                                     which unflushed writes survive the stop
 *   nanox.m4.flush=device|noop       negative control: flush does nothing
 */
#include <stdarg.h>
#include <stdint.h>

#include <nanox/diag.h>
#include <nanox/m3.h>
#include <nanox/m4.h>
#include <nanox/printf.h>
#include <nanox/string.h>

#include "dev/blk.h"
#include "dev/virtio_blk.h"
#include "kernel.h"
#include "m2test.h"
#include "m3test.h"
#include "m4test.h"

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

static int value_is(const char *v, uint32_t n, const char *lit)
{
    return n == nx_strlen(lit) && memcmp(v, lit, n) == 0;
}

/* Reads the nanox.m4.* parameters into nx_blk_test. */
static void parse_params(const char *mode)
{
    const char *v;
    uint32_t n;
    nx_blk_test.enabled = 1;
    if (nx_cmdline_value("nanox.m4.cache", &v, &n)) {
        if (value_is(v, n, "volatile"))
            nx_blk_test.volatile_cache = 1;
        else if (!value_is(v, n, "device"))
            fail("%s: bad nanox.m4.cache value", mode);
    }
    if (nx_cmdline_value("nanox.m4.flush", &v, &n)) {
        if (value_is(v, n, "noop"))
            nx_blk_test.flush_noop = 1;
        else if (!value_is(v, n, "device"))
            fail("%s: bad nanox.m4.flush value", mode);
    }
    if (nx_cmdline_value("nanox.m4.lose", &v, &n)) {
        static const char *const names[] = {"all", "none", "torn", "reorder"};
        int found = -1;
        for (int i = 0; i < 4; i++)
            if (value_is(v, n, names[i]))
                found = i;
        if (found < 0)
            fail("%s: bad nanox.m4.lose value", mode);
        nx_blk_test.lose = found;
    }
    if (nx_cmdline_value("nanox.m4.crash", &v, &n)) {
        uint32_t k = 0;
        if (n == 0 || n > 6)
            fail("%s: bad nanox.m4.crash value", mode);
        for (uint32_t i = 0; i < n; i++) {
            if (v[i] < '0' || v[i] > '9')
                fail("%s: bad nanox.m4.crash value", mode);
            k = k * 10 + (uint32_t)(v[i] - '0');
        }
        nx_blk_test.crash_at = k;
    }
}

static struct nx_blkdev *open_data(const char *mode)
{
    struct nx_blkdev *b = nx_blk_open_data();
    if (!b)
        fail("%s: no virtio-blk device with serial \"%s\" (data disk missing)", mode,
             NX_BLK_DATA_SERIAL);
    if (b->dev->read_only || !b->dev->has_flush)
        fail("%s: data disk is read-only or has no flush command", mode);
    return b;
}

__attribute__((noreturn)) static void run_core(const char *mode, uint64_t flags, int bridge)
{
    parse_params(mode);
    struct nx_blkdev *b = open_data(mode);
    nx_blk_test_setup();
    struct nx_core_opts o = {mode, flags, bridge, b, 0};
    nx_core_session(&o);
}

void nx_m4_serve(void)
{
    run_core("m4-serve", 0, 1);
}

void nx_m4_work(void)
{
    run_core("m4-work", NX_M4_CORE_WORKLOAD, 0);
}

void nx_m4_check(void)
{
    run_core("m4-check", NX_M4_CORE_CHECK, 0);
}

/* ---- m4-blk: the driver alone ----------------------------------------------------- */

static uint8_t buf_a[NX_BLK_IO_MAX * NX_BLK_SIZE], buf_b[NX_BLK_IO_MAX * NX_BLK_SIZE];
static uint8_t saved[NX_BLK_IO_MAX * NX_BLK_SIZE];

static void fill(uint8_t *p, uint32_t len, uint32_t seed)
{
    for (uint32_t i = 0; i < len; i++)
        p[i] = (uint8_t)((i * 131u + seed * 7u) ^ (i >> 9));
}

/* Reads `count` blocks straight from the device (bypassing the test layer). */
static void raw_read(struct nx_vblk *d, uint64_t blk, uint32_t count, uint8_t *dst)
{
    int st = nx_vblk_read(d, blk * 8u, count * NX_BLK_SIZE);
    if (st != NX_VBLK_OK)
        fail("m4-blk: device read of block %" NX_PRIu64 " failed: %s", blk, nx_vblk_strerror(st));
    for (uint32_t i = 0; i < count; i++)
        memcpy(dst + (uint64_t)i * NX_BLK_SIZE, nx_vblk_page(d, i), NX_BLK_SIZE);
}

void nx_m4_blk(void)
{
    uint32_t n = nx_vblk_probe();
    if (n != 2)
        fail("m4-blk: %u virtio-blk devices, expected 2 (boot disk and data disk)", n);
    struct nx_vblk *boot = 0;
    for (uint32_t i = 0; i < n; i++)
        if (nx_vblk_dev[i].read_only)
            boot = &nx_vblk_dev[i];
    if (!boot)
        fail("m4-blk: no read-only virtio-blk device (the boot disk is opened read-only)");

    /* Boot disk: protective MBR and GPT header written by tools/image/mkimage.py. */
    int st = nx_vblk_read(boot, 0, 1024);
    const uint8_t *s0 = nx_vblk_page(boot, 0);
    if (st != NX_VBLK_OK || s0[510] != 0x55 || s0[511] != 0xAA || memcmp(s0 + 512, "EFI PART", 8))
        fail("m4-blk: boot disk sector 0/1 is not MBR + GPT (%s)", nx_vblk_strerror(st));
    memset(nx_vblk_page(boot, 0), 0, 512);
    int wst = nx_vblk_write(boot, 0, 512);
    if (wst == NX_VBLK_OK)
        fail("m4-blk: the read-only boot disk accepted a write");
    nx_printf("NANOX: m4-blk boot disk sectors=%" NX_PRIu64 " mbr=55aa gpt=\"EFI PART\""
              " write=%s\n",
              boot->sectors, nx_vblk_strerror(wst));

    struct nx_blkdev *b = open_data("m4-blk");
    struct nx_vblk *d = b->dev;
    uint64_t last = b->blocks - NX_BLK_IO_MAX;
    uint32_t bytes = NX_BLK_IO_MAX * NX_BLK_SIZE;
    raw_read(d, last, NX_BLK_IO_MAX, saved);

    /* Write through the block layer (test layer off), flush, read back. */
    fill(buf_a, bytes, 1);
    if (nx_blk_write(b, last, NX_BLK_IO_MAX, buf_a) != NX_OK || nx_blk_flush(b) != NX_OK)
        fail("m4-blk: write or flush of the data disk failed");
    raw_read(d, last, NX_BLK_IO_MAX, buf_b);
    if (memcmp(buf_a, buf_b, bytes) != 0)
        fail("m4-blk: data read back differs from the data written");
    if (nx_blk_read(b, last + 3, 1, buf_b) != NX_OK ||
        memcmp(buf_b, buf_a + 3 * NX_BLK_SIZE, NX_BLK_SIZE) != 0)
        fail("m4-blk: single-block read through the block layer differs");
    if (nx_blk_read(b, b->blocks, 1, buf_b) != NX_EINVAL ||
        nx_blk_write(b, b->blocks - 1, 2, buf_a) != NX_EINVAL)
        fail("m4-blk: request beyond the capacity not rejected");
    nx_printf("NANOX: m4-blk data disk serial=\"%s\" blocks=%" NX_PRIu64
              " write+flush+readback=ok blocks=%" NX_PRIu64 "-%" NX_PRIu64 " range_check=ok\n",
              d->serial, b->blocks, last, last + NX_BLK_IO_MAX - 1);

    /* The emulated volatile cache: a write is visible to reads at once but
     * reaches the device only with the flush. */
    nx_blk_test_setup();
    nx_blk_test.enabled = 1;
    nx_blk_test.volatile_cache = 1;
    fill(buf_a, NX_BLK_SIZE, 2);
    if (nx_blk_write(b, last, 1, buf_a) != NX_OK)
        fail("m4-blk: cached write failed");
    uint32_t pending = nx_blk_pending();
    if (nx_blk_read(b, last, 1, buf_b) != NX_OK || memcmp(buf_a, buf_b, NX_BLK_SIZE) != 0)
        fail("m4-blk: cached write not visible to reads");
    raw_read(d, last, 1, buf_b);
    int on_device_before = memcmp(buf_a, buf_b, NX_BLK_SIZE) == 0;
    if (nx_blk_flush(b) != NX_OK)
        fail("m4-blk: flush of the cache failed");
    raw_read(d, last, 1, buf_b);
    int on_device_after = memcmp(buf_a, buf_b, NX_BLK_SIZE) == 0;
    if (pending != 1 || on_device_before || !on_device_after || nx_blk_pending() != 0)
        fail("m4-blk: emulated cache: pending=%u before_flush=%d after_flush=%d", pending,
             on_device_before, on_device_after);
    nx_printf("NANOX: m4-blk test cache pending=%u device_before_flush=old"
              " device_after_flush=new ops=%u\n",
              pending, nx_blk_test.ops);
    nx_blk_test.enabled = 0;
    nx_blk_test.volatile_cache = 0;

    /* Restore the original content. */
    if (nx_blk_write(b, last, NX_BLK_IO_MAX, saved) != NX_OK || nx_blk_flush(b) != NX_OK)
        fail("m4-blk: restoring the data disk failed");
    raw_read(d, last, NX_BLK_IO_MAX, buf_b);
    if (memcmp(saved, buf_b, bytes) != 0)
        fail("m4-blk: restore did not take");
    nx_printf("NANOX: m4-blk restored requests=%" NX_PRIu64 " errors=%" NX_PRIu64 "\n",
              d->requests, d->errors);
    nx_test_pass();
}
