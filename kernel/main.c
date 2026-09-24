/*
 * NANOX kernel (M1).
 *
 * Boot sequence: own GDT/TSS/IDT, boot info validation, physical allocator,
 * own page tables (CR3 switch), reclaim of boot memory, initramfs check,
 * self-tests, then the scenario selected with nanox.test= on the command
 * line (docs/m0-bench.md, docs/m1-kernel.md):
 *
 *   (none) / pass   self-tests, then TEST PASS
 *   fail            deliberate TEST FAIL
 *   panic           deliberate kernel panic
 *   hang            halt forever; the harness must time out
 *   ud, gp, divzero, pagefault, nullderef, wprotect, nxexec, stackoverflow
 *                   deliberate CPU exceptions (kernel/faults.c)
 *   doublefree      the page allocator must reject a double free (panic)
 *   timer-masked    timer left masked: the timer check must report TEST FAIL
 */
#include <stdint.h>

#include <nanox/bootinfo.h>
#include <nanox/diag.h>
#include <nanox/printf.h>
#include <nanox/serial.h>
#include <nanox/sha256.h>
#include <nanox/string.h>

#include "arch/x86_64/cpu.h"
#include "arch/x86_64/timer.h"
#include "arch/x86_64/trap.h"
#include "bootinfo_check.h"
#include "faults.h"
#include "initramfs.h"
#include "kernel.h"
#include "mm/mm.h"

/* M0/M1 early boot runs on the UEFI identity mapping: physical == virtual. */
static const void *identity_map(void *opaque, uint64_t phys, uint64_t len)
{
    (void)opaque;
    if (phys == 0 || len == 0 || phys > UINT64_MAX - len)
        return 0;
    return (const void *)(uintptr_t)phys;
}

enum mode_kind { K_PASS, K_FAIL, K_PANIC, K_HANG, K_FAULT, K_TIMER_MASKED };

struct test_mode {
    const char *name;
    enum mode_kind kind;
    void (*fault)(void);
};

static void fault_nullderef(void)
{
    (void)nx_fault_nullderef();
}

static void fault_stack_overflow(void)
{
    (void)nx_fault_stack_overflow(0);
}

/* Frees the same page twice: the allocator must refuse and the kernel panic. */
static void fault_double_free(void)
{
    uint64_t p = nx_page_alloc();
    nx_page_free(p);
    nx_page_free(p);
}

/* nanox.test=<name> values (docs/m0-bench.md, docs/m1-kernel.md). */
static const struct test_mode MODES[] = {
    {"pass", K_PASS, 0},
    {"fail", K_FAIL, 0},
    {"panic", K_PANIC, 0},
    {"hang", K_HANG, 0},
    {"ud", K_FAULT, nx_fault_ud},
    {"gp", K_FAULT, nx_fault_gp},
    {"divzero", K_FAULT, nx_fault_divzero},
    {"pagefault", K_FAULT, nx_fault_pagefault},
    {"nullderef", K_FAULT, fault_nullderef},
    {"wprotect", K_FAULT, nx_fault_wprotect},
    {"nxexec", K_FAULT, nx_fault_nxexec},
    {"stackoverflow", K_FAULT, fault_stack_overflow},
    {"doublefree", K_FAULT, fault_double_free},
    {"timer-masked", K_TIMER_MASKED, 0},
};

static int token_eq(const char *tok, uint32_t len, const char *lit)
{
    uint32_t n = (uint32_t)nx_strlen(lit);
    return len == n && memcmp(tok, lit, n) == 0;
}

/* Finds "nanox.test=<value>" (the last occurrence wins); NULL if unknown. */
static const struct test_mode *parse_mode(const char *cl, uint32_t len, uint32_t *vlen)
{
    static const char key[] = "nanox.test=";
    const uint32_t klen = sizeof(key) - 1;
    const struct test_mode *mode = &MODES[0];
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
        *vlen = n;
        mode = 0;
        for (unsigned m = 0; m < sizeof(MODES) / sizeof(MODES[0]); m++)
            if (token_eq(v, n, MODES[m].name))
                mode = &MODES[m];
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
              " loader_stack=0x%" NX_PRIx64 "-0x%" NX_PRIx64 "\n",
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
    const uint8_t *base = nx_phys_to_virt(bi->initrd_phys);
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

/* Fails the run with a TEST FAIL verdict. */
__attribute__((noreturn)) static void test_fail(const char *why)
{
    nx_printf("NANOX: TEST FAIL %s\n", why);
    nx_debug_exit(NX_EXIT_TEST_FAIL);
}

/* The exception path must also return: #BP is handled and resumed. */
static void selftest_breakpoint(void)
{
    uint64_t before = nx_breakpoint_count;
    __asm__ volatile("int3");
    __asm__ volatile("int3");
    if (nx_breakpoint_count != before + 2)
        test_fail("selftest breakpoint: #BP handler did not resume");
    nx_printf("NANOX: selftest breakpoint ok resumed=%" NX_PRIu64 "\n", nx_breakpoint_count);
}

static int region_type_of(const struct nx_mem_region *rg, uint32_t n, uint64_t phys)
{
    for (uint32_t i = 0; i < n; i++)
        if (phys >= rg[i].base && phys - rg[i].base < rg[i].length)
            return (int)rg[i].type;
    return -1;
}

/* Allocates pages, checks where they come from and that they do not alias,
 * frees them again and exercises the allocator's error paths. */
static void selftest_pmm(const struct nx_mem_region *rg, uint32_t n)
{
    enum { N = 256 };
    static uint64_t pages[N];
    uint64_t free_before = nx_pmm.free_pages;
    for (unsigned i = 0; i < N; i++) {
        uint64_t p = nx_page_alloc();
        int t = region_type_of(rg, n, p);
        if ((p & 0xFFF) || p < NX_PMM_MIN_PHYS ||
            (t != NX_MEM_USABLE && t != NX_MEM_BOOT_RECLAIMABLE && t != NX_MEM_KERNEL_STACK))
            test_fail("selftest pmm: page outside allocatable memory");
        for (unsigned j = 0; j < i; j++)
            if (pages[j] == p)
                test_fail("selftest pmm: page handed out twice");
        uint64_t *v = nx_phys_to_virt(p);
        v[0] = p ^ 0x4E414E4F58504D4Dull;
        v[511] = ~p;
        pages[i] = p;
    }
    for (unsigned i = 0; i < N; i++) {
        const uint64_t *v = nx_phys_to_virt(pages[i]);
        if (v[0] != (pages[i] ^ 0x4E414E4F58504D4Dull) || v[511] != ~pages[i])
            test_fail("selftest pmm: page contents changed (aliasing)");
    }
    for (unsigned i = 0; i < N; i++)
        nx_page_free(pages[i]);
    if (nx_pmm.free_pages != free_before)
        test_fail("selftest pmm: free count not restored");
    uint64_t p = nx_page_alloc();
    nx_page_free(p);
    if (nx_pmm_free(&nx_pmm, p) != NX_PMM_E_DOUBLE_FREE ||
        nx_pmm_free(&nx_pmm, (uint64_t)(uintptr_t)__kernel_start) != NX_PMM_E_UNMANAGED ||
        nx_pmm_free(&nx_pmm, p + 8) != NX_PMM_E_ALIGN)
        test_fail("selftest pmm: invalid free not rejected");
    nx_printf("NANOX: selftest pmm ok pages=%u free=%" NX_PRIu64
              " double_free=rejected unmanaged=rejected\n",
              (unsigned)N, nx_pmm.free_pages);
}

/* Maps a fresh page at a scratch address, checks the translation and the
 * data through both aliases, unmaps it and checks that it is gone. */
static void selftest_vmm(void)
{
    uint64_t p = nx_page_alloc(), pa, flags, size;
    if (nx_vmm_map_page(NX_VMM_SELFTEST_VA, p, NX_PTE_W | NX_PTE_NX | NX_PTE_G) != NX_PT_OK)
        test_fail("selftest vmm: map failed");
    *(volatile uint64_t *)NX_VMM_SELFTEST_VA = 0x56414C4944415445ull;
    if (*(volatile uint64_t *)nx_phys_to_virt(p) != 0x56414C4944415445ull)
        test_fail("selftest vmm: scratch page does not alias its physmap window");
    if (nx_vmm_query(NX_VMM_SELFTEST_VA, &pa, &flags, &size) != NX_PT_OK || pa != p ||
        size != NX_PAGE_4K || !(flags & NX_PTE_W) || !(flags & NX_PTE_NX))
        test_fail("selftest vmm: query mismatch");
    if (nx_vmm_unmap_page(NX_VMM_SELFTEST_VA, &pa) != NX_PT_OK || pa != p ||
        nx_vmm_query(NX_VMM_SELFTEST_VA, &pa, &flags, &size) != NX_PT_E_NOT_MAPPED)
        test_fail("selftest vmm: unmap failed");
    nx_page_free(p);
    nx_printf("NANOX: selftest vmm ok va=0x%016" NX_PRIx64 " pa=0x%" NX_PRIx64 "\n",
              NX_VMM_SELFTEST_VA, p);
}

/* Periodic timer interrupts must arrive at the calibrated rate.  In the
 * timer-masked scenario the timer is deliberately left masked and this check
 * must report the failure. */
static void selftest_timer(int masked)
{
    struct nx_timer_result t = {0, 0, 0};
    const char *err = nx_timer_check(masked, &t);
    if (err) {
        nx_printf("NANOX: TEST FAIL timer: %s (ticks=%" NX_PRIu64 " expected=%u)\n", err, t.ticks,
                  NX_TIMER_WINDOW_PERIODS);
        nx_debug_exit(NX_EXIT_TEST_FAIL);
    }
    nx_printf("NANOX: timer ok source=lapic vector=0x%x hz=%u lapic_per_10ms=%u window_ms=%u"
              " ticks=%" NX_PRIu64 " expected=%u tsc_delta=%" NX_PRIu64 " spurious=%" NX_PRIu64
              "\n",
              NX_VEC_TIMER, NX_TIMER_HZ, t.lapic_per_period, NX_TIMER_WINDOW_PERIODS * 10, t.ticks,
              NX_TIMER_WINDOW_PERIODS, t.tsc_delta, nx_spurious_count);
}

/* After the switch nothing uses firmware boot-services memory or the
 * loader's stack any more: hand them to the allocator. */
static void reclaim_boot_memory(const struct nx_mem_region *rg, uint32_t n)
{
    uint64_t boot, stack;
    nx_pmm_add_type(&nx_pmm, rg, n, NX_MEM_BOOT_RECLAIMABLE, &boot);
    nx_pmm_add_type(&nx_pmm, rg, n, NX_MEM_KERNEL_STACK, &stack);
    /* Overwrite every reclaimed page: if anything (firmware page tables,
     * GDT/IDT, loader data) were still in use after ExitBootServices and the
     * CR3 switch, the rest of the boot would fail. */
    uint64_t poisoned = 0;
    for (uint32_t i = 0; i < n; i++) {
        if (rg[i].type != NX_MEM_BOOT_RECLAIMABLE && rg[i].type != NX_MEM_KERNEL_STACK)
            continue;
        for (uint64_t pa = rg[i].base; pa < rg[i].base + rg[i].length; pa += 4096) {
            if (!nx_pmm_is_free(&nx_pmm, pa))
                continue;
            memset(nx_phys_to_virt(pa), 0xCC, 4096);
            poisoned++;
        }
    }
    if (poisoned != boot + stack)
        nx_panic("reclaim: poisoned %" NX_PRIu64 " pages, expected %" NX_PRIu64, poisoned,
                 boot + stack);
    nx_printf("NANOX: pmm reclaimed boot_reclaimable=%" NX_PRIu64 " loader_stack=%" NX_PRIu64
              " pages poisoned=%" NX_PRIu64 " free=%" NX_PRIu64 "\n",
              boot, stack, poisoned, nx_pmm.free_pages);
}

__attribute__((noreturn)) static void run_mode(const struct test_mode *mode, uint32_t vlen)
{
    if (!mode) {
        nx_printf("NANOX: TEST FAIL unknown nanox.test value (%u bytes)\n", vlen);
        nx_debug_exit(NX_EXIT_TEST_FAIL);
    }
    switch (mode->kind) {
    case K_PASS: nx_printf("NANOX: TEST PASS\n"); nx_debug_exit(NX_EXIT_TEST_PASS);
    case K_FAIL: test_fail("requested by nanox.test=fail");
    case K_PANIC: nx_panic("requested by nanox.test=panic");
    case K_HANG:
        nx_printf("NANOX: test hang: halting with interrupts disabled\n");
        for (;;)
            __asm__ volatile("cli; hlt");
    case K_FAULT:
        nx_printf("NANOX: test fault %s: expecting a failure report\n", mode->name);
        mode->fault();
        test_fail("deliberate fault did not trap");
    case K_TIMER_MASKED: test_fail("timer-masked: the timer check did not detect the masked timer");
    }
    test_fail("bad test mode");
}

__attribute__((noreturn)) void kernel_main(const struct nx_boot_info *boot_bi)
{
    const struct nx_boot_info *bi = boot_bi; /* identity-mapped until nx_vmm_init */
    nx_serial_init();
    nx_printf("NANOX: kernel_main bootinfo=%p\n", (const void *)bi);

    /* Own descriptor tables first, so any later fault gets a report. */
    nx_gdt_init();
    nx_idt_init();
    nx_printf("NANOX: cpu gdt+tss+idt loaded boot_stack=0x%" NX_PRIx64 "-0x%" NX_PRIx64 " ist=3\n",
              (uint64_t)(uintptr_t)__boot_stack_bottom, (uint64_t)(uintptr_t)__boot_stack_top);

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

    /* The loader must have entered with RSP at the top of its stack region;
     * _start then switched to the kernel's own stack. */
    if (nx_boot_entry_rsp != bi->stack_phys_base + bi->stack_size)
        nx_panic("entry RSP 0x%" NX_PRIx64 " is not the top of the boot-info stack",
                 nx_boot_entry_rsp);
    uint64_t rsp = nx_read_rsp();
    if (rsp <= (uint64_t)(uintptr_t)__boot_stack_bottom ||
        rsp > (uint64_t)(uintptr_t)__boot_stack_top)
        nx_panic("not running on the kernel boot stack (rsp=0x%" NX_PRIx64 ")", rsp);

    report(&res);

    /* Own memory management: allocator, page tables, CR3 switch. */
    uint64_t bi_phys = (uint64_t)(uintptr_t)boot_bi;
    uint32_t nreg = bi->mmap_count;
    nx_pmm_setup(res.regions, nreg);
    nx_vmm_init(res.regions, nreg);
    bi = nx_phys_to_virt(bi_phys); /* the identity mapping is gone now */
    const struct nx_mem_region *rg = nx_phys_to_virt(bi->mmap_phys);
    reclaim_boot_memory(rg, nreg);

    check_initramfs(bi);
    selftest_breakpoint();
    selftest_pmm(rg, nreg);
    selftest_vmm();

    const char *cl = nx_phys_to_virt(bi->cmdline_phys);
    nx_printf("NANOX: cmdline \"%s\"\n", cl);
    uint32_t vlen;
    const struct test_mode *mode = parse_mode(cl, bi->cmdline_len, &vlen);
    selftest_timer(mode && mode->kind == K_TIMER_MASKED);
    run_mode(mode, vlen);
}
