#include <stdint.h>
#include <string.h>

#include "efi.h"
#include "mmap_convert.h"
#include "test.h"

/* OVMF reports 48-byte descriptors (40-byte struct + padding). */
struct desc48 {
    EFI_MEMORY_DESCRIPTOR d;
    uint64_t pad;
};
_Static_assert(sizeof(struct desc48) == 48, "desc48");

static struct desc48 in[16];
static struct nx_mem_region out[16];

static void set(int i, uint32_t type, uint64_t base, uint64_t pages)
{
    memset(&in[i], 0, sizeof(in[i]));
    in[i].d.Type = type;
    in[i].d.PhysicalStart = base;
    in[i].d.NumberOfPages = pages;
}

static int convert(int n, uint32_t cap, uint32_t *count)
{
    return nx_mmap_from_uefi((const uint8_t *)in, (uint64_t)n * sizeof(in[0]), sizeof(in[0]), out,
                             cap, count);
}

void test_mmap(void)
{
    uint32_t count;

    /* Unsorted input with coalescing, skipping and type translation. */
    set(0, EfiConventionalMemory, 0x200000, 16);
    set(1, NX_EFI_TYPE_KERNEL_IMAGE, 0x100000, 3);
    set(2, EfiConventionalMemory, 0x210000, 16);
    set(3, EfiBootServicesData, 0x103000, 1);
    set(4, EfiLoaderCode, 0x104000, 1);
    set(5, EfiConventionalMemory, 0x900000, 0); /* empty: skipped */
    set(6, EfiMemoryMappedIO, 0xFEC00000, 1);
    set(7, 0x70000000, 0x300000, 1); /* OEM type */
    set(8, EfiPersistentMemory, 0x301000, 1);
    CHECK_EQ_INT(convert(9, 16, &count), NX_MMAP_OK);
    CHECK_EQ_INT(count, 5);
    CHECK_EQ_INT(out[0].base, 0x100000);
    CHECK_EQ_INT(out[0].length, 0x3000);
    CHECK_EQ_INT(out[0].type, NX_MEM_KERNEL_IMAGE);
    CHECK_EQ_INT(out[1].base, 0x103000);
    CHECK_EQ_INT(out[1].length, 0x2000);
    CHECK_EQ_INT(out[1].type, NX_MEM_BOOT_RECLAIMABLE);
    CHECK_EQ_INT(out[2].base, 0x200000);
    CHECK_EQ_INT(out[2].length, 0x20000);
    CHECK_EQ_INT(out[2].type, NX_MEM_USABLE);
    CHECK_EQ_INT(out[3].base, 0x300000);
    CHECK_EQ_INT(out[3].length, 0x2000);
    CHECK_EQ_INT(out[3].type, NX_MEM_RESERVED);
    CHECK_EQ_INT(out[4].type, NX_MEM_MMIO);
    for (uint32_t i = 0; i < count; i++)
        CHECK_EQ_INT(out[i].flags, 0);

    /* Errors. */
    set(0, EfiConventionalMemory, 0x100000, 4);
    set(1, EfiLoaderData, 0x102000, 1); /* inside the first */
    CHECK_EQ_INT(convert(2, 16, &count), NX_MMAP_E_OVERLAP);
    set(1, EfiLoaderData, 0x100000, 1); /* same base */
    CHECK_EQ_INT(convert(2, 16, &count), NX_MMAP_E_OVERLAP);
    set(1, EfiLoaderData, 0x200800, 1);
    CHECK_EQ_INT(convert(2, 16, &count), NX_MMAP_E_ALIGN);
    set(1, EfiLoaderData, 0xFFFFFFFFFFFFF000ull, 1); /* end would be 2^64 */
    CHECK_EQ_INT(convert(2, 16, &count), NX_MMAP_E_OVERFLOW);
    set(1, EfiLoaderData, 0x1000, UINT64_MAX / 4096);
    CHECK_EQ_INT(convert(2, 16, &count), NX_MMAP_E_OVERFLOW);
    set(1, EfiLoaderData, 0x200000, 1);
    CHECK_EQ_INT(convert(2, 1, &count), NX_MMAP_E_CAPACITY);
    CHECK_EQ_INT(convert(0, 16, &count), NX_MMAP_E_EMPTY);
    CHECK_EQ_INT(nx_mmap_from_uefi((const uint8_t *)in, 64, 32, out, 16, &count),
                 NX_MMAP_E_DESC_SIZE);

    /* Translation table. */
    CHECK_EQ_INT(nx_mmap_translate_type(EfiReservedMemoryType), NX_MEM_RESERVED);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiLoaderData), NX_MEM_BOOT_RECLAIMABLE);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiBootServicesCode), NX_MEM_BOOT_RECLAIMABLE);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiRuntimeServicesCode), NX_MEM_FIRMWARE_RUNTIME);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiRuntimeServicesData), NX_MEM_FIRMWARE_RUNTIME);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiUnusableMemory), NX_MEM_RESERVED);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiACPIReclaimMemory), NX_MEM_ACPI_RECLAIMABLE);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiACPIMemoryNVS), NX_MEM_ACPI_NVS);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiMemoryMappedIOPortSpace), NX_MEM_MMIO);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiPalCode), NX_MEM_RESERVED);
    CHECK_EQ_INT(nx_mmap_translate_type(EfiUnacceptedMemoryType), NX_MEM_RESERVED);
    CHECK_EQ_INT(nx_mmap_translate_type(NX_EFI_TYPE_KERNEL_STACK), NX_MEM_KERNEL_STACK);
    CHECK_EQ_INT(nx_mmap_translate_type(NX_EFI_TYPE_BOOT_INFO), NX_MEM_BOOT_INFO);
    CHECK_EQ_INT(nx_mmap_translate_type(0x80000099u), NX_MEM_RESERVED);
}
