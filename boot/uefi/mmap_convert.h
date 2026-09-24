/*
 * Conversion of the UEFI memory map into the NANOX boot-info memory map.
 * Runs after ExitBootServices(): no allocation, output buffer is supplied
 * by the caller.  Pure function; exercised by tests/host.
 */
#ifndef NANOX_BOOT_MMAP_CONVERT_H
#define NANOX_BOOT_MMAP_CONVERT_H

#include <stdint.h>

#include <nanox/bootinfo.h>

enum nx_mmap_error {
    NX_MMAP_OK = 0,
    NX_MMAP_E_DESC_SIZE, /* descriptor size smaller than EFI_MEMORY_DESCRIPTOR */
    NX_MMAP_E_ALIGN,     /* PhysicalStart not 4 KiB aligned */
    NX_MMAP_E_OVERFLOW,  /* PhysicalStart + NumberOfPages * 4 KiB overflows */
    NX_MMAP_E_CAPACITY,  /* output buffer too small */
    NX_MMAP_E_OVERLAP,   /* two firmware descriptors overlap */
    NX_MMAP_E_EMPTY,     /* no descriptors */
};

/* Maps a UEFI memory type (including NANOX OS-reserved types) to nx_mem_type. */
uint32_t nx_mmap_translate_type(uint32_t efi_type);

int nx_mmap_from_uefi(const uint8_t *map, uint64_t map_size, uint64_t desc_size,
                      struct nx_mem_region *out, uint32_t cap, uint32_t *out_count);
const char *nx_mmap_strerror(int err);

#endif
