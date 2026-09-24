/*
 * Block device object (M4): a virtio-blk device seen in 4096-byte blocks,
 * handed to bin/core as a handle (NX_OBJ_BLOCKDEV, rights READ/WRITE), and
 * the crash-test layer used by the M4 test modes (docs/m4-store.md §7):
 *
 *   - operation counting: every write and flush request is numbered;
 *   - crash point: before operation K the kernel stops the machine (exit
 *     status 43, "NANOX: CRASH POINT ...") as if power had failed there;
 *   - emulated volatile write cache: writes stay in kernel memory until a
 *     flush (reads see them); at the crash point the persistence policy
 *     decides which of the unflushed writes reach the disk:
 *       all      none of them (power loss, cache lost)
 *       none     all of them, in order (process crash: nothing lost)
 *       torn     all but the newest, and the newest only half (sectors 0-3)
 *       reorder  only the newest one (the device wrote out of order)
 *   - flush no-op (negative control): the flush request reports success
 *     without writing anything.
 *
 * Without the test layer, writes go straight to the device and a flush is
 * a VIRTIO_BLK_T_FLUSH request.
 */
#ifndef NANOX_KERNEL_DEV_BLK_H
#define NANOX_KERNEL_DEV_BLK_H

#include <stdint.h>

#include "obj/handle.h"
#include "virtio_blk.h"

#define NX_BLK_DATA_SERIAL "nanox-data"
#define NX_BLK_CACHE_PAGES 128u

enum nx_blk_lose { NX_LOSE_ALL = 0, NX_LOSE_NONE, NX_LOSE_TORN, NX_LOSE_REORDER };

struct nx_blkdev {
    struct nx_object base; /* NX_OBJ_BLOCKDEV, never freed */
    struct nx_vblk *dev;
    uint64_t blocks;
    uint64_t reads, writes, flushes;
};

struct nx_blk_test {
    int enabled;        /* count and trace operations */
    int volatile_cache; /* emulate a volatile write cache */
    int flush_noop;     /* negative control */
    uint32_t crash_at;  /* 0: no crash point */
    int lose;           /* enum nx_blk_lose */
    uint32_t ops;       /* write and flush requests so far */
};

extern struct nx_blk_test nx_blk_test;

/* Probes the virtio-blk devices and returns the data disk (serial
 * NX_BLK_DATA_SERIAL), or NULL. */
struct nx_blkdev *nx_blk_open_data(void);
/* Allocates the cache pages of the test layer (call before the resource
 * snapshot of a test mode). */
void nx_blk_test_setup(void);
/* Kernel buffers of count * NX_BLK_SIZE bytes, count <= NX_BLK_IO_MAX.
 * NX_OK or NX_EIO / NX_EINVAL. */
int nx_blk_read(struct nx_blkdev *b, uint64_t blk, uint32_t count, uint8_t *dst);
int nx_blk_write(struct nx_blkdev *b, uint64_t blk, uint32_t count, const uint8_t *src);
int nx_blk_flush(struct nx_blkdev *b);
const char *nx_blk_lose_name(int lose);
uint32_t nx_blk_pending(void);

#endif
