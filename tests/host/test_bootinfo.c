/*
 * Host tests for the kernel boot-info validator (kernel/bootinfo_check.c).
 *
 * A fake physical arena at ARENA_PHYS holds the boot info, command line,
 * memory map and an ACPI RSDP; the validator reaches it only through
 * arena_map(), exactly as the kernel reaches physical memory through its
 * identity mapping.
 */
#include <stdint.h>
#include <string.h>

#include <nanox/bootinfo.h>

#include "bootinfo_check.h"
#include "efi.h"
#include "mmap_convert.h"
#include "test.h"

#define ARENA_PHYS 0x100000ull
#define ARENA_SIZE 0x10000ull
#define BI_PHYS 0x100000ull
#define CMDLINE_PHYS 0x100100ull
#define MMAP_PHYS 0x101000ull
#define RSDP_PHYS 0x103000ull

static _Alignas(4096) uint8_t arena[ARENA_SIZE];

static const void *arena_map(void *opaque, uint64_t phys, uint64_t len)
{
    (void)opaque;
    if (phys < ARENA_PHYS || len > ARENA_SIZE || phys - ARENA_PHYS > ARENA_SIZE - len)
        return NULL;
    return arena + (phys - ARENA_PHYS);
}

static void *at(uint64_t phys)
{
    return arena + (phys - ARENA_PHYS);
}

static struct nx_boot_info *BI;
static struct nx_mem_region *RG;
static struct nx_bi_check CHK;

static const struct nx_mem_region FIXTURE_MAP[] = {
    {0x00000000, 0x000A0000, NX_MEM_USABLE, 0},
    {0x00100000, 0x00003000, NX_MEM_BOOT_INFO, 0},
    {0x00103000, 0x00001000, NX_MEM_ACPI_RECLAIMABLE, 0},
    {0x00104000, 0x000FC000, NX_MEM_USABLE, 0},
    {0x00200000, 0x00004000, NX_MEM_KERNEL_IMAGE, 0},
    {0x00204000, 0x00010000, NX_MEM_KERNEL_STACK, 0},
    {0x00214000, 0x07DEC000, NX_MEM_USABLE, 0},
    {0xE0000000, 0x10000000, NX_MEM_RESERVED, 0},
    {0xFFC00000, 0x00400000, NX_MEM_MMIO, 0},
    {0x100000000ull, 0x00001000, NX_MEM_INITRD, 0},
};
#define FIXTURE_COUNT (sizeof(FIXTURE_MAP) / sizeof(FIXTURE_MAP[0]))

static void fix_checksum(uint8_t *p, uint32_t len, uint32_t csum_off)
{
    p[csum_off] = 0;
    uint8_t sum = 0;
    for (uint32_t i = 0; i < len; i++)
        sum = (uint8_t)(sum + p[i]);
    p[csum_off] = (uint8_t)(0x100 - sum);
}

static void write_rsdp(int revision)
{
    uint8_t *r = at(RSDP_PHYS);
    memset(r, 0, 36);
    memcpy(r, "RSD PTR ", 8);
    memcpy(r + 9, "NANOX ", 6);
    r[15] = (uint8_t)revision;
    uint32_t rsdt = 0x0F000000, length = 36;
    memcpy(r + 16, &rsdt, 4);
    if (revision >= 2) {
        memcpy(r + 20, &length, 4);
        fix_checksum(r, 20, 8);
        fix_checksum(r, 36, 32);
    } else {
        fix_checksum(r, 20, 8);
    }
}

static void make_valid(void)
{
    memset(arena, 0, sizeof(arena));
    BI = at(BI_PHYS);
    RG = at(MMAP_PHYS);
    memcpy(RG, FIXTURE_MAP, sizeof(FIXTURE_MAP));
    static const char cmdline[] = "nanox.test=pass";
    memcpy(at(CMDLINE_PHYS), cmdline, sizeof(cmdline));
    write_rsdp(2);

    BI->magic = NX_BOOTINFO_MAGIC;
    BI->version_major = NX_BOOTINFO_VERSION_MAJOR;
    BI->version_minor = NX_BOOTINFO_VERSION_MINOR;
    BI->size = sizeof(*BI);
    BI->flags = NX_BI_HAS_ACPI_RSDP;
    BI->mmap_phys = MMAP_PHYS;
    BI->mmap_count = FIXTURE_COUNT;
    BI->mmap_entry_size = sizeof(struct nx_mem_region);
    BI->kernel_phys_base = 0x200000;
    BI->kernel_phys_size = 0x4000;
    BI->kernel_entry = 0x200000;
    BI->stack_phys_base = 0x204000;
    BI->stack_size = 0x10000;
    BI->cmdline_phys = CMDLINE_PHYS;
    BI->cmdline_len = sizeof(cmdline) - 1;
    BI->acpi_rsdp_phys = RSDP_PHYS;
    BI->flags |= NX_BI_HAS_INITRD;
    BI->initrd_phys = 0x100000000ull;
    BI->initrd_size = 0x800;
    memset(BI->initrd_sha256, 0xA5, sizeof(BI->initrd_sha256));

    CHK.map = arena_map;
    CHK.opaque = NULL;
    CHK.bi_phys = BI_PHYS;
    CHK.image_start = 0x200000;
    CHK.image_end = 0x203000;
}

static int run(struct nx_bi_result *r)
{
    return nx_bootinfo_check(&CHK, r);
}

#define EXPECT(err)                                                                                \
    do {                                                                                           \
        struct nx_bi_result r_;                                                                    \
        int e_ = run(&r_);                                                                         \
        CHECK_EQ_INT(e_, err);                                                                     \
        if (e_ != (err))                                                                           \
            fprintf(stderr, "  got %s, expected %s\n", nx_bi_strerror(e_), nx_bi_strerror(err));   \
    } while (0)

static void set_fb(void)
{
    BI->flags |= NX_BI_HAS_FRAMEBUFFER;
    BI->fb_phys = 0x80000000;
    BI->fb_width = 1280;
    BI->fb_height = 800;
    BI->fb_pitch = 1280 * 4;
    BI->fb_size = 1280ull * 4 * 800;
    BI->fb_format = NX_FB_BGRX8888;
}

static void test_valid(void)
{
    struct nx_bi_result r;
    make_valid();
    CHECK_EQ_INT(run(&r), NX_BI_OK);
    CHECK(r.bi == at(BI_PHYS));
    CHECK(r.regions == at(MMAP_PHYS));
    CHECK_EQ_INT(r.usable_bytes, 0xA0000 + 0xFC000 + 0x7DEC000);

    make_valid(); /* running-image check disabled */
    CHK.image_start = CHK.image_end = 0;
    EXPECT(NX_BI_OK);

    make_valid(); /* ACPI 1.0 RSDP (20 bytes) */
    write_rsdp(0);
    EXPECT(NX_BI_OK);

    make_valid(); /* no RSDP at all */
    BI->flags &= ~NX_BI_HAS_ACPI_RSDP;
    BI->acpi_rsdp_phys = 0;
    EXPECT(NX_BI_OK);

    make_valid(); /* framebuffer present, outside RAM */
    set_fb();
    EXPECT(NX_BI_OK);
    BI->fb_format = NX_FB_RGBX8888;
    EXPECT(NX_BI_OK);

    make_valid(); /* empty command line */
    BI->cmdline_len = 0;
    *(char *)at(CMDLINE_PHYS) = 0;
    EXPECT(NX_BI_OK);

    make_valid(); /* newer minor: larger struct, unknown flags and types are tolerated */
    BI->version_minor = NX_BOOTINFO_VERSION_MINOR + 1;
    BI->size = 256;
    BI->flags |= 1ull << 40;
    RG[7].type = 40;
    EXPECT(NX_BI_OK);

    make_valid(); /* a 1.0 loader: 192-byte struct, no initramfs, no type 11 */
    BI->version_minor = 0;
    BI->size = NX_BOOTINFO_V1_SIZE;
    BI->flags &= ~NX_BI_HAS_INITRD;
    BI->initrd_phys = 0;
    BI->initrd_size = 0;
    BI->mmap_count = FIXTURE_COUNT - 1;
    EXPECT(NX_BI_OK);

    make_valid(); /* initramfs size exactly one page */
    BI->initrd_size = 0x1000;
    EXPECT(NX_BI_OK);

    make_valid(); /* 1.1 without initramfs */
    BI->flags &= ~NX_BI_HAS_INITRD;
    BI->initrd_phys = 0;
    BI->initrd_size = 0;
    memset(BI->initrd_sha256, 0, sizeof(BI->initrd_sha256));
    EXPECT(NX_BI_OK);
}

static void test_header(void)
{
    make_valid();
    CHK.bi_phys = 0;
    EXPECT(NX_BI_E_NULL);
    make_valid();
    CHK.bi_phys = 0x900000; /* not mappable */
    EXPECT(NX_BI_E_NULL);
    make_valid();
    CHK.bi_phys = UINT64_MAX - 8;
    EXPECT(NX_BI_E_NULL);
    make_valid();
    BI->magic ^= 1;
    EXPECT(NX_BI_E_MAGIC);
    make_valid();
    BI->version_major = 2;
    EXPECT(NX_BI_E_VERSION);
    make_valid();
    BI->version_major = 0;
    EXPECT(NX_BI_E_VERSION);
    make_valid();
    BI->size = 100;
    EXPECT(NX_BI_E_SIZE);
    make_valid();
    BI->size = 256; /* same minor must have the exact size */
    EXPECT(NX_BI_E_SIZE);
    make_valid();
    BI->version_minor = 1;
    BI->size = NX_BOOTINFO_MAX_SIZE + 8;
    EXPECT(NX_BI_E_SIZE);
    make_valid();
    BI->flags |= 1ull << 5;
    EXPECT(NX_BI_E_FLAGS);
    make_valid();
    BI->reserved0 = 1;
    EXPECT(NX_BI_E_RESERVED);
    make_valid(); /* 1.0 must have a 192-byte struct */
    BI->version_minor = 0;
    EXPECT(NX_BI_E_SIZE);
    make_valid(); /* 1.0 does not know NX_BI_HAS_INITRD */
    BI->version_minor = 0;
    BI->size = NX_BOOTINFO_V1_SIZE;
    EXPECT(NX_BI_E_FLAGS);
    make_valid(); /* 1.0: initrd fields are reserved */
    BI->version_minor = 0;
    BI->size = NX_BOOTINFO_V1_SIZE;
    BI->flags &= ~NX_BI_HAS_INITRD;
    EXPECT(NX_BI_E_RESERVED);
    make_valid(); /* 1.0 does not know region type 11 */
    BI->version_minor = 0;
    BI->size = NX_BOOTINFO_V1_SIZE;
    BI->flags &= ~NX_BI_HAS_INITRD;
    BI->initrd_phys = 0;
    BI->initrd_size = 0;
    EXPECT(NX_BI_E_MMAP_TYPE);
}

static void test_initrd(void)
{
    make_valid(); /* fields without the flag */
    BI->flags &= ~NX_BI_HAS_INITRD;
    EXPECT(NX_BI_E_INITRD);
    make_valid(); /* hash without the flag */
    BI->flags &= ~NX_BI_HAS_INITRD;
    BI->initrd_phys = 0;
    BI->initrd_size = 0;
    EXPECT(NX_BI_E_INITRD);
    make_valid();
    BI->initrd_phys += 0x10;
    EXPECT(NX_BI_E_INITRD);
    make_valid();
    BI->initrd_phys = 0;
    EXPECT(NX_BI_E_INITRD);
    make_valid();
    BI->initrd_size = 0;
    EXPECT(NX_BI_E_INITRD);
    make_valid(); /* larger than its region */
    BI->initrd_size = 0x1001;
    EXPECT(NX_BI_E_INITRD);
    make_valid();
    BI->initrd_size = UINT64_MAX;
    EXPECT(NX_BI_E_INITRD);
    make_valid(); /* not reserved as initramfs */
    RG[9].type = NX_MEM_RESERVED;
    EXPECT(NX_BI_E_INITRD);
}

static void test_mmap_rules(void)
{
    make_valid();
    BI->mmap_entry_size = 16;
    EXPECT(NX_BI_E_MMAP_ENTRY_SIZE);
    make_valid();
    BI->mmap_count = 0;
    EXPECT(NX_BI_E_MMAP_COUNT);
    make_valid();
    BI->mmap_count = NX_MMAP_MAX_ENTRIES + 1;
    EXPECT(NX_BI_E_MMAP_COUNT);
    make_valid();
    BI->mmap_count = 4000; /* allowed count, array runs past mappable memory */
    EXPECT(NX_BI_E_MMAP_PTR);
    make_valid();
    BI->mmap_phys = 0;
    EXPECT(NX_BI_E_MMAP_PTR);
    make_valid();
    BI->mmap_phys = MMAP_PHYS + 4;
    EXPECT(NX_BI_E_MMAP_PTR);
    make_valid();
    BI->mmap_phys = 0x900000;
    EXPECT(NX_BI_E_MMAP_PTR);
    make_valid();
    BI->mmap_phys = UINT64_MAX - 7;
    EXPECT(NX_BI_E_MMAP_PTR);

    make_valid();
    RG[3].length = 0;
    EXPECT(NX_BI_E_MMAP_EMPTY);
    make_valid();
    RG[6].base += 0x800;
    EXPECT(NX_BI_E_MMAP_ALIGN);
    make_valid();
    RG[6].length += 0x10;
    EXPECT(NX_BI_E_MMAP_ALIGN);
    make_valid();
    RG[8].base = 0xFFFFFFFFFFFFF000ull;
    RG[8].length = 0x2000;
    EXPECT(NX_BI_E_MMAP_OVERFLOW);
    make_valid();
    RG[8].base = 0xFFFFFFFFFFFFF000ull; /* end == 2^64 is also rejected */
    RG[8].length = 0x1000;
    EXPECT(NX_BI_E_MMAP_OVERFLOW);
    make_valid();
    RG[7].type = 0;
    EXPECT(NX_BI_E_MMAP_TYPE);
    make_valid();
    RG[7].type = NX_MEM_TYPE_MAX + 1;
    EXPECT(NX_BI_E_MMAP_TYPE);
    make_valid();
    RG[7].flags = 1;
    EXPECT(NX_BI_E_MMAP_FLAGS);

    make_valid(); /* swapped entries */
    struct nx_mem_region tmp = RG[6];
    RG[6] = RG[7];
    RG[7] = tmp;
    EXPECT(NX_BI_E_MMAP_ORDER);
    make_valid(); /* same base as previous */
    RG[3].base = 0x103000;
    EXPECT(NX_BI_E_MMAP_OVERLAP);
    make_valid(); /* starts inside previous */
    RG[7].base = 0x08000000 - 0x1000;
    EXPECT(NX_BI_E_MMAP_OVERLAP);
    make_valid(); /* large region swallowing the next ones */
    RG[0].length = 0x00200000;
    EXPECT(NX_BI_E_MMAP_OVERLAP);

    make_valid();
    for (uint32_t i = 0; i < FIXTURE_COUNT; i++)
        if (RG[i].type == NX_MEM_USABLE)
            RG[i].type = NX_MEM_RESERVED;
    EXPECT(NX_BI_E_MMAP_NO_USABLE);
}

static void test_ranges(void)
{
    make_valid();
    BI->kernel_phys_size = 0x5000; /* runs into the stack region */
    EXPECT(NX_BI_E_KERNEL_RANGE);
    make_valid();
    BI->kernel_phys_base = 0x200800;
    EXPECT(NX_BI_E_KERNEL_RANGE);
    make_valid();
    BI->kernel_phys_size = 0;
    EXPECT(NX_BI_E_KERNEL_RANGE);
    make_valid();
    RG[4].type = NX_MEM_USABLE; /* kernel not reserved */
    EXPECT(NX_BI_E_KERNEL_RANGE);
    make_valid();
    BI->kernel_phys_base = UINT64_MAX & ~0xFFFull;
    EXPECT(NX_BI_E_KERNEL_RANGE);
    make_valid();
    BI->kernel_entry = 0x204000;
    EXPECT(NX_BI_E_KERNEL_ENTRY);
    make_valid();
    BI->kernel_entry = 0x1FFFFF;
    EXPECT(NX_BI_E_KERNEL_ENTRY);
    make_valid();
    CHK.image_end = 0x205000;
    EXPECT(NX_BI_E_KERNEL_SELF);
    make_valid();
    CHK.image_start = 0x100000;
    EXPECT(NX_BI_E_KERNEL_SELF);

    make_valid();
    BI->stack_size = 0x20000;
    EXPECT(NX_BI_E_STACK_RANGE);
    make_valid();
    BI->stack_phys_base = 0x300000; /* usable memory, not reserved as stack */
    EXPECT(NX_BI_E_STACK_RANGE);
    make_valid();
    BI->stack_size = 0;
    EXPECT(NX_BI_E_STACK_RANGE);
    make_valid();
    BI->stack_phys_base = 0x204010;
    EXPECT(NX_BI_E_STACK_RANGE);

    make_valid();
    RG[1].type = NX_MEM_BOOT_RECLAIMABLE; /* boot info not reserved */
    EXPECT(NX_BI_E_BOOTINFO_RANGE);
}

static void test_cmdline(void)
{
    make_valid();
    ((char *)at(CMDLINE_PHYS))[BI->cmdline_len] = 'x';
    EXPECT(NX_BI_E_CMDLINE);
    make_valid();
    ((char *)at(CMDLINE_PHYS))[3] = '\n';
    EXPECT(NX_BI_E_CMDLINE);
    make_valid();
    ((char *)at(CMDLINE_PHYS))[3] = (char)0xC3;
    EXPECT(NX_BI_E_CMDLINE);
    make_valid();
    BI->cmdline_len = NX_CMDLINE_MAX + 1;
    EXPECT(NX_BI_E_CMDLINE);
    make_valid();
    BI->cmdline_phys = RSDP_PHYS; /* ACPI region, not boot info */
    EXPECT(NX_BI_E_CMDLINE);
    make_valid();
    BI->cmdline_phys = 0;
    EXPECT(NX_BI_E_CMDLINE);
}

static void test_optional(void)
{
    make_valid();
    ((uint8_t *)at(RSDP_PHYS))[0] = 'X';
    EXPECT(NX_BI_E_RSDP);
    make_valid();
    ((uint8_t *)at(RSDP_PHYS))[8] ^= 1; /* v1 checksum */
    EXPECT(NX_BI_E_RSDP);
    make_valid();
    ((uint8_t *)at(RSDP_PHYS))[32] ^= 1; /* extended checksum */
    EXPECT(NX_BI_E_RSDP);
    make_valid();
    BI->flags &= ~NX_BI_HAS_ACPI_RSDP; /* pointer without flag */
    EXPECT(NX_BI_E_RSDP);
    make_valid();
    BI->acpi_rsdp_phys = 0x300000; /* in usable RAM */
    EXPECT(NX_BI_E_RSDP);
    make_valid();
    BI->acpi_rsdp_phys = 0x0A0000; /* hole, not mappable */
    EXPECT(NX_BI_E_RSDP);

    make_valid();
    set_fb();
    BI->fb_width = 0;
    EXPECT(NX_BI_E_FRAMEBUFFER);
    make_valid();
    set_fb();
    BI->fb_pitch = 1280 * 4 - 4;
    EXPECT(NX_BI_E_FRAMEBUFFER);
    make_valid();
    set_fb();
    BI->fb_size = 4096;
    EXPECT(NX_BI_E_FRAMEBUFFER);
    make_valid();
    set_fb();
    BI->fb_format = 7;
    EXPECT(NX_BI_E_FRAMEBUFFER);
    make_valid();
    set_fb();
    BI->fb_phys = 0x300000; /* overlaps usable RAM */
    EXPECT(NX_BI_E_FRAMEBUFFER);
    make_valid();
    set_fb();
    BI->fb_phys = UINT64_MAX - 0x1000;
    EXPECT(NX_BI_E_FRAMEBUFFER);
    make_valid();
    BI->fb_width = 1024; /* field set without flag */
    EXPECT(NX_BI_E_FRAMEBUFFER);
}

/* The loader's conversion output must satisfy the kernel's validator. */
static void test_loader_conversion_accepted(void)
{
    struct {
        EFI_MEMORY_DESCRIPTOR d;
        uint64_t pad;
    } raw[12];
    static const struct {
        uint32_t type;
        uint64_t base, pages;
    } src[] = {
        {NX_EFI_TYPE_KERNEL_STACK, 0x204000, 16},  {EfiConventionalMemory, 0x104000, 0xFC},
        {NX_EFI_TYPE_BOOT_INFO, 0x100000, 3},      {EfiConventionalMemory, 0x0, 0xA0},
        {EfiACPIReclaimMemory, 0x103000, 1},       {NX_EFI_TYPE_KERNEL_IMAGE, 0x200000, 4},
        {EfiConventionalMemory, 0x214000, 0x3000}, {EfiConventionalMemory, 0x3214000, 0x4DEC},
        {EfiMemoryMappedIO, 0xFFC00000, 0x400},    {EfiReservedMemoryType, 0xE0000000, 0x10000},
        {NX_EFI_TYPE_INITRD, 0x100000000ull, 1},
    };
    const int n = sizeof(src) / sizeof(src[0]);
    memset(raw, 0, sizeof(raw));
    for (int i = 0; i < n; i++) {
        raw[i].d.Type = src[i].type;
        raw[i].d.PhysicalStart = src[i].base;
        raw[i].d.NumberOfPages = src[i].pages;
    }
    make_valid();
    uint32_t count = 0;
    CHECK_EQ_INT(nx_mmap_from_uefi((const uint8_t *)raw, (uint64_t)n * sizeof(raw[0]),
                                   sizeof(raw[0]), RG, 64, &count),
                 NX_MMAP_OK);
    CHECK_EQ_INT(count, FIXTURE_COUNT); /* the two adjacent usable ranges coalesce */
    BI->mmap_count = count;
    CHECK(memcmp(RG, FIXTURE_MAP, sizeof(FIXTURE_MAP)) == 0);
    EXPECT(NX_BI_OK);
}

/* Deterministic random corruption: the validator must never crash (UBSan traps
 * on undefined behaviour) and must reject every mutation of the magic. */
static void test_mutations(void)
{
    uint64_t x = 0x9E3779B97F4A7C15ull;
    int ok = 0, rejected = 0, bad = 0;
    for (int iter = 0; iter < 20000; iter++) {
        make_valid();
        int flips = 1 + (int)(x % 4);
        for (int f = 0; f < flips; f++) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            /* boot info + command line + memory map + RSDP */
            uint64_t off = x % (RSDP_PHYS + 36 - ARENA_PHYS);
            arena[off] ^= (uint8_t)(1u << (x >> 60 & 7));
        }
        struct nx_bi_result r;
        int e = run(&r);
        if (e < NX_BI_OK || e >= NX_BI_E__COUNT)
            bad++;
        if (e == NX_BI_OK) {
            ok++;
            if (BI->magic != NX_BOOTINFO_MAGIC)
                bad++;
        } else {
            rejected++;
        }
    }
    CHECK_EQ_INT(bad, 0);
    CHECK(rejected > 0);
    printf("bootinfo mutations: %d accepted (benign bytes), %d rejected\n", ok, rejected);
}

void test_bootinfo(void)
{
    test_valid();
    test_header();
    test_initrd();
    test_mmap_rules();
    test_ranges();
    test_cmdline();
    test_optional();
    test_loader_conversion_accepted();
    test_mutations();
    CHECK(strcmp(nx_bi_strerror(NX_BI_E_MMAP_OVERLAP), "E_MMAP_OVERLAP") == 0);
    CHECK(strcmp(nx_bi_strerror(-1), "E_UNKNOWN") == 0);
}
