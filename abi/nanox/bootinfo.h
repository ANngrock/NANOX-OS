/*
 * NANOX boot info ABI, version 1.0.
 *
 * Passed by the UEFI loader (boot/uefi) to the kernel entry point after
 * ExitBootServices().  The byte layout is normative and documented in
 * docs/boot-info.md; the _Static_assert block at the end of this file keeps
 * the header and the document in sync.  Only fixed-width types are used so
 * the layout is identical for the loader (PE/COFF, LLP64) and the kernel
 * (ELF64, LP64).
 *
 * Versioning rules (see docs/boot-info.md, "Правила версий"):
 *  - version_major changes on any incompatible change; the kernel rejects a
 *    major it does not know.
 *  - version_minor changes only when fields or flag bits are appended at the
 *    end; existing offsets and meanings never change within a major.
 *  - `size` is the number of bytes of struct nx_boot_info written by the
 *    loader.  It must be >= NX_BOOTINFO_V1_SIZE; when version_minor equals
 *    the kernel's minor it must be exactly sizeof(struct nx_boot_info).
 */
#ifndef NANOX_ABI_BOOTINFO_H
#define NANOX_ABI_BOOTINFO_H

#include <stddef.h>
#include <stdint.h>

/* "NANOX_BI" in little-endian byte order. */
#define NX_BOOTINFO_MAGIC 0x49425F584F4E414EULL
#define NX_BOOTINFO_VERSION_MAJOR 1u
#define NX_BOOTINFO_VERSION_MINOR 0u
#define NX_BOOTINFO_V1_SIZE 192u
/* Upper bound accepted by the validator for `size` (future minors). */
#define NX_BOOTINFO_MAX_SIZE 4096u

/* flags */
#define NX_BI_HAS_ACPI_RSDP (1ull << 0)
#define NX_BI_HAS_FRAMEBUFFER (1ull << 1)
#define NX_BI_KNOWN_FLAGS_V1_0 (NX_BI_HAS_ACPI_RSDP | NX_BI_HAS_FRAMEBUFFER)

/* Memory region types (nx_mem_region.type). */
enum nx_mem_type {
    NX_MEM_USABLE = 1,           /* free RAM */
    NX_MEM_RESERVED = 2,         /* reserved, unusable, unknown, persistent */
    NX_MEM_ACPI_RECLAIMABLE = 3, /* ACPI tables; reusable after parsing */
    NX_MEM_ACPI_NVS = 4,         /* ACPI NVS; must be preserved */
    NX_MEM_MMIO = 5,             /* firmware-reported MMIO / port space */
    NX_MEM_BOOT_RECLAIMABLE = 6, /* UEFI boot services + loader code/data */
    NX_MEM_KERNEL_IMAGE = 7,     /* loaded kernel segments (whole span) */
    NX_MEM_KERNEL_STACK = 8,     /* initial kernel stack */
    NX_MEM_BOOT_INFO = 9,        /* this struct, cmdline, memory map */
    NX_MEM_FIRMWARE_RUNTIME = 10 /* UEFI runtime services code/data */
};
#define NX_MEM_TYPE_MIN 1u
#define NX_MEM_TYPE_MAX 10u

#define NX_PAGE_SIZE 4096u
/* Validator limit for mmap_count. */
#define NX_MMAP_MAX_ENTRIES 4096u
/* Maximum command line length in bytes, excluding the terminating NUL. */
#define NX_CMDLINE_MAX 4095u

/* One physical memory region.  Regions are sorted by base, page aligned,
 * non-empty and non-overlapping; adjacent regions of the same type are
 * coalesced by the loader. */
struct nx_mem_region {
    uint64_t base;   /* 0  physical base, 4 KiB aligned */
    uint64_t length; /* 8  bytes, multiple of 4 KiB, > 0 */
    uint32_t type;   /* 16 enum nx_mem_type */
    uint32_t flags;  /* 20 reserved, must be 0 in v1.0 */
};

/* Framebuffer pixel formats (fb_format). */
enum nx_fb_format {
    NX_FB_NONE = 0,
    NX_FB_RGBX8888 = 1, /* byte 0 = R, 1 = G, 2 = B, 3 = reserved */
    NX_FB_BGRX8888 = 2  /* byte 0 = B, 1 = G, 2 = R, 3 = reserved */
};

struct nx_boot_info {
    uint64_t magic;         /*   0 NX_BOOTINFO_MAGIC */
    uint16_t version_major; /*   8 */
    uint16_t version_minor; /*  10 */
    uint32_t size;          /*  12 bytes written by the loader */
    uint64_t flags;         /*  16 NX_BI_* */

    uint64_t mmap_phys;       /*  24 physical address of nx_mem_region[] */
    uint32_t mmap_count;      /*  32 number of entries */
    uint32_t mmap_entry_size; /*  36 sizeof(struct nx_mem_region) == 24 */

    uint64_t kernel_phys_base; /*  40 page-aligned start of the kernel span */
    uint64_t kernel_phys_size; /*  48 page-aligned size of the kernel span */
    uint64_t kernel_entry;     /*  56 ELF e_entry (identity mapped) */

    uint64_t stack_phys_base; /*  64 initial stack, page aligned */
    uint64_t stack_size;      /*  72 bytes; entry RSP = base + size */

    uint64_t cmdline_phys; /*  80 NUL-terminated printable ASCII */
    uint32_t cmdline_len;  /*  88 bytes, excluding NUL */
    uint32_t reserved0;    /*  92 must be 0 */

    uint64_t initrd_phys; /*  96 reserved for M1, must be 0 in v1.0 */
    uint64_t initrd_size; /* 104 reserved for M1, must be 0 in v1.0 */

    uint64_t acpi_rsdp_phys; /* 112 valid iff NX_BI_HAS_ACPI_RSDP, else 0 */

    uint64_t fb_phys;   /* 120 valid iff NX_BI_HAS_FRAMEBUFFER, else 0 */
    uint64_t fb_size;   /* 128 bytes */
    uint32_t fb_width;  /* 136 pixels */
    uint32_t fb_height; /* 140 pixels */
    uint32_t fb_pitch;  /* 144 bytes per scanline */
    uint32_t fb_format; /* 148 enum nx_fb_format */

    uint64_t uefi_system_table_phys; /* 152 informational; boot services are gone */

    uint8_t kernel_sha256[32]; /* 160 SHA-256 of KERNEL.ELF verified by the loader */
};                             /* 192 */

#define NX_BI_ASSERT_OFFSET(field, off)                                       \
    _Static_assert(offsetof(struct nx_boot_info, field) == (off),           \
                   "nx_boot_info." #field " offset")

NX_BI_ASSERT_OFFSET(magic, 0);
NX_BI_ASSERT_OFFSET(version_major, 8);
NX_BI_ASSERT_OFFSET(version_minor, 10);
NX_BI_ASSERT_OFFSET(size, 12);
NX_BI_ASSERT_OFFSET(flags, 16);
NX_BI_ASSERT_OFFSET(mmap_phys, 24);
NX_BI_ASSERT_OFFSET(mmap_count, 32);
NX_BI_ASSERT_OFFSET(mmap_entry_size, 36);
NX_BI_ASSERT_OFFSET(kernel_phys_base, 40);
NX_BI_ASSERT_OFFSET(kernel_phys_size, 48);
NX_BI_ASSERT_OFFSET(kernel_entry, 56);
NX_BI_ASSERT_OFFSET(stack_phys_base, 64);
NX_BI_ASSERT_OFFSET(stack_size, 72);
NX_BI_ASSERT_OFFSET(cmdline_phys, 80);
NX_BI_ASSERT_OFFSET(cmdline_len, 88);
NX_BI_ASSERT_OFFSET(reserved0, 92);
NX_BI_ASSERT_OFFSET(initrd_phys, 96);
NX_BI_ASSERT_OFFSET(initrd_size, 104);
NX_BI_ASSERT_OFFSET(acpi_rsdp_phys, 112);
NX_BI_ASSERT_OFFSET(fb_phys, 120);
NX_BI_ASSERT_OFFSET(fb_size, 128);
NX_BI_ASSERT_OFFSET(fb_width, 136);
NX_BI_ASSERT_OFFSET(fb_height, 140);
NX_BI_ASSERT_OFFSET(fb_pitch, 144);
NX_BI_ASSERT_OFFSET(fb_format, 148);
NX_BI_ASSERT_OFFSET(uefi_system_table_phys, 152);
NX_BI_ASSERT_OFFSET(kernel_sha256, 160);
_Static_assert(sizeof(struct nx_boot_info) == NX_BOOTINFO_V1_SIZE, "nx_boot_info size");

_Static_assert(offsetof(struct nx_mem_region, base) == 0, "nx_mem_region.base");
_Static_assert(offsetof(struct nx_mem_region, length) == 8, "nx_mem_region.length");
_Static_assert(offsetof(struct nx_mem_region, type) == 16, "nx_mem_region.type");
_Static_assert(offsetof(struct nx_mem_region, flags) == 20, "nx_mem_region.flags");
_Static_assert(sizeof(struct nx_mem_region) == 24, "nx_mem_region size");

#endif /* NANOX_ABI_BOOTINFO_H */
