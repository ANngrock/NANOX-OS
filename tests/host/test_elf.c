#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/elf_plan.h>
#include <nanox/syscall.h>

#include "test.h"

struct seg {
    uint32_t type, flags;
    uint64_t off, vaddr, paddr, filesz, memsz;
};

static uint8_t img[0x4000];

static void w16(uint8_t *p, uint16_t v)
{
    memcpy(p, &v, 2);
}
static void w32(uint8_t *p, uint32_t v)
{
    memcpy(p, &v, 4);
}
static void w64(uint8_t *p, uint64_t v)
{
    memcpy(p, &v, 8);
}

/* Little-endian host assumed (x86-64). */
static void build(uint64_t entry, const struct seg *s, int n)
{
    memset(img, 0, sizeof(img));
    memcpy(img,
           "\x7f"
           "ELF",
           4);
    img[4] = 2;
    img[5] = 1;
    img[6] = 1;
    w16(img + 16, 2);
    w16(img + 18, 62);
    w32(img + 20, 1);
    w64(img + 24, entry);
    w64(img + 32, 64);
    w16(img + 52, 64);
    w16(img + 54, 56);
    w16(img + 56, (uint16_t)n);
    for (int i = 0; i < n; i++) {
        uint8_t *ph = img + 64 + 56 * i;
        w32(ph, s[i].type);
        w32(ph + 4, s[i].flags);
        w64(ph + 8, s[i].off);
        w64(ph + 16, s[i].vaddr);
        w64(ph + 24, s[i].paddr);
        w64(ph + 32, s[i].filesz);
        w64(ph + 40, s[i].memsz);
        w64(ph + 48, 0x1000);
    }
}

static const struct seg TEXT = {1, 5, 0x1000, 0x200000, 0x200000, 0x100, 0x100};
static const struct seg DATA = {1, 6, 0x2000, 0x201000, 0x201000, 0x80, 0x2000};

static int plan_of(uint64_t entry, const struct seg *s, int n, uint64_t size, struct nx_elf_plan *p)
{
    build(entry, s, n);
    return nx_elf_plan(img, size, p);
}

static void expect_err(uint64_t entry, const struct seg *s, int n, int err)
{
    struct nx_elf_plan p;
    CHECK_EQ_INT(plan_of(entry, s, n, sizeof(img), &p), err);
}

void test_elf(const char *kernel_path)
{
    struct nx_elf_plan p;
    struct seg two[2] = {TEXT, DATA};

    CHECK_EQ_INT(plan_of(0x200010, two, 2, sizeof(img), &p), NX_ELF_OK);
    CHECK_EQ_INT(p.segment_count, 2);
    CHECK_EQ_INT(p.span_base, 0x200000);
    CHECK_EQ_INT(p.span_end, 0x203000);
    CHECK_EQ_INT(p.entry, 0x200010);

    /* Header-level rejections. */
    build(0x200000, two, 2);
    CHECK_EQ_INT(nx_elf_plan(img, 10, &p), NX_ELF_E_TRUNCATED);
    build(0x200000, two, 2);
    img[1] = 'X';
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_IDENT);
    build(0x200000, two, 2);
    img[4] = 1; /* ELFCLASS32 */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_IDENT);
    build(0x200000, two, 2);
    img[5] = 2; /* big endian */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_IDENT);
    build(0x200000, two, 2);
    w16(img + 16, 3); /* ET_DYN */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_TYPE);
    build(0x200000, two, 2);
    w16(img + 18, 3); /* EM_386 */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_TYPE);
    build(0x200000, two, 0);
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_PHDR);
    build(0x200000, two, 2);
    w16(img + 54, 32); /* wrong phentsize */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_PHDR);
    build(0x200000, two, 2);
    w64(img + 32, sizeof(img) - 60); /* table runs past the end */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_PHDR);
    build(0x200000, two, 2);
    w64(img + 32, UINT64_MAX - 8); /* offset overflow */
    CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_PHDR);

    /* Segment-level rejections. */
    struct seg s = TEXT;
    s.filesz = s.memsz + 1;
    expect_err(0x200000, &s, 1, NX_ELF_E_SEG_BOUNDS);
    s = TEXT;
    s.off = sizeof(img) - 0x10;
    expect_err(0x200000, &s, 1, NX_ELF_E_SEG_BOUNDS);
    s = TEXT;
    s.off = UINT64_MAX - 0x10;
    expect_err(0x200000, &s, 1, NX_ELF_E_SEG_BOUNDS);
    s = TEXT;
    s.vaddr = 0xFFFFFFFF80200000ull;
    expect_err(0x200000, &s, 1, NX_ELF_E_SEG_ADDR);
    s = TEXT;
    s.vaddr = s.paddr = 0x1000; /* below the load window */
    expect_err(0x1000, &s, 1, NX_ELF_E_SEG_ADDR);
    s = TEXT;
    s.vaddr = s.paddr = UINT64_MAX - 0x10;
    expect_err(UINT64_MAX - 0x10, &s, 1, NX_ELF_E_SEG_ADDR);
    s = TEXT;
    s.memsz = NX_KERNEL_LOAD_MAX; /* crosses the top of the window */
    expect_err(0x200000, &s, 1, NX_ELF_E_SEG_ADDR);

    struct seg overlap[2] = {TEXT, DATA};
    overlap[1].vaddr = overlap[1].paddr = 0x200080;
    expect_err(0x200000, overlap, 2, NX_ELF_E_SEG_ORDER);
    struct seg unsorted[2] = {DATA, TEXT};
    expect_err(0x200000, unsorted, 2, NX_ELF_E_SEG_ORDER);

    expect_err(0x201000, two, 2, NX_ELF_E_ENTRY); /* entry in the RW segment */
    expect_err(0x300000, two, 2, NX_ELF_E_ENTRY);

    s = TEXT;
    s.type = 4; /* PT_NOTE only */
    expect_err(0x200000, &s, 1, NX_ELF_E_NO_LOAD);

    s = TEXT;
    s.memsz = NX_KERNEL_SPAN_MAX + 0x1000;
    expect_err(0x200000, &s, 1, NX_ELF_E_SPAN);

    /* User program rules (M2): p_paddr is ignored, the segments must lie in
     * the user half below the stack, and no segment may be W+X. */
    {
        const uint64_t B = NX_USER_BASE;
        struct seg utext = {1, 5, 0x1000, B, 0, 0x100, 0x100};
        struct seg udata = {1, 6, 0x2000, B + 0x1000, 0x1234, 0x80, 0x3000};
        struct seg u2[2] = {utext, udata};
        build(B + 0x10, u2, 2);
        CHECK_EQ_INT(nx_elf_plan_user(img, sizeof(img), &p), NX_ELF_OK);
        CHECK_EQ_INT(p.span_base, B);
        CHECK_EQ_INT(p.span_end, B + 0x4000);
        CHECK_EQ_INT(p.segments[1].addr, B + 0x1000);
        /* The kernel rules reject the same image (not identity, too high). */
        CHECK_EQ_INT(nx_elf_plan(img, sizeof(img), &p), NX_ELF_E_SEG_ADDR);
        /* W+X segment. */
        u2[1].flags = 7;
        build(B + 0x10, u2, 2);
        CHECK_EQ_INT(nx_elf_plan_user(img, sizeof(img), &p), NX_ELF_E_WX);
        u2[1].flags = 6;
        /* Below the user half (PML4 slot 0 belongs to the kernel image). */
        struct seg low = utext;
        low.vaddr = B - 0x1000;
        build(B - 0x1000, &low, 1);
        CHECK_EQ_INT(nx_elf_plan_user(img, sizeof(img), &p), NX_ELF_E_SEG_ADDR);
        /* Into the stack area or the higher half. */
        struct seg high = utext;
        high.vaddr = NX_USER_STACK_TOP - 0x2000;
        build(high.vaddr, &high, 1);
        CHECK_EQ_INT(nx_elf_plan_user(img, sizeof(img), &p), NX_ELF_E_SEG_ADDR);
        high.vaddr = 0xFFFF800000000000ull;
        build(high.vaddr, &high, 1);
        CHECK_EQ_INT(nx_elf_plan_user(img, sizeof(img), &p), NX_ELF_E_SEG_ADDR);
        /* Span limit. */
        struct seg wide[2] = {utext, udata};
        wide[1].vaddr = B + NX_USER_SPAN_MAX;
        build(B, wide, 2);
        CHECK_EQ_INT(nx_elf_plan_user(img, sizeof(img), &p), NX_ELF_E_SPAN);
    }

    /* The real kernel produced by the build. */
    if (kernel_path) {
        FILE *f = fopen(kernel_path, "rb");
        CHECK(f != NULL);
        if (f) {
            static uint8_t buf[1 << 22];
            size_t n = fread(buf, 1, sizeof(buf), f);
            fclose(f);
            CHECK(n > 0 && n < sizeof(buf));
            CHECK_EQ_INT(nx_elf_plan(buf, n, &p), NX_ELF_OK);
            CHECK_EQ_INT(p.span_base, 0x200000);
            CHECK_EQ_INT(p.entry, 0x200000);
        }
    } else {
        fprintf(stderr, "test_elf: no kernel path given, real-kernel check skipped\n");
    }
}
