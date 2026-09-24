#include "mmap_convert.h"
#include "efi.h"

uint32_t nx_mmap_translate_type(uint32_t t)
{
    switch (t) {
    case EfiConventionalMemory: return NX_MEM_USABLE;
    case EfiLoaderCode:
    case EfiLoaderData:
    case EfiBootServicesCode:
    case EfiBootServicesData: return NX_MEM_BOOT_RECLAIMABLE;
    case EfiRuntimeServicesCode:
    case EfiRuntimeServicesData: return NX_MEM_FIRMWARE_RUNTIME;
    case EfiACPIReclaimMemory: return NX_MEM_ACPI_RECLAIMABLE;
    case EfiACPIMemoryNVS: return NX_MEM_ACPI_NVS;
    case EfiMemoryMappedIO:
    case EfiMemoryMappedIOPortSpace: return NX_MEM_MMIO;
    case NX_EFI_TYPE_KERNEL_IMAGE: return NX_MEM_KERNEL_IMAGE;
    case NX_EFI_TYPE_KERNEL_STACK: return NX_MEM_KERNEL_STACK;
    case NX_EFI_TYPE_BOOT_INFO: return NX_MEM_BOOT_INFO;
    case NX_EFI_TYPE_INITRD: return NX_MEM_INITRD;
    default:
        /* Reserved, unusable, PAL code, persistent, unaccepted, unknown. */
        return NX_MEM_RESERVED;
    }
}

int nx_mmap_from_uefi(const uint8_t *map, uint64_t map_size, uint64_t desc_size,
                      struct nx_mem_region *out, uint32_t cap, uint32_t *out_count)
{
    *out_count = 0;
    if (desc_size < sizeof(EFI_MEMORY_DESCRIPTOR))
        return NX_MMAP_E_DESC_SIZE;

    uint32_t n = 0;
    for (uint64_t off = 0; map_size - off >= desc_size; off += desc_size) {
        const EFI_MEMORY_DESCRIPTOR *d = (const EFI_MEMORY_DESCRIPTOR *)(map + off);
        if (d->NumberOfPages == 0)
            continue;
        if (d->PhysicalStart & (NX_PAGE_SIZE - 1))
            return NX_MMAP_E_ALIGN;
        if (d->NumberOfPages > (UINT64_MAX - d->PhysicalStart) / NX_PAGE_SIZE)
            return NX_MMAP_E_OVERFLOW;
        if (n == cap)
            return NX_MMAP_E_CAPACITY;
        out[n].base = d->PhysicalStart;
        out[n].length = d->NumberOfPages * NX_PAGE_SIZE;
        out[n].type = nx_mmap_translate_type(d->Type);
        out[n].flags = 0;
        n++;
    }
    if (n == 0)
        return NX_MMAP_E_EMPTY;

    /* Insertion sort by base: n is ~100 entries, no allocation allowed. */
    for (uint32_t i = 1; i < n; i++) {
        struct nx_mem_region r = out[i];
        uint32_t j = i;
        while (j > 0 && out[j - 1].base > r.base) {
            out[j] = out[j - 1];
            j--;
        }
        out[j] = r;
    }

    /* Reject overlaps, coalesce adjacent regions of the same type. */
    uint32_t m = 0;
    for (uint32_t i = 0; i < n; i++) {
        if (m > 0) {
            struct nx_mem_region *prev = &out[m - 1];
            uint64_t prev_end = prev->base + prev->length;
            if (out[i].base < prev_end)
                return NX_MMAP_E_OVERLAP;
            if (out[i].base == prev_end && out[i].type == prev->type) {
                prev->length += out[i].length;
                continue;
            }
        }
        out[m++] = out[i];
    }
    *out_count = m;
    return NX_MMAP_OK;
}

const char *nx_mmap_strerror(int err)
{
    switch (err) {
    case NX_MMAP_OK: return "ok";
    case NX_MMAP_E_DESC_SIZE: return "descriptor size too small";
    case NX_MMAP_E_ALIGN: return "unaligned descriptor";
    case NX_MMAP_E_OVERFLOW: return "descriptor range overflows";
    case NX_MMAP_E_CAPACITY: return "output capacity exceeded";
    case NX_MMAP_E_OVERLAP: return "firmware descriptors overlap";
    case NX_MMAP_E_EMPTY: return "empty memory map";
    default: return "unknown";
    }
}
