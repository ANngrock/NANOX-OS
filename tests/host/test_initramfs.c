/* Host tests for the cpio newc initramfs reader (kernel/initramfs.c). */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/elf_plan.h>
#include <nanox/syscall.h>

#include "initramfs.h"
#include "test.h"

static uint8_t buf[4096];
static size_t blen;

static void pad4(void)
{
    while (blen % 4)
        buf[blen++] = 0;
}

/* Appends one newc entry; namesize/filesize may be overridden (-1 = real). */
static void add_raw(const char *magic, uint32_t mode, const char *name, long long namesize,
                    const void *data, size_t size, long long filesize)
{
    size_t nlen = strlen(name) + 1;
    char hdr[111];
    snprintf(hdr, sizeof(hdr), "%s%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X", magic, 1u,
             mode, 0u, 0u, 1u, 0u, (unsigned)(filesize < 0 ? size : (size_t)filesize), 0u, 0u, 0u,
             0u, (unsigned)(namesize < 0 ? nlen : (size_t)namesize), 0u);
    memcpy(buf + blen, hdr, 110);
    blen += 110;
    memcpy(buf + blen, name, nlen);
    blen += nlen;
    pad4();
    memcpy(buf + blen, data, size);
    blen += size;
    pad4();
}

static void add(uint32_t mode, const char *name, const char *data)
{
    add_raw("070701", mode, name, -1, data, strlen(data), -1);
}

static void trailer(void)
{
    add(0, "TRAILER!!!", "");
}

static void valid_archive(void)
{
    blen = 0;
    memset(buf, 0, sizeof(buf));
    add(040755, "etc", "");
    add(0100644, "etc/a", "hello");
    trailer();
}

static int validate(uint32_t *entries, uint64_t *bad)
{
    return nx_cpio_validate(buf, blen, entries, bad);
}

static void expect_invalid(int status)
{
    uint32_t n;
    uint64_t bad;
    int st = validate(&n, &bad);
    CHECK_EQ_INT(st, status);
    if (st != status)
        fprintf(stderr, "  got %s, expected %s\n", nx_cpio_strerror(st), nx_cpio_strerror(status));
}

void test_initramfs(const char *initrd_path)
{
    uint32_t n;
    uint64_t bad;
    struct nx_cpio_entry e;

    valid_archive();
    CHECK_EQ_INT(validate(&n, &bad), NX_CPIO_OK);
    CHECK_EQ_INT(n, 2);
    CHECK_EQ_INT(nx_cpio_find(buf, blen, "etc/a", &e), NX_CPIO_OK);
    CHECK_EQ_INT(e.size, 5);
    CHECK(memcmp(e.data, "hello", 5) == 0);
    CHECK_EQ_INT(e.mode & NX_CPIO_MODE_TYPE, NX_CPIO_MODE_REG);
    CHECK_EQ_INT(nx_cpio_find(buf, blen, "etc", &e), NX_CPIO_OK);
    CHECK_EQ_INT(e.mode & NX_CPIO_MODE_TYPE, NX_CPIO_MODE_DIR);
    CHECK_EQ_INT(nx_cpio_find(buf, blen, "etc/", &e), NX_CPIO_E_NOT_FOUND);
    CHECK_EQ_INT(nx_cpio_find(buf, blen, "etc/ab", &e), NX_CPIO_E_NOT_FOUND);
    CHECK_EQ_INT(nx_cpio_find(buf, blen, "TRAILER!!!", &e), NX_CPIO_E_NOT_FOUND);

    /* Trailing zero padding after the trailer is allowed. */
    blen += 64;
    CHECK_EQ_INT(validate(&n, &bad), NX_CPIO_OK);

    valid_archive(); /* truncated in the first header */
    blen = 50;
    expect_invalid(NX_CPIO_E_TRUNCATED);
    blen = 0;
    expect_invalid(NX_CPIO_E_NO_TRAILER);

    valid_archive();
    buf[5] = '2'; /* 070702 (crc variant) is not accepted */
    expect_invalid(NX_CPIO_E_MAGIC);
    valid_archive();
    buf[6 + 8 * 6 + 3] = 'G'; /* filesize field */
    expect_invalid(NX_CPIO_E_HEX);

    valid_archive(); /* missing trailer */
    blen = 0;
    add(040755, "etc", "");
    expect_invalid(NX_CPIO_E_NO_TRAILER);

    blen = 0;
    add_raw("070701", 0100644, "a", 1, "", 0, -1); /* namesize 1: empty name */
    trailer();
    expect_invalid(NX_CPIO_E_NAME);
    blen = 0;
    add_raw("070701", 0100644, "a", 0xFFFFFFFFll, "", 0, -1);
    trailer();
    expect_invalid(NX_CPIO_E_NAME);
    blen = 0;
    add(0100644, "/etc/a", "x"); /* absolute */
    trailer();
    expect_invalid(NX_CPIO_E_NAME);
    blen = 0;
    add_raw("070701", 0100644, "abc", 3, "", 0, -1); /* namesize excludes NUL: no terminator */
    trailer();
    expect_invalid(NX_CPIO_E_NAME);
    blen = 0;
    add_raw("070701", 0100644, "abcd", 5, "", 0, -1);
    buf[110 + 1] = 0; /* embedded NUL */
    trailer();
    expect_invalid(NX_CPIO_E_NAME);

    blen = 0; /* name runs past the end */
    add_raw("070701", 0100644, "abcd", 64, "", 0, -1);
    blen = 110 + 8;
    expect_invalid(NX_CPIO_E_TRUNCATED);

    blen = 0; /* file data past the end */
    add_raw("070701", 0100644, "f", -1, "xy", 2, 0x1000);
    trailer();
    expect_invalid(NX_CPIO_E_BOUNDS);
    blen = 0;
    add_raw("070701", 0100644, "f", -1, "xy", 2, 0xFFFFFFFFll);
    trailer();
    expect_invalid(NX_CPIO_E_BOUNDS);

    blen = 0; /* symbolic link */
    add(0120777, "link", "target");
    trailer();
    expect_invalid(NX_CPIO_E_TYPE);

    /* Random corruption: no crash (UBSan traps), status always in range. */
    uint64_t x = 0x243F6A8885A308D3ull;
    int bad_status = 0, rejected = 0;
    for (int iter = 0; iter < 20000; iter++) {
        valid_archive();
        for (int f = 0; f < 3; f++) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            buf[x % blen] ^= (uint8_t)(1u << (x >> 61));
        }
        int st = validate(&n, &bad);
        if (st != NX_CPIO_OK && (st < NX_CPIO_E_TRUNCATED || st >= NX_CPIO_E__COUNT))
            bad_status++;
        if (st != NX_CPIO_OK)
            rejected++;
        nx_cpio_find(buf, blen, "etc/a", &e);
    }
    CHECK_EQ_INT(bad_status, 0);
    CHECK(rejected > 0);
    printf("initramfs mutations: %d rejected of 20000\n", rejected);

    /* The archive produced by the build (tools/image/mkinitrd.py). */
    if (initrd_path) {
        FILE *f = fopen(initrd_path, "rb");
        CHECK(f != NULL);
        if (f) {
            static uint8_t img[1 << 20];
            size_t len = fread(img, 1, sizeof(img), f);
            fclose(f);
            CHECK_EQ_INT(nx_cpio_validate(img, len, &n, &bad), NX_CPIO_OK);
            CHECK_EQ_INT(n, 10); /* etc, etc/nanox, release, bin and 6 programs */
            CHECK_EQ_INT(nx_cpio_find(img, len, "etc/nanox/release", &e), NX_CPIO_OK);
            CHECK(e.size == 22 && memcmp(e.data, "NANOX-OS initramfs M3\n", 22) == 0);
            /* The M2 and M3 user programs pass the kernel's loader rules. */
            static const char *const progs[] = {"bin/hello",    "bin/spin", "bin/ipc-send",
                                                "bin/ipc-recv", "bin/load", "bin/core"};
            for (unsigned i = 0; i < sizeof(progs) / sizeof(progs[0]); i++) {
                struct nx_elf_plan plan;
                CHECK_EQ_INT(nx_cpio_find(img, len, progs[i], &e), NX_CPIO_OK);
                CHECK_EQ_INT(e.mode & NX_CPIO_MODE_TYPE, NX_CPIO_MODE_REG);
                CHECK_EQ_INT(nx_elf_plan_user(e.data, e.size, &plan), NX_ELF_OK);
                CHECK_EQ_INT(plan.entry, NX_USER_BASE);
                CHECK(plan.segment_count >= 2);
            }
        }
    } else {
        fprintf(stderr, "test_initramfs: no initrd path given, real-archive check skipped\n");
    }
}
