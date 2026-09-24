/*
 * NANOX UEFI loader (M0).
 *
 * Sequence (docs/boot-info.md, "Последовательность загрузчика"):
 *   1. serial init, "NANOX: loader start"
 *   2. read \NANOX\MANIFEST.BIN, \NANOX\KERNEL.ELF, \NANOX\INITRD.IMG and the
 *      optional \NANOX\CMDLINE.TXT
 *   3. verify kernel and initramfs size + SHA-256 against the manifest
 *   4. validate ELF64, load PT_LOAD segments at their fixed physical addresses;
 *      copy the initramfs to pages of type NX_EFI_TYPE_INITRD
 *   5. allocate initial stack and the boot-info block, collect ACPI RSDP / GOP
 *   6. GetMemoryMap + ExitBootServices, convert memory map
 *   7. jump to the kernel entry: RDI = boot info, RSP = stack top, IF = 0
 *
 * Every failure prints "NANOX: LOADER ERROR <NAME> (<code>): <detail>" and
 * writes NX_EXIT_LOADER_ERROR to isa-debug-exit.  Outside QEMU the loader
 * then returns EFI_LOAD_ERROR to the firmware (before ExitBootServices) or
 * halts (after).
 */
#include <stdint.h>

#include <nanox/bootinfo.h>
#include <nanox/diag.h>
#include <nanox/manifest.h>
#include <nanox/port.h>
#include <nanox/printf.h>
#include <nanox/serial.h>
#include <nanox/sha256.h>
#include <nanox/string.h>

#include "efi.h"
#include "elf_plan.h"
#include "mmap_convert.h"

#define KERNEL_STACK_PAGES 16u /* 64 KiB */
#define BI_CMDLINE_OFFSET 256u
#define BI_MMAP_OFFSET (BI_CMDLINE_OFFSET + NX_CMDLINE_MAX + 1u)
#define MMAP_SLACK_DESCRIPTORS 32u
#define EBS_ATTEMPTS 4
#define INITRD_MAX_BYTES (64ull << 20)

_Static_assert(BI_CMDLINE_OFFSET >= sizeof(struct nx_boot_info), "boot info block layout");
_Static_assert(BI_MMAP_OFFSET % 8 == 0, "mmap array alignment");

static EFI_BOOT_SERVICES *BS;
static int boot_services_active = 1;

static const char *loader_error_name(enum nx_loader_error e)
{
    switch (e) {
    case NX_LE_OK: return "OK";
    case NX_LE_PROTOCOL: return "E_PROTOCOL";
    case NX_LE_MANIFEST_OPEN: return "E_MANIFEST_OPEN";
    case NX_LE_MANIFEST_INVALID: return "E_MANIFEST_INVALID";
    case NX_LE_KERNEL_OPEN: return "E_KERNEL_OPEN";
    case NX_LE_KERNEL_READ: return "E_KERNEL_READ";
    case NX_LE_KERNEL_SIZE: return "E_KERNEL_SIZE";
    case NX_LE_KERNEL_HASH: return "E_KERNEL_HASH";
    case NX_LE_ELF_INVALID: return "E_ELF_INVALID";
    case NX_LE_KERNEL_ALLOC: return "E_KERNEL_ALLOC";
    case NX_LE_CMDLINE: return "E_CMDLINE";
    case NX_LE_MEMMAP: return "E_MEMMAP";
    case NX_LE_EXIT_BOOT_SERVICES: return "E_EXIT_BOOT_SERVICES";
    case NX_LE_OUT_OF_MEMORY: return "E_OUT_OF_MEMORY";
    case NX_LE_INITRD_OPEN: return "E_INITRD_OPEN";
    case NX_LE_INITRD_READ: return "E_INITRD_READ";
    case NX_LE_INITRD_SIZE: return "E_INITRD_SIZE";
    case NX_LE_INITRD_HASH: return "E_INITRD_HASH";
    }
    return "E_UNKNOWN";
}

/* Reports the error and stops.  Returns only before ExitBootServices when not
 * running under QEMU's isa-debug-exit; the caller then returns to firmware. */
static EFI_STATUS loader_fail(enum nx_loader_error e, const char *detail, EFI_STATUS st)
{
    nx_printf("NANOX: LOADER ERROR %s (%u): %s (efi_status=0x%" NX_PRIx64 ")\n",
              loader_error_name(e), (unsigned)e, detail, (uint64_t)st);
    nx_outb(NX_DEBUG_EXIT_PORT, NX_EXIT_LOADER_ERROR);
    if (!boot_services_active) {
        for (;;)
            __asm__ volatile("cli; hlt");
    }
    return EFI_LOAD_ERROR;
}

static int guid_eq(const EFI_GUID *a, const EFI_GUID *b)
{
    return memcmp(a, b, sizeof(*a)) == 0;
}

/* Reads a whole file into pool memory.  Returns EFI status; *missing is set
 * when the file does not exist. */
static EFI_STATUS read_file(EFI_FILE_PROTOCOL *root, const CHAR16 *path, uint64_t max_size,
                            uint8_t **out, uint64_t *out_size, int *missing)
{
    EFI_FILE_PROTOCOL *f = 0;
    *missing = 0;
    *out = 0;
    *out_size = 0;
    EFI_STATUS st = root->Open(root, &f, path, EFI_FILE_MODE_READ, 0);
    if (st == EFI_NOT_FOUND) {
        *missing = 1;
        return st;
    }
    if (EFI_ERROR(st))
        return st;

    uint64_t size = 0;
    st = f->SetPosition(f, UINT64_MAX); /* moves to end of file */
    if (!EFI_ERROR(st))
        st = f->GetPosition(f, &size);
    if (!EFI_ERROR(st))
        st = f->SetPosition(f, 0);
    if (!EFI_ERROR(st) && size > max_size)
        st = EFI_BUFFER_TOO_SMALL;
    uint8_t *buf = 0;
    if (!EFI_ERROR(st))
        st = BS->AllocatePool(EfiLoaderData, size ? size : 1, (VOID **)&buf);
    uint64_t done = 0;
    while (!EFI_ERROR(st) && done < size) {
        UINTN chunk = size - done;
        st = f->Read(f, &chunk, buf + done);
        if (!EFI_ERROR(st) && chunk == 0)
            st = EFI_LOAD_ERROR; /* unexpected EOF */
        done += chunk;
    }
    f->Close(f);
    if (EFI_ERROR(st)) {
        if (buf)
            BS->FreePool(buf);
        return st;
    }
    *out = buf;
    *out_size = size;
    return EFI_SUCCESS;
}

static uint64_t find_rsdp(EFI_SYSTEM_TABLE *st)
{
    static const EFI_GUID acpi20 = EFI_ACPI_20_TABLE_GUID;
    static const EFI_GUID acpi10 = EFI_ACPI_10_TABLE_GUID;
    uint64_t v1 = 0;
    for (UINTN i = 0; i < st->NumberOfTableEntries; i++) {
        const EFI_CONFIGURATION_TABLE *t = &st->ConfigurationTable[i];
        if (guid_eq(&t->VendorGuid, &acpi20))
            return (uint64_t)(uintptr_t)t->VendorTable;
        if (guid_eq(&t->VendorGuid, &acpi10))
            v1 = (uint64_t)(uintptr_t)t->VendorTable;
    }
    return v1;
}

static void fill_framebuffer(struct nx_boot_info *bi)
{
    static const EFI_GUID gop_guid = EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID;
    EFI_GRAPHICS_OUTPUT_PROTOCOL *gop = 0;
    if (EFI_ERROR(BS->LocateProtocol(&gop_guid, 0, (VOID **)&gop)) || !gop || !gop->Mode ||
        !gop->Mode->Info) {
        nx_printf("NANOX: loader framebuffer none\n");
        return;
    }
    const EFI_GRAPHICS_OUTPUT_MODE_INFORMATION *info = gop->Mode->Info;
    uint32_t format;
    if (info->PixelFormat == PixelRedGreenBlueReserved8BitPerColor)
        format = NX_FB_RGBX8888;
    else if (info->PixelFormat == PixelBlueGreenRedReserved8BitPerColor)
        format = NX_FB_BGRX8888;
    else {
        nx_printf("NANOX: loader framebuffer unsupported pixel format %u\n",
                  (unsigned)info->PixelFormat);
        return;
    }
    bi->flags |= NX_BI_HAS_FRAMEBUFFER;
    bi->fb_phys = gop->Mode->FrameBufferBase;
    bi->fb_size = gop->Mode->FrameBufferSize;
    bi->fb_width = info->HorizontalResolution;
    bi->fb_height = info->VerticalResolution;
    bi->fb_pitch = info->PixelsPerScanLine * 4u;
    bi->fb_format = format;
    nx_printf("NANOX: loader framebuffer %ux%u pitch=%u base=0x%" NX_PRIx64 "\n", bi->fb_width,
              bi->fb_height, bi->fb_pitch, bi->fb_phys);
}

/* Normalises CMDLINE.TXT: strips trailing whitespace, requires printable ASCII. */
static int copy_cmdline(char *dst, const uint8_t *src, uint64_t len, uint32_t *out_len)
{
    while (len && (src[len - 1] == '\n' || src[len - 1] == '\r' || src[len - 1] == ' ' ||
                   src[len - 1] == '\t'))
        len--;
    if (len > NX_CMDLINE_MAX)
        return -1;
    for (uint64_t i = 0; i < len; i++) {
        if (src[i] < 0x20 || src[i] > 0x7E)
            return -1;
        dst[i] = (char)src[i];
    }
    dst[len] = '\0';
    *out_len = (uint32_t)len;
    return 0;
}

__attribute__((noreturn)) static void jump_to_kernel(uint64_t entry, uint64_t stack_top,
                                                     struct nx_boot_info *bi)
{
    __asm__ volatile("cli\n\t"
                     "mov %0, %%rsp\n\t"
                     "xor %%ebp, %%ebp\n\t"
                     "jmp *%1\n\t"
                     :
                     : "r"(stack_top), "r"(entry), "D"(bi)
                     : "memory");
    __builtin_unreachable();
}

EFI_STATUS EFIAPI efi_main(EFI_HANDLE image, EFI_SYSTEM_TABLE *systab)
{
    static const EFI_GUID loaded_image_guid = EFI_LOADED_IMAGE_PROTOCOL_GUID;
    static const EFI_GUID sfs_guid = EFI_SIMPLE_FILE_SYSTEM_PROTOCOL_GUID;
    EFI_STATUS st;

    BS = systab->BootServices;
    nx_serial_init();
    nx_printf("\nNANOX: loader start\n");
    nx_printf("NANOX: loader bootinfo_version=%u.%u\n", NX_BOOTINFO_VERSION_MAJOR,
              NX_BOOTINFO_VERSION_MINOR);

    /* The external QEMU harness is the watchdog on the M0 bench. */
    BS->SetWatchdogTimer(0, 0, 0, 0);

    /* ---- file system of the device we were loaded from ---- */
    EFI_LOADED_IMAGE_PROTOCOL *li = 0;
    st = BS->HandleProtocol(image, &loaded_image_guid, (VOID **)&li);
    if (EFI_ERROR(st) || !li)
        return loader_fail(NX_LE_PROTOCOL, "LoadedImage protocol", st);
    EFI_SIMPLE_FILE_SYSTEM_PROTOCOL *sfs = 0;
    st = BS->HandleProtocol(li->DeviceHandle, &sfs_guid, (VOID **)&sfs);
    if (EFI_ERROR(st) || !sfs)
        return loader_fail(NX_LE_PROTOCOL, "SimpleFileSystem protocol", st);
    EFI_FILE_PROTOCOL *root = 0;
    st = sfs->OpenVolume(sfs, &root);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_PROTOCOL, "OpenVolume", st);

    /* ---- manifest ---- */
    uint8_t *mf_buf;
    uint64_t mf_size;
    int missing;
    st = read_file(root, u"\\NANOX\\MANIFEST.BIN", 4096, &mf_buf, &mf_size, &missing);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_MANIFEST_OPEN,
                           missing ? "\\NANOX\\MANIFEST.BIN not found" : "manifest read failed",
                           st);
    struct nx_manifest mf;
    if (mf_size != sizeof(mf))
        return loader_fail(NX_LE_MANIFEST_INVALID, "manifest size", EFI_SUCCESS);
    memcpy(&mf, mf_buf, sizeof(mf));
    BS->FreePool(mf_buf);
    static const uint8_t zero_reserved[sizeof(mf.reserved)];
    if (mf.magic != NX_MANIFEST_MAGIC || mf.version != NX_MANIFEST_VERSION ||
        mf.size != NX_MANIFEST_SIZE ||
        memcmp(mf.reserved, zero_reserved, sizeof(zero_reserved)) != 0)
        return loader_fail(NX_LE_MANIFEST_INVALID, "manifest magic/version/size/reserved",
                           EFI_SUCCESS);

    /* ---- kernel file + integrity ---- */
    uint8_t *kfile;
    uint64_t ksize;
    st = read_file(root, u"\\NANOX\\KERNEL.ELF", NX_KERNEL_SPAN_MAX * 4, &kfile, &ksize, &missing);
    if (EFI_ERROR(st) && missing)
        return loader_fail(NX_LE_KERNEL_OPEN, "\\NANOX\\KERNEL.ELF not found", st);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_KERNEL_READ, "kernel read failed", st);
    nx_printf("NANOX: loader kernel size=%" NX_PRIu64 "\n", ksize);
    if (ksize != mf.kernel_size)
        return loader_fail(NX_LE_KERNEL_SIZE, "kernel size differs from manifest", EFI_SUCCESS);
    uint8_t digest[NX_SHA256_DIGEST_SIZE];
    nx_sha256(kfile, ksize, digest);
    if (memcmp(digest, mf.kernel_sha256, sizeof(digest)) != 0) {
        nx_printf("NANOX: loader kernel sha256 actual=");
        nx_print_hex_bytes(digest, sizeof(digest));
        nx_printf(" expected=");
        nx_print_hex_bytes(mf.kernel_sha256, sizeof(mf.kernel_sha256));
        nx_printf("\n");
        return loader_fail(NX_LE_KERNEL_HASH, "kernel sha256 differs from manifest", EFI_SUCCESS);
    }
    nx_printf("NANOX: loader kernel sha256 ok ");
    nx_print_hex_bytes(digest, sizeof(digest));
    nx_printf("\n");

    /* ---- ELF load ---- */
    static struct nx_elf_plan plan;
    int elf_err = nx_elf_plan(kfile, ksize, &plan);
    if (elf_err != NX_ELF_OK)
        return loader_fail(NX_LE_ELF_INVALID, nx_elf_strerror(elf_err), EFI_SUCCESS);
    uint64_t span_pages = (plan.span_end - plan.span_base) / NX_PAGE_SIZE;
    EFI_PHYSICAL_ADDRESS kbase = plan.span_base;
    st = BS->AllocatePages(AllocateAddress, NX_EFI_TYPE_KERNEL_IMAGE, span_pages, &kbase);
    if (EFI_ERROR(st) || kbase != plan.span_base)
        return loader_fail(NX_LE_KERNEL_ALLOC, "kernel physical range unavailable", st);
    uint8_t *kmem = (uint8_t *)(uintptr_t)kbase;
    memset(kmem, 0, plan.span_end - plan.span_base);
    for (uint32_t i = 0; i < plan.segment_count; i++) {
        const struct nx_elf_segment *s = &plan.segments[i];
        memcpy(kmem + (s->phys_addr - plan.span_base), kfile + s->file_offset, s->file_size);
    }
    BS->FreePool(kfile);
    nx_printf("NANOX: loader kernel loaded span=0x%" NX_PRIx64 "-0x%" NX_PRIx64
              " entry=0x%" NX_PRIx64 " segments=%u\n",
              plan.span_base, plan.span_end, plan.entry, plan.segment_count);

    /* ---- initramfs file + integrity ---- */
    uint8_t *ifile;
    uint64_t isize;
    st = read_file(root, u"\\NANOX\\INITRD.IMG", INITRD_MAX_BYTES, &ifile, &isize, &missing);
    if (EFI_ERROR(st) && missing)
        return loader_fail(NX_LE_INITRD_OPEN, "\\NANOX\\INITRD.IMG not found", st);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_INITRD_READ, "initramfs read failed or too large", st);
    nx_printf("NANOX: loader initrd size=%" NX_PRIu64 "\n", isize);
    if (isize != mf.initrd_size || isize == 0)
        return loader_fail(NX_LE_INITRD_SIZE, "initramfs size differs from manifest", EFI_SUCCESS);
    uint8_t idigest[NX_SHA256_DIGEST_SIZE];
    nx_sha256(ifile, isize, idigest);
    if (memcmp(idigest, mf.initrd_sha256, sizeof(idigest)) != 0) {
        nx_printf("NANOX: loader initrd sha256 actual=");
        nx_print_hex_bytes(idigest, sizeof(idigest));
        nx_printf(" expected=");
        nx_print_hex_bytes(mf.initrd_sha256, sizeof(mf.initrd_sha256));
        nx_printf("\n");
        return loader_fail(NX_LE_INITRD_HASH, "initramfs sha256 differs from manifest",
                           EFI_SUCCESS);
    }
    uint64_t initrd_pages = (isize + NX_PAGE_SIZE - 1) / NX_PAGE_SIZE;
    EFI_PHYSICAL_ADDRESS initrd = 0;
    st = BS->AllocatePages(AllocateAnyPages, NX_EFI_TYPE_INITRD, initrd_pages, &initrd);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_OUT_OF_MEMORY, "initramfs pages", st);
    memset((void *)(uintptr_t)initrd, 0, initrd_pages * NX_PAGE_SIZE);
    memcpy((void *)(uintptr_t)initrd, ifile, isize);
    BS->FreePool(ifile);
    nx_printf("NANOX: loader initrd sha256 ok ");
    nx_print_hex_bytes(idigest, sizeof(idigest));
    nx_printf(" at 0x%" NX_PRIx64 "\n", (uint64_t)initrd);

    /* ---- stack ---- */
    EFI_PHYSICAL_ADDRESS stack = 0;
    st = BS->AllocatePages(AllocateAnyPages, NX_EFI_TYPE_KERNEL_STACK, KERNEL_STACK_PAGES, &stack);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_OUT_OF_MEMORY, "kernel stack", st);

    /* ---- memory map sizing, boot-info block ---- */
    UINTN map_size = 0, map_key = 0, desc_size = 0;
    UINT32 desc_version = 0;
    st = BS->GetMemoryMap(&map_size, 0, &map_key, &desc_size, &desc_version);
    if (st != EFI_BUFFER_TOO_SMALL || desc_size < sizeof(EFI_MEMORY_DESCRIPTOR))
        return loader_fail(NX_LE_MEMMAP, "GetMemoryMap size query", st);
    if (desc_version != EFI_MEMORY_DESCRIPTOR_VERSION)
        return loader_fail(NX_LE_MEMMAP, "unexpected descriptor version", EFI_SUCCESS);
    UINTN map_cap = map_size + MMAP_SLACK_DESCRIPTORS * desc_size;
    uint8_t *map = 0;
    st = BS->AllocatePool(EfiLoaderData, map_cap, (VOID **)&map);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_OUT_OF_MEMORY, "memory map buffer", st);
    /* The converted map never has more entries than the raw map. */
    uint32_t region_cap = (uint32_t)(map_cap / desc_size);
    uint64_t bi_bytes = BI_MMAP_OFFSET + (uint64_t)region_cap * sizeof(struct nx_mem_region);
    uint64_t bi_pages = (bi_bytes + NX_PAGE_SIZE - 1) / NX_PAGE_SIZE;
    EFI_PHYSICAL_ADDRESS bi_phys = 0;
    st = BS->AllocatePages(AllocateAnyPages, NX_EFI_TYPE_BOOT_INFO, bi_pages, &bi_phys);
    if (EFI_ERROR(st))
        return loader_fail(NX_LE_OUT_OF_MEMORY, "boot info block", st);
    uint8_t *bi_block = (uint8_t *)(uintptr_t)bi_phys;
    memset(bi_block, 0, bi_pages * NX_PAGE_SIZE);
    struct nx_boot_info *bi = (struct nx_boot_info *)bi_block;
    char *cmdline = (char *)(bi_block + BI_CMDLINE_OFFSET);
    struct nx_mem_region *regions = (struct nx_mem_region *)(bi_block + BI_MMAP_OFFSET);

    bi->magic = NX_BOOTINFO_MAGIC;
    bi->version_major = NX_BOOTINFO_VERSION_MAJOR;
    bi->version_minor = NX_BOOTINFO_VERSION_MINOR;
    bi->size = sizeof(*bi);
    bi->mmap_phys = (uint64_t)(uintptr_t)regions;
    bi->mmap_entry_size = sizeof(struct nx_mem_region);
    bi->kernel_phys_base = plan.span_base;
    bi->kernel_phys_size = plan.span_end - plan.span_base;
    bi->kernel_entry = plan.entry;
    bi->stack_phys_base = stack;
    bi->stack_size = KERNEL_STACK_PAGES * NX_PAGE_SIZE;
    bi->cmdline_phys = (uint64_t)(uintptr_t)cmdline;
    bi->uefi_system_table_phys = (uint64_t)(uintptr_t)systab;
    memcpy(bi->kernel_sha256, digest, sizeof(digest));
    bi->flags |= NX_BI_HAS_INITRD;
    bi->initrd_phys = initrd;
    bi->initrd_size = isize;
    memcpy(bi->initrd_sha256, idigest, sizeof(idigest));

    /* ---- command line (optional file) ---- */
    uint8_t *cl_buf;
    uint64_t cl_size;
    st = read_file(root, u"\\NANOX\\CMDLINE.TXT", NX_CMDLINE_MAX + 2, &cl_buf, &cl_size, &missing);
    if (EFI_ERROR(st) && !missing)
        return loader_fail(NX_LE_CMDLINE, "cmdline read failed or too long", st);
    if (!EFI_ERROR(st)) {
        int bad = copy_cmdline(cmdline, cl_buf, cl_size, &bi->cmdline_len);
        BS->FreePool(cl_buf);
        if (bad)
            return loader_fail(NX_LE_CMDLINE, "cmdline too long or not printable ASCII",
                               EFI_SUCCESS);
    }
    nx_printf("NANOX: loader cmdline \"%s\"\n", cmdline);
    root->Close(root);

    /* ---- optional platform data ---- */
    uint64_t rsdp = find_rsdp(systab);
    if (rsdp) {
        bi->flags |= NX_BI_HAS_ACPI_RSDP;
        bi->acpi_rsdp_phys = rsdp;
    }
    nx_printf("NANOX: loader acpi_rsdp=0x%" NX_PRIx64 "\n", rsdp);
    fill_framebuffer(bi);

    /* ---- final memory map + ExitBootServices ---- */
    for (int attempt = 0;; attempt++) {
        if (attempt == EBS_ATTEMPTS)
            return loader_fail(NX_LE_EXIT_BOOT_SERVICES, "ExitBootServices kept failing", st);
        map_size = map_cap;
        st = BS->GetMemoryMap(&map_size, (EFI_MEMORY_DESCRIPTOR *)map, &map_key, &desc_size,
                              &desc_version);
        if (EFI_ERROR(st))
            return loader_fail(NX_LE_MEMMAP, "GetMemoryMap", st);
        /* No boot-services call (and no allocation) between here and EBS. */
        st = BS->ExitBootServices(image, map_key);
        if (!EFI_ERROR(st))
            break;
    }
    boot_services_active = 0;

    uint32_t count = 0;
    int mm_err = nx_mmap_from_uefi(map, map_size, desc_size, regions, region_cap, &count);
    if (mm_err != NX_MMAP_OK)
        return loader_fail(NX_LE_MEMMAP, nx_mmap_strerror(mm_err), EFI_SUCCESS);
    bi->mmap_count = count;

    nx_printf("NANOX: loader exit_boot_services ok regions=%u\n", count);
    nx_printf("NANOX: loader jump entry=0x%" NX_PRIx64 " bootinfo=0x%" NX_PRIx64
              " stack_top=0x%" NX_PRIx64 "\n",
              plan.entry, (uint64_t)bi_phys, bi->stack_phys_base + bi->stack_size);
    jump_to_kernel(plan.entry, bi->stack_phys_base + bi->stack_size, bi);
}
