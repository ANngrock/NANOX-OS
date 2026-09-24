/*
 * Block device object and the M4 crash-test layer; see blk.h.
 *
 * Runs inside system calls with interrupts disabled (single CPU, no
 * preemption), so the static state needs no locking.
 */
#include <nanox/diag.h>
#include <nanox/printf.h>
#include <nanox/string.h>
#include <nanox/syscall.h>

#include "blk.h"
#include "kernel.h"
#include "mm/mm.h"

#define SECTORS_PER_BLOCK (NX_BLK_SIZE / NX_VBLK_SECTOR)

struct nx_blk_test nx_blk_test;

static struct nx_blkdev data_dev = {{NX_OBJ_BLOCKDEV, 0, 0}, 0, 0, 0, 0, 0}; /* never freed */

/* Emulated volatile cache: unflushed writes in order, one block each. */
static struct {
    uint64_t blk;
    uint64_t pa;
} cache[NX_BLK_CACHE_PAGES];
static uint64_t free_pages[NX_BLK_CACHE_PAGES]; /* pages not holding a cached block */
static uint32_t cache_n, free_n, pool_ready;

const char *nx_blk_lose_name(int lose)
{
    switch (lose) {
    case NX_LOSE_ALL: return "all";
    case NX_LOSE_NONE: return "none";
    case NX_LOSE_TORN: return "torn";
    case NX_LOSE_REORDER: return "reorder";
    default: return "?";
    }
}

uint32_t nx_blk_pending(void)
{
    return cache_n;
}

void nx_blk_test_setup(void)
{
    if (pool_ready)
        return;
    for (uint32_t i = 0; i < NX_BLK_CACHE_PAGES; i++)
        free_pages[free_n++] = nx_page_alloc();
    pool_ready = 1;
}

struct nx_blkdev *nx_blk_open_data(void)
{
    if (data_dev.dev)
        return &data_dev;
    nx_vblk_probe();
    struct nx_vblk *d = nx_vblk_by_serial(NX_BLK_DATA_SERIAL);
    if (!d)
        return 0;
    data_dev.dev = d;
    data_dev.blocks = d->sectors / SECTORS_PER_BLOCK;
    return &data_dev;
}

/* ---- device access (whole blocks through the bounce pages) ----------------- */

static int dev_write(struct nx_blkdev *b, uint64_t blk, uint32_t count, const uint8_t *src)
{
    for (uint32_t i = 0; i < count; i++)
        memcpy(nx_vblk_page(b->dev, i), src + (uint64_t)i * NX_BLK_SIZE, NX_BLK_SIZE);
    return nx_vblk_write(b->dev, blk * SECTORS_PER_BLOCK, count * NX_BLK_SIZE) == NX_VBLK_OK
               ? NX_OK
               : NX_EIO;
}

static int dev_flush(struct nx_blkdev *b)
{
    return nx_vblk_flush(b->dev) == NX_VBLK_OK ? NX_OK : NX_EIO;
}

static int cache_writeback(struct nx_blkdev *b, uint32_t i)
{
    return dev_write(b, cache[i].blk, 1, nx_phys_to_virt(cache[i].pa));
}

/* Writes the oldest cached block to the device and drops it (a cache that
 * is full persists early, which the flush semantics allow). */
static int cache_evict_oldest(struct nx_blkdev *b)
{
    int st = cache_writeback(b, 0);
    free_pages[free_n++] = cache[0].pa;
    for (uint32_t i = 1; i < cache_n; i++)
        cache[i - 1] = cache[i];
    cache_n--;
    return st;
}

/* ---- crash point -------------------------------------------------------------- */

__attribute__((noreturn)) static void crash(struct nx_blkdev *b, const char *op)
{
    uint32_t pending = cache_n, persisted = 0;
    int torn = 0;
    int lose = nx_blk_test.lose;
    for (uint32_t i = 0; i < cache_n; i++) {
        int newest = i + 1 == cache_n;
        int keep = lose == NX_LOSE_NONE || (lose == NX_LOSE_TORN && !newest) ||
                   (lose == NX_LOSE_REORDER && newest);
        if (keep) {
            cache_writeback(b, i);
            persisted++;
        } else if (lose == NX_LOSE_TORN && newest) {
            /* First half of the block (sectors 0-3) new, the rest as on disk. */
            memcpy(nx_vblk_page(b->dev, 0), nx_phys_to_virt(cache[i].pa), NX_BLK_SIZE / 2);
            nx_vblk_write(b->dev, cache[i].blk * SECTORS_PER_BLOCK, NX_BLK_SIZE / 2);
            torn = 1;
        }
    }
    if (b->dev->has_flush)
        nx_vblk_flush(b->dev);
    nx_printf("NANOX: CRASH POINT io=%u op=%s pending=%u policy=%s persisted=%u torn=%d\n",
              nx_blk_test.ops, op, pending, nx_blk_lose_name(lose), persisted, torn);
    nx_debug_exit(NX_EXIT_CRASH_POINT);
}

/* Counts the operation; stops the machine if it is the crash point. */
static void count_op(struct nx_blkdev *b, const char *op, uint64_t blk, uint32_t count)
{
    if (!nx_blk_test.enabled)
        return;
    nx_blk_test.ops++;
    if (nx_blk_test.crash_at && nx_blk_test.ops == nx_blk_test.crash_at)
        crash(b, op);
    if (count)
        nx_printf("NANOX: m4 io %u %s blk=%" NX_PRIu64 " n=%u pending=%u\n", nx_blk_test.ops, op,
                  blk, count, cache_n);
    else
        nx_printf("NANOX: m4 io %u %s pending=%u%s\n", nx_blk_test.ops, op, cache_n,
                  nx_blk_test.flush_noop ? " noop" : "");
}

/* ---- operations ------------------------------------------------------------------- */

int nx_blk_read(struct nx_blkdev *b, uint64_t blk, uint32_t count, uint8_t *dst)
{
    if (count == 0 || count > NX_BLK_IO_MAX || blk >= b->blocks || count > b->blocks - blk)
        return NX_EINVAL;
    if (nx_vblk_read(b->dev, blk * SECTORS_PER_BLOCK, count * NX_BLK_SIZE) != NX_VBLK_OK)
        return NX_EIO;
    for (uint32_t i = 0; i < count; i++)
        memcpy(dst + (uint64_t)i * NX_BLK_SIZE, nx_vblk_page(b->dev, i), NX_BLK_SIZE);
    /* Newer data from the emulated cache, oldest first so the newest wins. */
    for (uint32_t c = 0; c < cache_n; c++)
        if (cache[c].blk >= blk && cache[c].blk < blk + count)
            memcpy(dst + (cache[c].blk - blk) * NX_BLK_SIZE, nx_phys_to_virt(cache[c].pa),
                   NX_BLK_SIZE);
    b->reads++;
    return NX_OK;
}

int nx_blk_write(struct nx_blkdev *b, uint64_t blk, uint32_t count, const uint8_t *src)
{
    if (count == 0 || count > NX_BLK_IO_MAX || blk >= b->blocks || count > b->blocks - blk)
        return NX_EINVAL;
    if (b->dev->read_only)
        return NX_EIO;
    count_op(b, "write", blk, count);
    b->writes++;
    if (!(nx_blk_test.enabled && nx_blk_test.volatile_cache && pool_ready))
        return dev_write(b, blk, count, src);
    for (uint32_t i = 0; i < count; i++) {
        if (free_n == 0 && cache_evict_oldest(b) != NX_OK)
            return NX_EIO;
        cache[cache_n].blk = blk + i;
        cache[cache_n].pa = free_pages[--free_n];
        memcpy(nx_phys_to_virt(cache[cache_n].pa), src + (uint64_t)i * NX_BLK_SIZE, NX_BLK_SIZE);
        cache_n++;
    }
    return NX_OK;
}

int nx_blk_flush(struct nx_blkdev *b)
{
    count_op(b, "flush", 0, 0);
    b->flushes++;
    if (nx_blk_test.enabled && nx_blk_test.flush_noop)
        return NX_OK; /* deliberately broken: success without effect */
    if (nx_blk_test.enabled && nx_blk_test.volatile_cache) {
        for (uint32_t i = 0; i < cache_n; i++)
            if (cache_writeback(b, i) != NX_OK)
                return NX_EIO;
        for (uint32_t i = 0; i < cache_n; i++)
            free_pages[free_n++] = cache[i].pa;
        cache_n = 0;
    }
    return dev_flush(b);
}
