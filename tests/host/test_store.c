/*
 * Host tests of nxstore (lib/store.c, M4): format, objects, versions,
 * retention and pins, full device, recovery from damaged roots and
 * superblocks, the consistency check, and a crash simulation that stops a
 * workload before every single device operation and mounts every
 * persistence outcome the device model allows (docs/m4-store.md §6).
 *
 * Device model (simdev): write() lands in a volatile cache; flush() makes
 * everything written so far durable.  At a crash, any subset of the
 * cached block writes may have reached the medium, and a block write may
 * be torn (half of its sectors new).  Reads see the newest data.
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/crc32.h>
#include <nanox/store.h>

#include "test.h"

/* ---- simulated device ------------------------------------------------------ */

#define SIM_MAX_BLOCKS 256u
#define SIM_MAX_PENDING 64u

struct simdev {
    uint64_t blocks;
    uint8_t disk[SIM_MAX_BLOCKS][ST_BLOCK];
    uint32_t npend;
    uint64_t pend_blk[SIM_MAX_PENDING];
    uint8_t pend[SIM_MAX_PENDING][ST_BLOCK];
    uint32_t ops;      /* writes and flushes attempted */
    uint32_t crash_at; /* 0: never; else the op with this number is not performed */
    int crashed;
    int fail_write_at; /* for error-path tests: op number whose write fails */
    uint32_t writes, flushes;
};

static int sim_read(void *ctx, uint64_t blk, uint32_t n, void *buf)
{
    struct simdev *d = ctx;
    for (uint32_t i = 0; i < n; i++) {
        uint8_t *dst = (uint8_t *)buf + (uint64_t)i * ST_BLOCK;
        if (blk + i >= d->blocks)
            return -1;
        memcpy(dst, d->disk[blk + i], ST_BLOCK);
        for (uint32_t p = 0; p < d->npend; p++)
            if (d->pend_blk[p] == blk + i)
                memcpy(dst, d->pend[p], ST_BLOCK);
    }
    return 0;
}

static int sim_op(struct simdev *d)
{
    if (d->crashed)
        return -1;
    d->ops++;
    if (d->crash_at && d->ops == d->crash_at) {
        d->crashed = 1;
        return -1;
    }
    if (d->fail_write_at && (int)d->ops == d->fail_write_at)
        return -1;
    return 0;
}

static int sim_write(void *ctx, uint64_t blk, uint32_t n, const void *buf)
{
    struct simdev *d = ctx;
    if (sim_op(d) != 0)
        return -1;
    for (uint32_t i = 0; i < n; i++) {
        if (blk + i >= d->blocks || d->npend == SIM_MAX_PENDING) {
            fprintf(stderr, "simdev: write out of range or cache full\n");
            abort();
        }
        d->pend_blk[d->npend] = blk + i;
        memcpy(d->pend[d->npend], (const uint8_t *)buf + (uint64_t)i * ST_BLOCK, ST_BLOCK);
        d->npend++;
    }
    d->writes++;
    return 0;
}

static void sim_apply(struct simdev *d, uint32_t p)
{
    memcpy(d->disk[d->pend_blk[p]], d->pend[p], ST_BLOCK);
}

static int sim_flush(void *ctx)
{
    struct simdev *d = ctx;
    if (sim_op(d) != 0)
        return -1;
    for (uint32_t p = 0; p < d->npend; p++)
        sim_apply(d, p);
    d->npend = 0;
    d->flushes++;
    return 0;
}

static struct st_dev sim_dev(struct simdev *d)
{
    struct st_dev dev = {d, d->blocks, sim_read, sim_write, sim_flush};
    return dev;
}

static void sim_init(struct simdev *d, uint64_t blocks)
{
    memset(d, 0, sizeof(*d));
    d->blocks = blocks;
}

/* ---- deterministic content ---------------------------------------------------- */

static uint8_t pattern_byte(uint32_t seed, uint32_t i)
{
    uint32_t x = seed * 2654435761u + i * 40503u;
    x ^= x >> 13;
    return (uint8_t)(x * 0x5bd1e995u >> 24);
}

static uint8_t databuf[ST_OBJ_MAX_BYTES], readbuf[ST_OBJ_MAX_BYTES];

static const uint8_t *make_data(uint32_t seed, uint32_t len)
{
    for (uint32_t i = 0; i < len; i++)
        databuf[i] = pattern_byte(seed, i);
    return databuf;
}

static struct store st, st2;
static struct simdev dev1, dev2;

static int put(struct store *s, const char *name, uint32_t seed, uint32_t len)
{
    return st_put(s, name, 1, make_data(seed, len), len);
}

static int read_matches(struct store *s, uint64_t gen, const char *name, uint32_t seed,
                        uint32_t len)
{
    uint32_t got = 0;
    if (st_read(s, gen, name, readbuf, sizeof(readbuf), &got, 0) != ST_OK || got != len)
        return 0;
    make_data(seed, len);
    return memcmp(readbuf, databuf, len) == 0;
}

/* ---- unit tests ---------------------------------------------------------------- */

static void test_crc32(void)
{
    CHECK_EQ_INT(nx_crc32("123456789", 9), 0xCBF43926u);
    CHECK_EQ_INT(nx_crc32("", 0), 0);
    uint32_t part = nx_crc32_update(0, "12345", 5);
    CHECK_EQ_INT(nx_crc32_update(part, "6789", 4), 0xCBF43926u);
}

static void test_layout(void)
{
    CHECK_EQ_INT(offsetof(struct st_super, gen), 32);
    CHECK_EQ_INT(offsetof(struct st_super, root_blk), 40);
    CHECK_EQ_INT(offsetof(struct st_super, crc), ST_BLOCK - 4);
    CHECK_EQ_INT(offsetof(struct st_root, nobj), 48);
    CHECK_EQ_INT(offsetof(struct st_root, label), 72);
    CHECK_EQ_INT(offsetof(struct st_root, hist), 128);
    CHECK_EQ_INT(offsetof(struct st_root, pin), 256);
    CHECK_EQ_INT(offsetof(struct st_root, log), 384);
    CHECK_EQ_INT(offsetof(struct st_root, obj), 896);
    CHECK_EQ_INT(offsetof(struct st_root, crc), ST_BLOCK - 4);
    CHECK_EQ_INT(offsetof(struct st_obj, name), 36);
}

static void format_mount(uint64_t blocks, uint32_t retain)
{
    sim_init(&dev1, blocks);
    struct st_dev d = sim_dev(&dev1);
    CHECK_EQ_INT(st_format(&st, &d, 0x5eed, retain, 7), ST_OK);
    CHECK_EQ_INT(st_mount(&st, &d, 8, 0), ST_OK);
}

static int commit_one(const char *name, uint32_t seed, uint32_t len, const char *label)
{
    uint64_t g;
    if (st_begin(&st) != ST_OK)
        return -100;
    int r = put(&st, name, seed, len);
    if (r != ST_OK) {
        st_abort(&st);
        return r;
    }
    return st_commit(&st, label, &g);
}

static void test_format_and_objects(void)
{
    struct st_check_report cr;
    format_mount(64, 4);
    CHECK_EQ_INT(st.cur.gen, 1);
    CHECK_EQ_INT(st.cur.nobj, 0);
    CHECK_EQ_INT(st.rep.slot, 0);
    CHECK_EQ_INT(st.rep.slot_state[1], ST_SLOT_EMPTY);
    CHECK_EQ_INT(st_free_blocks(&st), 64 - 3);
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);

    /* A mount never writes. */
    uint32_t w = dev1.writes, f = dev1.flushes;
    struct st_dev d = sim_dev(&dev1);
    CHECK_EQ_INT(st_mount(&st, &d, 9, 0), ST_OK);
    CHECK_EQ_INT(dev1.writes, w);
    CHECK_EQ_INT(dev1.flushes, f);

    uint64_t g = 0;
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_begin(&st), ST_E_STATE);
    CHECK_EQ_INT(put(&st, "cfg/a", 1, 100), ST_OK);
    CHECK_EQ_INT(put(&st, "blob/b", 2, 9000), ST_OK);
    CHECK_EQ_INT(st_put(&st, "empty", 3, "", 0), ST_OK);
    CHECK_EQ_INT(put(&st, "bad name", 1, 1), ST_E_NAME);
    CHECK_EQ_INT(put(&st, "", 1, 1), ST_E_NAME);
    CHECK_EQ_INT(put(&st, "x23456789012345678901234567890", 1, 1), ST_E_NAME);
    CHECK_EQ_INT(st_put(&st, "big", 1, databuf, ST_OBJ_MAX_BYTES + 1), ST_E_TOOBIG);
    /* Not visible before the commit. */
    CHECK(st_find(&st.cur, "cfg/a") == NULL);
    uint32_t writes_before_commit = dev1.writes;
    CHECK_EQ_INT(st_commit(&st, "t1", &g), ST_OK);
    CHECK_EQ_INT(g, 2);
    /* Protocol: root, flush, superblock, flush. */
    CHECK_EQ_INT(dev1.writes - writes_before_commit, 2);
    CHECK_EQ_INT(st.cur_slot, 1);
    CHECK(read_matches(&st, 0, "cfg/a", 1, 100));
    CHECK(read_matches(&st, 0, "blob/b", 2, 9000));
    uint32_t len = 99;
    struct st_obj meta;
    CHECK_EQ_INT(st_read(&st, 0, "empty", readbuf, 0, &len, &meta), ST_OK);
    CHECK_EQ_INT(len, 0);
    CHECK_EQ_INT(meta.nblk, 0);
    CHECK_EQ_INT(meta.kind, 3);
    const struct st_obj *a = st_find(&st.cur, "cfg/a");
    CHECK(a && a->version == 1 && a->mod_gen == 2 && a->oid == 1);
    CHECK_EQ_INT(st_find(&st.cur, "blob/b")->nblk, 3);
    CHECK_EQ_INT(st_read(&st, 0, "blob/b", readbuf, 10, &len, 0), ST_E_TOOBIG);
    CHECK_EQ_INT(st_read(&st, 0, "nope", readbuf, 10, &len, 0), ST_E_NOTFOUND);

    /* Overwrite: same identity, next version, old version kept in history. */
    CHECK_EQ_INT(commit_one("cfg/a", 11, 5000, "t2"), ST_OK);
    a = st_find(&st.cur, "cfg/a");
    CHECK(a && a->version == 2 && a->mod_gen == 3 && a->oid == 1);
    CHECK(read_matches(&st, 0, "cfg/a", 11, 5000));
    CHECK(read_matches(&st, 2, "cfg/a", 1, 100));
    CHECK_EQ_INT(st.cur_slot, 0);

    /* Delete. */
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_del(&st, "blob/b"), ST_OK);
    CHECK_EQ_INT(st_del(&st, "blob/b"), ST_E_NOTFOUND);
    CHECK_EQ_INT(st_commit(&st, "t3", &g), ST_OK);
    CHECK(st_find(&st.cur, "blob/b") == NULL);
    CHECK(read_matches(&st, 3, "blob/b", 2, 9000));
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);

    /* Remount from the device: the same state. */
    CHECK_EQ_INT(st_mount(&st2, &d, 10, 0), ST_OK);
    CHECK_EQ_INT(st2.cur.gen, 4);
    CHECK(memcmp(&st2.cur, &st.cur, sizeof(st.cur)) == 0);
    CHECK_EQ_INT(st2.rep.slot_state[st2.cur_slot], ST_SLOT_CURRENT);
    CHECK_EQ_INT(st2.rep.slot_state[1 - st2.cur_slot], ST_SLOT_OLDER);
    CHECK_EQ_INT(st2.rep.fallback, 0);
    CHECK_EQ_INT(st_check(&st2, &cr), ST_OK);

    /* Commit log and labels. */
    CHECK_EQ_INT(st.cur.nlog, 4);
    CHECK(strcmp(st.cur.log[0].label, "t3") == 0 && st.cur.log[0].gen == 4);
    CHECK(strcmp(st.cur.log[3].label, "format") == 0 && st.cur.log[3].gen == 1);
    CHECK_EQ_INT(st.cur.log[0].boot_id, 9);
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_commit(&st, "has space", &g), ST_E_NAME);
    CHECK_EQ_INT(st.in_tx, 0);

    /* Object table full. */
    format_mount(128, 2);
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    char name[8];
    int r = ST_OK;
    for (unsigned i = 0; i < ST_OBJ_MAX && r == ST_OK; i++) {
        snprintf(name, sizeof(name), "o%u", i);
        r = st_put(&st, name, 1, "", 0);
    }
    CHECK_EQ_INT(r, ST_OK);
    CHECK_EQ_INT(st_put(&st, "one-more", 1, "", 0), ST_E_OBJFULL);
    CHECK_EQ_INT(st_commit(&st, "full", &g), ST_OK);
    CHECK_EQ_INT(st.cur.nobj, ST_OBJ_MAX);
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
}

static void test_retention(void)
{
    struct st_check_report cr;
    uint64_t g;
    format_mount(96, 3);
    CHECK_EQ_INT(commit_one("a", 1, 5000, "c1"), ST_OK); /* gen 2 */
    CHECK_EQ_INT(commit_one("a", 2, 5000, "c2"), ST_OK); /* gen 3 */
    CHECK(st_is_retained(&st, 1) && st_is_retained(&st, 2) && st_is_retained(&st, 3));
    CHECK_EQ_INT(commit_one("a", 3, 5000, "c3"), ST_OK); /* gen 4: retains 4, 3, 2 */
    CHECK(!st_is_retained(&st, 1));
    CHECK(st_is_retained(&st, 2));
    CHECK_EQ_INT(st_read(&st, 1, "a", readbuf, sizeof(readbuf), 0, 0), ST_E_PRUNED);
    CHECK(read_matches(&st, 2, "a", 1, 5000));
    CHECK_EQ_INT(st.cur.nhist, 2);

    /* Pin generation 2, then move on: it stays readable. */
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_pin(&st, "keep", 2), ST_OK);
    CHECK_EQ_INT(st_pin(&st, "keep", 3), ST_E_EXISTS);
    CHECK_EQ_INT(st_pin(&st, "old", 1), ST_E_PRUNED);
    CHECK_EQ_INT(st_pin(&st, "Bad!", 3), ST_E_NAME);
    CHECK_EQ_INT(st_commit(&st, "pin", &g), ST_OK); /* gen 5 */
    for (int i = 0; i < 4; i++)
        CHECK_EQ_INT(commit_one("a", 10 + i, 5000, "more"), ST_OK); /* gens 6..9 */
    CHECK(st_is_retained(&st, 2));
    CHECK(!st_is_retained(&st, 3));
    CHECK(read_matches(&st, 2, "a", 1, 5000));
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
    uint32_t free_pinned = st_free_blocks(&st);

    /* Unpin: released by the commit that drops the pin. */
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_unpin(&st, "nope"), ST_E_NOTFOUND);
    CHECK_EQ_INT(st_unpin(&st, "keep"), ST_OK);
    CHECK_EQ_INT(st_commit(&st, "unpin", &g), ST_OK); /* gen 10 */
    CHECK(!st_is_retained(&st, 2));
    /* Released: the roots of generations 7 (out of the window) and 2
     * (unpinned) and their extents of "a" (2 blocks each); taken: the new
     * root. */
    CHECK_EQ_INT(st_free_blocks(&st), free_pinned + 5);

    /* Pins survive a remount. */
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_pin(&st, "p1", 9), ST_OK);
    CHECK_EQ_INT(st_pin(&st, "p2", 10), ST_OK);
    CHECK_EQ_INT(st_commit(&st, "pins", &g), ST_OK);
    struct st_dev d = sim_dev(&dev1);
    CHECK_EQ_INT(st_mount(&st2, &d, 1, 0), ST_OK);
    CHECK_EQ_INT(st2.cur.npin, 2);
    CHECK(st_is_retained(&st2, 9) && st_is_retained(&st2, 10));
    CHECK_EQ_INT(st_check(&st2, &cr), ST_OK);

    /* Prune: the next generation keeps only itself and its parent. */
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(st_unpin(&st, "p1"), ST_OK);
    CHECK_EQ_INT(st_unpin(&st, "p2"), ST_OK);
    st_prune(&st);
    CHECK_EQ_INT(st_commit(&st, "prune", &g), ST_OK);
    CHECK_EQ_INT(st.cur.nhist, 1);
    CHECK(st_is_retained(&st, g - 1) && !st_is_retained(&st, g - 2));
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
    /* Only the current and the parent root, and one extent of "a"
     * (shared by both) are in use. */
    CHECK_EQ_INT(st_free_blocks(&st), 96 - 2 - 2 - 2);

    /* The commit log keeps ST_LOG_MAX entries. */
    for (int i = 0; i < 20; i++) {
        CHECK_EQ_INT(st_begin(&st), ST_OK);
        CHECK_EQ_INT(st_commit(&st, "tick", &g), ST_OK);
    }
    CHECK_EQ_INT(st.cur.nlog, ST_LOG_MAX);
    CHECK_EQ_INT(st.cur.log[ST_LOG_MAX - 1].gen, g - ST_LOG_MAX + 1);
}

static void test_full(void)
{
    struct st_check_report cr;
    uint64_t g;
    format_mount(48, 2);
    /* 48 blocks: 2 superblocks, 1 root; puts stop ST_RESERVE_BLOCKS short. */
    int r = ST_OK, n = 0;
    char name[8];
    while (r == ST_OK && n < 40) {
        snprintf(name, sizeof(name), "f%d", n);
        r = commit_one(name, (uint32_t)n, 8192, "fill");
        if (r == ST_OK)
            n++;
    }
    CHECK_EQ_INT(r, ST_E_NOSPC);
    CHECK(n >= 8);
    CHECK(st_free_blocks(&st) >= ST_RESERVE_BLOCKS - 1);
    uint64_t gen_before = st.cur.gen;
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
    /* The failed transaction changed nothing, on disk or in memory. */
    struct st_dev d = sim_dev(&dev1);
    CHECK_EQ_INT(st_mount(&st2, &d, 2, 0), ST_OK);
    CHECK_EQ_INT(st2.cur.gen, gen_before);
    CHECK_EQ_INT(st2.cur.nobj, (uint32_t)n);
    /* An ordinary transaction with a record of 3 blocks does not fit... */
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(put(&st, "record", 1, 3 * ST_BLOCK), ST_E_NOSPC);
    /* ...a releasing one may use the reserve for it. */
    st_allow_reserve(&st);
    CHECK_EQ_INT(put(&st, "record", 1, 3 * ST_BLOCK), ST_OK);
    for (int i = 0; i < 4; i++) {
        snprintf(name, sizeof(name), "f%d", i);
        CHECK_EQ_INT(st_del(&st, name), ST_OK);
    }
    st_prune(&st);
    CHECK_EQ_INT(st_commit(&st, "delete", &g), ST_OK);
    /* The space returns only after the parent generation that still
     * references it has been released. */
    CHECK_EQ_INT(commit_one("new", 99, 8192, "again"), ST_E_NOSPC);
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    st_allow_reserve(&st);
    CHECK_EQ_INT(put(&st, "record", 2, 3 * ST_BLOCK), ST_OK);
    CHECK_EQ_INT(st_commit(&st, "release", &g), ST_OK);
    CHECK_EQ_INT(commit_one("new", 99, 8192, "again"), ST_OK);
    CHECK(read_matches(&st, 0, "new", 99, 8192));
    CHECK(read_matches(&st, 0, "record", 2, 3 * ST_BLOCK));
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
}

static void corrupt_block(struct simdev *d, uint64_t blk, uint32_t off)
{
    d->disk[blk][off] ^= 0x40;
}

static void test_recovery(void)
{
    struct st_check_report cr;
    struct st_dev d;
    uint64_t g;
    format_mount(64, 3);
    CHECK_EQ_INT(commit_one("a", 1, 100, "g2"), ST_OK);
    CHECK_EQ_INT(commit_one("a", 2, 100, "g3"), ST_OK);
    CHECK_EQ_INT(commit_one("a", 3, 100, "g4"), ST_OK);
    uint32_t root4 = st.cur_blk;
    int slot4 = st.cur_slot;
    d = sim_dev(&dev1);

    /* Damaged current root: fall back to generation 3 in the other slot. */
    corrupt_block(&dev1, root4, 1000);
    CHECK_EQ_INT(st_mount(&st, &d, 3, 0), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 3);
    CHECK_EQ_INT(st.rep.fallback, 1);
    CHECK_EQ_INT(st.rep.rejected_gen, 4);
    CHECK_EQ_INT(st.rep.rejected_state, ST_SLOT_BAD_ROOT);
    CHECK_EQ_INT(st.rep.slot_state[slot4], ST_SLOT_BAD_ROOT);
    CHECK_EQ_INT(st.rep.max_gen, 4);
    CHECK(read_matches(&st, 0, "a", 2, 100));
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
    /* The next commit gets generation 5 and overwrites the damaged slot,
     * never the good one. */
    CHECK_EQ_INT(commit_one("a", 5, 100, "g5"), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 5);
    CHECK_EQ_INT(st.cur_slot, slot4);
    CHECK_EQ_INT(st.cur.parent_gen, 3);
    CHECK_EQ_INT(st_mount(&st2, &d, 4, 0), ST_OK);
    CHECK_EQ_INT(st2.cur.gen, 5);
    CHECK_EQ_INT(st2.rep.fallback, 0);

    /* Damaged data extent of the current generation: fall back as well. */
    const struct st_obj *a = st_find(&st2.cur, "a");
    corrupt_block(&dev1, a->blk, 5);
    CHECK_EQ_INT(st_mount(&st, &d, 5, 0), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 3);
    CHECK_EQ_INT(st.rep.rejected_state, ST_SLOT_BAD_DATA);
    corrupt_block(&dev1, a->blk, 5); /* repair */

    /* Torn superblock (half of its sectors from an older write). */
    CHECK_EQ_INT(st_mount(&st, &d, 6, 0), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 5);
    memset(dev1.disk[st.cur_slot] + 2048, 0, 2048);
    CHECK_EQ_INT(st_mount(&st, &d, 7, 0), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 3);
    CHECK_EQ_INT(st.rep.slot_state[slot4], ST_SLOT_BAD_CRC);

    /* Both superblocks damaged: unmountable, and mount still writes nothing. */
    uint32_t w = dev1.writes;
    corrupt_block(&dev1, 0, 100);
    corrupt_block(&dev1, 1, 100);
    CHECK_EQ_INT(st_mount(&st, &d, 8, 0), ST_E_NOROOT);
    CHECK_EQ_INT(st.mounted, 0);
    CHECK_EQ_INT(st_begin(&st), ST_E_STATE);
    CHECK_EQ_INT(dev1.writes, w);

    /* Blank device. */
    sim_init(&dev2, 64);
    struct st_dev d2 = sim_dev(&dev2);
    CHECK_EQ_INT(st_mount(&st, &d2, 1, 0), ST_E_NOROOT);
    CHECK_EQ_INT(st.rep.slot_state[0], ST_SLOT_EMPTY);
    sim_init(&dev2, 8);
    d2 = sim_dev(&dev2);
    CHECK_EQ_INT(st_format(&st, &d2, 1, 2, 1), ST_E_FORMAT);
    sim_init(&dev2, 64);
    d2 = sim_dev(&dev2);
    CHECK_EQ_INT(st_format(&st, &d2, 1, 1, 1), ST_E_FORMAT);
    CHECK_EQ_INT(st_format(&st, &d2, 1, ST_RETAIN_MAX + 1, 1), ST_E_FORMAT);

    /* The consistency check finds a damaged retained root. */
    format_mount(64, 3);
    CHECK_EQ_INT(commit_one("a", 1, 100, "g2"), ST_OK);
    CHECK_EQ_INT(commit_one("a", 2, 100, "g3"), ST_OK);
    corrupt_block(&dev1, st.cur.hist[1].blk, 77);
    CHECK_EQ_INT(st_check(&st, &cr), ST_E_CORRUPT);
    CHECK(cr.problems > 0 && strstr(cr.first, "root_invalid_gen") != NULL);
    corrupt_block(&dev1, st.cur.hist[1].blk, 77);
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);

    /* Device errors: before the superblock nothing changes; after it the
     * outcome is unknown and the store unmounts itself. */
    format_mount(64, 3);
    CHECK_EQ_INT(commit_one("a", 1, 100, "g2"), ST_OK);
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(put(&st, "b", 1, 100), ST_OK);
    dev1.fail_write_at = (int)dev1.ops + 1; /* the root write */
    CHECK_EQ_INT(st_commit(&st, "x", &g), ST_E_IO);
    CHECK_EQ_INT(st.mounted, 1);
    CHECK_EQ_INT(st.cur.gen, 2);
    CHECK_EQ_INT(st_begin(&st), ST_OK);
    CHECK_EQ_INT(put(&st, "b", 1, 100), ST_OK);
    dev1.fail_write_at = (int)dev1.ops + 4; /* the flush after the superblock */
    CHECK_EQ_INT(st_commit(&st, "y", &g), ST_E_UNKNOWN);
    CHECK_EQ_INT(st.mounted, 0);
    dev1.fail_write_at = 0;
    d = sim_dev(&dev1);
    CHECK_EQ_INT(st_mount(&st, &d, 9, 0), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 3); /* the superblock write itself had succeeded */
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
}

/* ---- crash simulation ------------------------------------------------------------ */

#define SIM_BLOCKS 64u
#define SIM_RETAIN 3u
#define MODEL_GENS 64u
#define MODEL_OBJS 16u

struct mobj {
    char name[16];
    uint32_t seed, len;
};

struct mstate {
    int valid;
    uint32_t n;
    struct mobj o[MODEL_OBJS];
};

static struct mstate model[MODEL_GENS];
static struct mstate cur_model;

static void m_put(const char *name, uint32_t seed, uint32_t len)
{
    for (uint32_t i = 0; i < cur_model.n; i++)
        if (strcmp(cur_model.o[i].name, name) == 0) {
            cur_model.o[i].seed = seed;
            cur_model.o[i].len = len;
            return;
        }
    struct mobj *o = &cur_model.o[cur_model.n++];
    snprintf(o->name, sizeof(o->name), "%s", name);
    o->seed = seed;
    o->len = len;
}

static void m_del(const char *name)
{
    for (uint32_t i = 0; i < cur_model.n; i++)
        if (strcmp(cur_model.o[i].name, name) == 0) {
            cur_model.o[i] = cur_model.o[--cur_model.n];
            return;
        }
}

/* One transaction of the workload: a list of actions. */
enum { A_PUT, A_DEL, A_PIN, A_UNPIN, A_PRUNE };

struct action {
    int kind;
    const char *name;
    uint32_t len; /* A_PUT; A_PIN: generation offset from the durable one (0: current) */
};

#define STEP(...) {__VA_ARGS__, {-1, 0, 0}}

static const struct action WORKLOAD[][5] = {
    STEP({A_PUT, "cfg/mode", 60}),
    STEP({A_PUT, "blob/one", 9000}, {A_PUT, "empty", 0}),
    STEP({A_PUT, "cfg/mode", 70}, {A_PUT, "core/tasks", 5000}),
    STEP({A_DEL, "blob/one", 0}, {A_PIN, "p1", 0}),
    STEP({A_PUT, "blob/two", 4096}, {A_PUT, "blob/three", 8192}),
    STEP({A_PRUNE, 0, 0}, {A_PUT, "cfg/mode", 200}),
    STEP({A_PUT, "blob/four", 12000}, {A_DEL, "empty", 0}),
    STEP({A_UNPIN, "p1", 0}, {A_PUT, "core/tasks", 300}),
    STEP({A_DEL, "cfg/mode", 0}),
    STEP({A_PUT, "blob/big", 32768}),
    STEP({A_PUT, "blob/two", 100}, {A_DEL, "blob/three", 0}),
};
#define NSTEPS (sizeof(WORKLOAD) / sizeof(WORKLOAD[0]))

struct run_result {
    uint64_t acked;     /* last generation reported saved */
    uint64_t attempted; /* generation of the interrupted commit, else acked */
    int crashed;
    int error;          /* unexpected error without a crash */
};

/* Runs the workload on a freshly formatted device until it ends or the
 * device crashes. */
static void run_workload(struct simdev *d, uint32_t crash_at, uint32_t flags,
                         struct run_result *res)
{
    struct st_dev dev;
    sim_init(d, SIM_BLOCKS);
    dev = sim_dev(d);
    memset(res, 0, sizeof(*res));
    memset(&cur_model, 0, sizeof(cur_model));
    if (st_format(&st, &dev, 0xC0FFEE, SIM_RETAIN, 1) != ST_OK ||
        st_mount(&st, &dev, 2, flags) != ST_OK) {
        res->error = 1;
        return;
    }
    model[1] = cur_model;
    model[1].valid = 1;
    res->acked = res->attempted = 1;
    d->ops = 0;
    d->crash_at = crash_at;
    for (uint32_t step = 0; step < NSTEPS; step++) {
        const struct action *a = WORKLOAD[step];
        if (st_begin(&st) != ST_OK) {
            res->error = 1;
            return;
        }
        res->attempted = st_next_gen(&st);
        int r = ST_OK;
        for (; a->kind >= 0 && r == ST_OK; a++) {
            switch (a->kind) {
            case A_PUT:
                r = put(&st, a->name, step * 100 + a->len, a->len);
                if (r == ST_OK)
                    m_put(a->name, step * 100 + a->len, a->len);
                break;
            case A_DEL:
                r = st_del(&st, a->name);
                m_del(a->name);
                break;
            case A_PIN: r = st_pin(&st, a->name, st.cur.gen - a->len); break;
            case A_UNPIN: r = st_unpin(&st, a->name); break;
            case A_PRUNE: st_prune(&st); break;
            }
        }
        uint64_t g = 0;
        if (r == ST_OK)
            r = st_commit(&st, "step", &g);
        if (d->crashed) {
            res->crashed = 1;
            return;
        }
        if (r != ST_OK) {
            res->error = 1;
            return;
        }
        res->acked = res->attempted = g;
        if (g < MODEL_GENS) {
            model[g] = cur_model;
            model[g].valid = 1;
        }
    }
}

struct sim_stats {
    uint32_t points, outcomes, violations;
    char first[160];
};

static void violation(struct sim_stats *ss, uint32_t point, const char *what, uint64_t a,
                      uint64_t b)
{
    if (!ss->violations++)
        snprintf(ss->first, sizeof(ss->first), "crash before op %u: %s (%llu, %llu)", point,
                 what, (unsigned long long)a, (unsigned long long)b);
}

static int state_matches(struct store *s, uint64_t gen, const struct mstate *m)
{
    struct st_root *r = &s->work; /* scratch: no transaction is open */
    if (st_root_of(s, gen, r) != ST_OK || r->nobj != m->n)
        return 0;
    for (uint32_t i = 0; i < m->n; i++)
        if (!read_matches(s, gen, m->o[i].name, m->o[i].seed, m->o[i].len))
            return 0;
    return 1;
}

/* Mounts the outcome in dev2 and checks the invariants. */
static void check_outcome(struct sim_stats *ss, uint32_t point, const struct run_result *res,
                          uint32_t flags)
{
    struct st_dev dev = sim_dev(&dev2);
    struct st_check_report cr;
    ss->outcomes++;
    if (st_mount(&st2, &dev, 3, flags) != ST_OK) {
        violation(ss, point, "unmountable", res->acked, res->attempted);
        return;
    }
    uint64_t g = st2.cur.gen;
    if (g < res->acked || g > res->attempted) {
        violation(ss, point, "generation outside [acked, attempted]", g, res->acked);
        return;
    }
    if (g >= MODEL_GENS || !model[g].valid || !state_matches(&st2, g, &model[g])) {
        violation(ss, point, "content differs from the model of the generation", g, 0);
        return;
    }
    for (uint32_t i = 0; i < st2.cur.nhist + st2.cur.npin; i++) {
        uint64_t h = i < st2.cur.nhist ? st2.cur.hist[i].gen
                                       : st2.cur.pin[i - st2.cur.nhist].ref.gen;
        if (h >= MODEL_GENS || !model[h].valid || !state_matches(&st2, h, &model[h])) {
            violation(ss, point, "retained generation differs from its model", h, g);
            return;
        }
    }
    if (st_check(&st2, &cr) != ST_OK)
        violation(ss, point, "consistency check failed", cr.problems, g);
}

/* dev2 := durable part of dev1 plus the pending writes selected by `mask`;
 * torn >= 0: pending write `torn` is applied half (which half: torn_hi). */
static void build_outcome(uint64_t mask, int torn, int torn_hi)
{
    memcpy(dev2.disk, dev1.disk, sizeof(dev1.disk));
    dev2.blocks = dev1.blocks;
    dev2.npend = 0;
    dev2.crash_at = 0;
    dev2.crashed = 0;
    dev2.fail_write_at = 0;
    for (uint32_t p = 0; p < dev1.npend; p++) {
        if ((int)p == torn) {
            uint32_t off = torn_hi ? ST_BLOCK / 2 : 0;
            memcpy(dev2.disk[dev1.pend_blk[p]] + off, dev1.pend[p] + off, ST_BLOCK / 2);
        } else if (mask >> p & 1) {
            memcpy(dev2.disk[dev1.pend_blk[p]], dev1.pend[p], ST_BLOCK);
        }
    }
}

static uint64_t lcg(uint64_t *x)
{
    *x = *x * 6364136223846793005ull + 1442695040888963407ull;
    return *x >> 11;
}

/* All crash points of the workload; returns the statistics.  stop_early:
 * stop at the first violation (negative controls). */
static void crash_sim(uint32_t flags, int stop_early, struct sim_stats *ss)
{
    struct run_result ref, res;
    memset(ss, 0, sizeof(*ss));
    memset(model, 0, sizeof(model));
    run_workload(&dev1, 0, flags, &ref);
    uint32_t total_ops = dev1.ops;
    if (ref.error || ref.crashed || total_ops == 0) {
        violation(ss, 0, "reference run failed", ref.error, total_ops);
        return;
    }
    uint64_t seed = 12345;
    for (uint32_t point = 1; point <= total_ops + 1; point++) {
        run_workload(&dev1, point, flags, &res);
        if (res.error) {
            violation(ss, point, "workload error without crash", 0, 0);
            return;
        }
        ss->points++;
        uint32_t p = dev1.npend;
        uint64_t all = p >= 64 ? ~0ull : ((1ull << p) - 1);
        if (p <= 6) {
            for (uint64_t m = 0; m <= all; m++) {
                build_outcome(m, -1, 0);
                check_outcome(ss, point, &res, flags);
            }
        } else {
            for (uint32_t k = 0; k <= p; k++) { /* prefixes and suffixes */
                build_outcome(k == 64 ? ~0ull : ((1ull << k) - 1), -1, 0);
                check_outcome(ss, point, &res, flags);
                build_outcome(all & ~((k == 64 ? ~0ull : (1ull << k)) - 1), -1, 0);
                check_outcome(ss, point, &res, flags);
            }
            for (uint32_t k = 0; k < p; k++) { /* one write, all but one write */
                build_outcome(1ull << k, -1, 0);
                check_outcome(ss, point, &res, flags);
                build_outcome(all & ~(1ull << k), -1, 0);
                check_outcome(ss, point, &res, flags);
            }
            for (uint32_t k = 0; k < 16; k++) {
                build_outcome(lcg(&seed) & all, -1, 0);
                check_outcome(ss, point, &res, flags);
            }
        }
        /* Torn writes: the earlier writes landed, write k only half. */
        for (uint32_t k = 0; k < p; k++)
            for (int hi = 0; hi < 2; hi++) {
                build_outcome((1ull << k) - 1, (int)k, hi);
                check_outcome(ss, point, &res, flags);
            }
        if (stop_early && ss->violations)
            return;
    }
}

static void test_crash_simulation(void)
{
    struct sim_stats ss;
    crash_sim(0, 0, &ss);
    printf("store crash simulation: %u crash points, %u outcomes mounted, %u violations%s%s\n",
           ss.points, ss.outcomes, ss.violations, ss.violations ? ": " : "", ss.first);
    CHECK(ss.points > 50);
    CHECK(ss.outcomes > 300);
    CHECK_EQ_INT(ss.violations, 0);

    /* Negative controls: each broken variant must be caught. */
    static const struct {
        uint32_t flags;
        const char *name;
    } controls[] = {
        {ST_TEST_NO_FINAL_FLUSH, "saved before the superblock flush"},
        {ST_TEST_NO_BARRIER | ST_TEST_NO_FINAL_FLUSH, "no flush at all"},
        {ST_TEST_NO_BARRIER | ST_TEST_NO_VERIFY, "no barrier and no verification at mount"},
        {ST_TEST_SAME_SLOT, "one superblock slot"},
        {ST_TEST_IN_PLACE, "blocks of retained generations reused"},
    };
    for (unsigned i = 0; i < sizeof(controls) / sizeof(controls[0]); i++) {
        crash_sim(controls[i].flags, 1, &ss);
        printf("store crash simulation, negative control \"%s\": %s\n", controls[i].name,
               ss.violations ? ss.first : "NOT DETECTED");
        CHECK(ss.violations > 0);
    }
    /* Either the barrier or the verification at mount alone keeps the
     * store consistent under this device model (both are kept). */
    crash_sim(ST_TEST_NO_BARRIER, 0, &ss);
    printf("store crash simulation without the barrier (verification only): %u violations\n",
           ss.violations);
    CHECK_EQ_INT(ss.violations, 0);
    crash_sim(ST_TEST_NO_VERIFY, 0, &ss);
    printf("store crash simulation without verification at mount (barrier only): %u violations\n",
           ss.violations);
    CHECK_EQ_INT(ss.violations, 0);
}

/* ---- image written by the host tools ---------------------------------------------------- */

static uint8_t *image;
static uint64_t image_blocks;

static int img_read(void *ctx, uint64_t blk, uint32_t n, void *buf)
{
    (void)ctx;
    if (blk + n > image_blocks)
        return -1;
    memcpy(buf, image + blk * ST_BLOCK, (uint64_t)n * ST_BLOCK);
    return 0;
}

static int img_write(void *ctx, uint64_t blk, uint32_t n, const void *buf)
{
    (void)ctx;
    (void)blk;
    (void)n;
    (void)buf;
    return -1;
}

static int img_flush(void *ctx)
{
    (void)ctx;
    return -1;
}

static void test_data_image(const char *path)
{
    if (!path) {
        printf("store: no data image given, skipping the image check\n");
        return;
    }
    FILE *f = fopen(path, "rb");
    CHECK(f != NULL);
    if (!f)
        return;
    fseek(f, 0, SEEK_END);
    long size = ftell(f);
    fseek(f, 0, SEEK_SET);
    image = malloc((size_t)size);
    CHECK(image && fread(image, 1, (size_t)size, f) == (size_t)size);
    fclose(f);
    image_blocks = (uint64_t)size / ST_BLOCK;
    struct st_dev d = {0, image_blocks, img_read, img_write, img_flush};
    struct st_check_report cr;
    CHECK_EQ_INT(st_mount(&st, &d, 1, 0), ST_OK);
    CHECK_EQ_INT(st.cur.gen, 1);
    CHECK_EQ_INT(st.cur.nobj, 0);
    CHECK(strcmp(st.cur.label, "format") == 0);
    CHECK_EQ_INT(st.cur.retain, 4);
    CHECK_EQ_INT(st.rep.slot_state[1], ST_SLOT_EMPTY);
    CHECK_EQ_INT(st_check(&st, &cr), ST_OK);
    free(image);
}

void test_store(const char *data_image)
{
    test_crc32();
    test_layout();
    test_format_and_objects();
    test_retention();
    test_full();
    test_recovery();
    test_crash_simulation();
    test_data_image(data_image);
}
