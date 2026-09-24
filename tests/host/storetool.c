/*
 * storetool: host front end of lib/store.c for cross-checks with the
 * independent Python implementation (tools/store/nxstore.py) in
 * tests/host/test_nxstore.py.
 *
 *   storetool sample IMG     format a 64-block store and commit a fixed
 *                            sequence (objects of 0..3 blocks, overwrite,
 *                            delete, pin, retention drops)
 *   storetool mount IMG      mount and print one line:
 *                            "mount <st_error> gen=<g> slot=<s> fallback=<0|1>
 *                             slots=<a>,<b> objects=<n> check=<ok|problem>"
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/store.h>

static uint8_t *disk;
static uint64_t nblocks;

static int rd(void *c, uint64_t b, uint32_t n, void *buf)
{
    (void)c;
    if (b + n > nblocks)
        return -1;
    memcpy(buf, disk + b * ST_BLOCK, (size_t)n * ST_BLOCK);
    return 0;
}

static int wr(void *c, uint64_t b, uint32_t n, const void *buf)
{
    (void)c;
    if (b + n > nblocks)
        return -1;
    memcpy(disk + b * ST_BLOCK, buf, (size_t)n * ST_BLOCK);
    return 0;
}

static int fl(void *c)
{
    (void)c;
    return 0;
}

static struct store st;
static uint8_t data[ST_OBJ_MAX_BYTES];

static int put(const char *name, uint32_t len, uint8_t seed)
{
    for (uint32_t i = 0; i < len; i++)
        data[i] = (uint8_t)(seed + i * 7u);
    return st_put(&st, name, 1, data, len);
}

static int sample(void)
{
    struct st_dev d = {0, nblocks, rd, wr, fl};
    uint64_t g;
    int r = st_format(&st, &d, 0x1234, 3, 42);
    r = r ? r : st_mount(&st, &d, 43, 0);
    r = r ? r : st_begin(&st);
    r = r ? r : put("cfg/a", 10, 1);
    r = r ? r : put("blob/x", 9000, 2);
    r = r ? r : st_put(&st, "empty", 2, data, 0);
    r = r ? r : st_commit(&st, "one", &g);
    r = r ? r : st_begin(&st);
    r = r ? r : put("cfg/a", 4096, 3);
    r = r ? r : st_pin(&st, "p", 2);
    r = r ? r : st_commit(&st, "two", &g);
    for (int i = 0; i < 4 && !r; i++) {
        r = st_begin(&st);
        r = r ? r : put("cfg/b", 100 + (uint32_t)i, (uint8_t)(10 + i));
        r = r ? r : st_commit(&st, "more", &g);
    }
    r = r ? r : st_begin(&st);
    r = r ? r : st_del(&st, "blob/x");
    r = r ? r : put("blob/y", 3 * ST_BLOCK + 1, 5);
    r = r ? r : st_commit(&st, "last", &g);
    return r;
}

static void mount_line(void)
{
    struct st_dev d = {0, nblocks, rd, wr, fl};
    int r = st_mount(&st, &d, 1, 0);
    struct st_check_report cr;
    memset(&cr, 0, sizeof(cr));
    int c = r == ST_OK ? st_check(&st, &cr) : ST_E_STATE;
    printf("mount %s gen=%llu slot=%d fallback=%d slots=%s,%s objects=%u check=%s\n",
           st_strerror(r), (unsigned long long)(r == ST_OK ? st.cur.gen : 0),
           r == ST_OK ? st.cur_slot : -1, st.rep.fallback,
           st_slot_state_name(st.rep.slot_state[0]), st_slot_state_name(st.rep.slot_state[1]),
           r == ST_OK ? st.cur.nobj : 0, c == ST_OK ? "ok" : "problem");
}

int main(int argc, char **argv)
{
    if (argc != 3)
        return 2;
    if (strcmp(argv[1], "sample") == 0) {
        nblocks = 64;
        disk = calloc(nblocks, ST_BLOCK);
        int r = sample();
        if (r) {
            fprintf(stderr, "storetool: sample failed: %s\n", st_strerror(r));
            return 1;
        }
        FILE *f = fopen(argv[2], "wb");
        if (!f || fwrite(disk, ST_BLOCK, nblocks, f) != nblocks || fclose(f))
            return 1;
        return 0;
    }
    if (strcmp(argv[1], "mount") == 0) {
        FILE *f = fopen(argv[2], "rb");
        if (!f)
            return 1;
        fseek(f, 0, SEEK_END);
        long size = ftell(f);
        fseek(f, 0, SEEK_SET);
        nblocks = (uint64_t)size / ST_BLOCK;
        disk = malloc((size_t)size);
        if (!disk || fread(disk, 1, (size_t)size, f) != (size_t)size)
            return 1;
        fclose(f);
        mount_line();
        return 0;
    }
    return 2;
}
