/*
 * Boot info validator.  Freestanding, no global state; compiled into the
 * kernel and into tests/host.  Physical memory is accessed only through
 * `map`, so host tests can supply a fake physical arena.
 */
#ifndef NANOX_KERNEL_BOOTINFO_CHECK_H
#define NANOX_KERNEL_BOOTINFO_CHECK_H

#include <stdint.h>

#include <nanox/bootinfo.h>

enum nx_bi_error {
    NX_BI_OK = 0,
    NX_BI_E_NULL,              /* boot info pointer 0 or not mappable */
    NX_BI_E_MAGIC,             /* wrong magic */
    NX_BI_E_VERSION,           /* unsupported version_major */
    NX_BI_E_SIZE,              /* size inconsistent with version */
    NX_BI_E_FLAGS,             /* unknown flag bits for this version */
    NX_BI_E_RESERVED,          /* reserved field not zero */
    NX_BI_E_MMAP_PTR,          /* memory map pointer 0, misaligned or not mappable */
    NX_BI_E_MMAP_ENTRY_SIZE,   /* mmap_entry_size != sizeof(struct nx_mem_region) */
    NX_BI_E_MMAP_COUNT,        /* 0 or > NX_MMAP_MAX_ENTRIES */
    NX_BI_E_MMAP_EMPTY,        /* region with length 0 */
    NX_BI_E_MMAP_ALIGN,        /* base or length not 4 KiB aligned */
    NX_BI_E_MMAP_OVERFLOW,     /* base + length wraps */
    NX_BI_E_MMAP_TYPE,         /* unknown region type */
    NX_BI_E_MMAP_FLAGS,        /* region flags not zero */
    NX_BI_E_MMAP_ORDER,        /* regions not sorted by base */
    NX_BI_E_MMAP_OVERLAP,      /* regions overlap */
    NX_BI_E_MMAP_NO_USABLE,    /* no NX_MEM_USABLE memory */
    NX_BI_E_KERNEL_RANGE,      /* kernel span invalid or not covered by KERNEL_IMAGE */
    NX_BI_E_KERNEL_ENTRY,      /* entry outside kernel span */
    NX_BI_E_KERNEL_SELF,       /* running image (linker symbols) outside kernel span */
    NX_BI_E_STACK_RANGE,       /* stack invalid or not covered by KERNEL_STACK */
    NX_BI_E_BOOTINFO_RANGE,    /* boot info / mmap array not covered by BOOT_INFO */
    NX_BI_E_CMDLINE,           /* cmdline not in BOOT_INFO, not NUL-terminated, bad chars */
    NX_BI_E_RSDP,              /* RSDP flag/field inconsistent, signature or checksum */
    NX_BI_E_FRAMEBUFFER,       /* framebuffer flag/fields inconsistent */
    NX_BI_E__COUNT
};

struct nx_bi_check {
    /* Returns a pointer to `len` readable bytes at physical `phys`, or NULL. */
    const void *(*map)(void *opaque, uint64_t phys, uint64_t len);
    void *opaque;
    uint64_t bi_phys;
    /* Optional [start, end) of the running image; both 0 = not checked. */
    uint64_t image_start, image_end;
};

struct nx_bi_result {
    int error;                           /* enum nx_bi_error */
    uint32_t index;                      /* region index for NX_BI_E_MMAP_* */
    const struct nx_boot_info *bi;       /* valid when error == NX_BI_OK */
    const struct nx_mem_region *regions; /* valid when error == NX_BI_OK */
    uint64_t usable_bytes;
};

int nx_bootinfo_check(const struct nx_bi_check *c, struct nx_bi_result *r);
const char *nx_bi_strerror(int error);

#endif
