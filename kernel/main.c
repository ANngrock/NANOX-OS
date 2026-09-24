/*
 * NANOX kernel, M0 bench build.
 *
 * In M0 the kernel only proves the boot contract: it validates the boot info
 * handed over by the loader, reports it on the serial port and finishes with
 * a verdict through isa-debug-exit.  The command line selects the scenario
 * used by the headless harness (docs/m0-bench.md):
 *
 *   (none) / nanox.test=pass   boot-info self check, then TEST PASS
 *   nanox.test=fail            deliberate TEST FAIL
 *   nanox.test=panic           deliberate kernel panic
 *   nanox.test=hang            halt forever; the harness must time out
 */
#include <stdint.h>

#include <nanox/bootinfo.h>
#include <nanox/diag.h>
#include <nanox/printf.h>
#include <nanox/serial.h>
#include <nanox/sha256.h>
#include <nanox/string.h>

#include "bootinfo_check.h"
#include "initramfs.h"
#include "kernel.h"

enum test_mode { MODE_PASS, MODE_FAIL, MODE_PANIC, MODE_HANG, MODE_UNKNOWN };

/* M0 runs on the UEFI identity mapping: physical == virtual. */
static const void *identity_map(void *opaque, uint64_t phys, uint64_t len)
{
    (void)opaque;
    if (phys == 0 || len == 0 || phys > UINT64_MAX - len)
        return 0;
    return (const void *)(uintptr_t)phys;
}

static int token_eq(const char *tok, uint32_t len, const char *lit)
{
    uint32_t n = (uint32_t)nx_strlen(lit);
    return len == n && memcmp(tok, lit, n) == 0;
}

/* Finds "nanox.test=<value>"; the last occurrence wins. */
static enum test_mode parse_mode(const char *cl, uint32_t len, const char **val, uint32_t *vlen)
{
    static const char key[] = "nanox.test=";
    const uint32_t klen = sizeof(key) - 1;
    enum test_mode mode = MODE_PASS;
    *val = "pass";
    *vlen = 4;
    uint32_t i = 0;
    while (i < len) {
        while (i < len && cl[i] == ' ')
            i++;
        uint32_t start = i;
        while (i < len && cl[i] != ' ')
            i++;
        uint32_t tlen = i - start;
        if (tlen < klen || memcmp(cl + start, key, klen) != 0)
            continue;
        const char *v = cl + start + klen;
        uint32_t n = tlen - klen;
        *val = v;
        *vlen = n;
        if (token_eq(v, n, "pass"))
            mode = MODE_PASS;
        else if (token_eq(v, n, "fail"))
            mode = MODE_FAIL;
        else if (token_eq(v, n, "panic"))
            mode = MODE_PANIC;
        else if (token_eq(v, n, "hang"))
            mode = MODE_HANG;
        else
            mode = MODE_UNKNOWN;
    }
    return mode;
}

static const char *mem_type_name(uint32_t t)
{
    switch (t) {
    case NX_MEM_USABLE: return "usable";
    case NX_MEM_RESERVED: return "reserved";
    case NX_MEM_ACPI_RECLAIMABLE: return "acpi_reclaimable";
    case NX_MEM_ACPI_NVS: return "acpi_nvs";
    case NX_MEM_MMIO: return "mmio";
    case NX_MEM_BOOT_RECLAIMABLE: return "boot_reclaimable";
    case NX_MEM_KERNEL_IMAGE: return "kernel_image";
    case NX_MEM_KERNEL_STACK: return "kernel_stack";
    case NX_MEM_BOOT_INFO: return "boot_info";
    case NX_MEM_FIRMWARE_RUNTIME: return "firmware_runtime";
    default: return "?";
    }
}

static void report(const struct nx_bi_result *r)
{
    const struct nx_boot_info *bi = r->bi;
    for (uint32_t i = 0; i < bi->mmap_count; i++) {
        const struct nx_mem_region *e = &r->regions[i];
        nx_printf("NANOX: mmap %02u 0x%016" NX_PRIx64 "-0x%016" NX_PRIx64 " %s\n", i, e->base,
                  e->base + e->length, mem_type_name(e->type));
    }
    nx_printf("NANOX: bootinfo ok version=%u.%u size=%u regions=%u usable_bytes=%" NX_PRIu64 "\n",
              bi->version_major, bi->version_minor, bi->size, bi->mmap_count, r->usable_bytes);
    nx_printf("NANOX: kernel image=0x%" NX_PRIx64 "-0x%" NX_PRIx64 " entry=0x%" NX_PRIx64
              " stack=0x%" NX_PRIx64 "-0x%" NX_PRIx64 "\n",
              bi->kernel_phys_base, bi->kernel_phys_base + bi->kernel_phys_size, bi->kernel_entry,
              bi->stack_phys_base, bi->stack_phys_base + bi->stack_size);
    nx_printf("NANOX: kernel sha256 ");
    nx_print_hex_bytes(bi->kernel_sha256, sizeof(bi->kernel_sha256));
    nx_printf("\n");
    if (bi->flags & NX_BI_HAS_ACPI_RSDP)
        nx_printf("NANOX: bootinfo acpi_rsdp=0x%" NX_PRIx64 "\n", bi->acpi_rsdp_phys);
    else
        nx_printf("NANOX: bootinfo acpi_rsdp none\n");
    if (bi->flags & NX_BI_HAS_FRAMEBUFFER)
        nx_printf("NANOX: bootinfo framebuffer %ux%u pitch=%u format=%u base=0x%" NX_PRIx64 "\n",
                  bi->fb_width, bi->fb_height, bi->fb_pitch, bi->fb_format, bi->fb_phys);
    else
        nx_printf("NANOX: bootinfo framebuffer none\n");
}

/* The loader verified the initramfs against the manifest; the kernel checks
 * that the bytes it received are the ones the loader hashed and that the
 * archive is well formed before anything reads from it. */
static void check_initramfs(const struct nx_boot_info *bi)
{
    if (bi->version_minor < 1 || !(bi->flags & NX_BI_HAS_INITRD))
        nx_panic("initramfs missing from boot info");
    const uint8_t *base = (const uint8_t *)(uintptr_t)bi->initrd_phys;
    uint8_t digest[NX_SHA256_DIGEST_SIZE];
    nx_sha256(base, bi->initrd_size, digest);
    if (memcmp(digest, bi->initrd_sha256, sizeof(digest)) != 0)
        nx_panic("initramfs sha256 differs from boot info");
    uint32_t entries;
    uint64_t bad;
    int st = nx_cpio_validate(base, bi->initrd_size, &entries, &bad);
    if (st != NX_CPIO_OK)
        nx_panic("initramfs invalid: %s at offset %" NX_PRIu64, nx_cpio_strerror(st), bad);
    struct nx_cpio_entry rel;
    st = nx_cpio_find(base, bi->initrd_size, "etc/nanox/release", &rel);
    if (st != NX_CPIO_OK || (rel.mode & NX_CPIO_MODE_TYPE) != NX_CPIO_MODE_REG)
        nx_panic("initramfs: etc/nanox/release missing (%s)", nx_cpio_strerror(st));
    nx_printf("NANOX: initramfs ok entries=%u bytes=%" NX_PRIu64 " sha256=", entries,
              bi->initrd_size);
    nx_print_hex_bytes(digest, sizeof(digest));
    nx_printf("\nNANOX: initramfs release \"");
    for (uint64_t i = 0; i < rel.size && rel.data[i] != '\n'; i++)
        nx_serial_putc(rel.data[i] >= 0x20 && rel.data[i] < 0x7F ? (char)rel.data[i] : '?');
    nx_printf("\"\n");
}

__attribute__((noreturn)) void kernel_main(const struct nx_boot_info *bi)
{
    nx_serial_init();
    nx_printf("NANOX: kernel_main bootinfo=%p\n", (const void *)bi);

    struct nx_bi_check chk = {
        .map = identity_map,
        .opaque = 0,
        .bi_phys = (uint64_t)(uintptr_t)bi,
        .image_start = (uint64_t)(uintptr_t)__kernel_start,
        .image_end = (uint64_t)(uintptr_t)__kernel_end,
    };
    struct nx_bi_result res;
    if (nx_bootinfo_check(&chk, &res) != NX_BI_OK)
        nx_panic("bootinfo invalid: %s index=%u", nx_bi_strerror(res.error), res.index);

    /* The stack we are running on must be the one the boot info describes. */
    uint64_t rsp;
    __asm__ volatile("mov %%rsp, %0" : "=r"(rsp));
    if (rsp < bi->stack_phys_base || rsp > bi->stack_phys_base + bi->stack_size)
        nx_panic("running stack 0x%" NX_PRIx64 " outside boot-info stack region", rsp);

    report(&res);
    check_initramfs(bi);

    const char *cl = (const char *)(uintptr_t)bi->cmdline_phys;
    nx_printf("NANOX: cmdline \"%s\"\n", cl);
    const char *val;
    uint32_t vlen;
    switch (parse_mode(cl, bi->cmdline_len, &val, &vlen)) {
    case MODE_PASS: nx_printf("NANOX: TEST PASS\n"); nx_debug_exit(NX_EXIT_TEST_PASS);
    case MODE_FAIL:
        nx_printf("NANOX: TEST FAIL requested by nanox.test=fail\n");
        nx_debug_exit(NX_EXIT_TEST_FAIL);
    case MODE_PANIC: nx_panic("requested by nanox.test=panic");
    case MODE_HANG:
        nx_printf("NANOX: test hang: halting with interrupts disabled\n");
        for (;;)
            __asm__ volatile("cli; hlt");
    case MODE_UNKNOWN: break;
    }
    nx_printf("NANOX: TEST FAIL unknown nanox.test value (%u bytes)\n", vlen);
    nx_debug_exit(NX_EXIT_TEST_FAIL);
}
