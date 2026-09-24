/*
 * Kernel address space (M1) and task address spaces (M2).
 *
 *   0x0000000000200000..  kernel image at its link address (VA == PA):
 *                         .text R-X, .rodata R--, .data/.bss RW-,
 *                         stacks RW- with unmapped guard pages
 *   0xFFFF800000000000 +  physmap: every RAM region except the kernel image,
 *   phys                  RW-, 2 MiB pages where alignment allows;
 *                         MMIO pages added on demand, uncached
 *   0xFFFFC00000000000    VMM self-test scratch page
 *   0xFFFFFE0000000000    kernel stacks of tasks (M2, kernel/task.c)
 *
 * Everything else, including the null page and the rest of the low half, is
 * not mapped.  All kernel mappings are global and supervisor-only.  A task
 * address space (nx_as_create) shares PML4 slot 0 and slots 256..511 with
 * the kernel and owns slots 1..255 (docs/m2-kernel.md).
 */
#include <nanox/printf.h>

#include "arch/x86_64/cpu.h"
#include "kernel.h"
#include "mm.h"

struct nx_pmm nx_pmm;
uint64_t nx_kernel_root;
uint64_t nx_phys_offset;

static uint64_t tables_allocated;

static uint64_t env_alloc(void *ctx)
{
    (void)ctx;
    uint64_t p = nx_pmm_alloc(&nx_pmm);
    if (p)
        tables_allocated++;
    return p;
}

static void *env_virt(void *ctx, uint64_t phys)
{
    (void)ctx;
    return nx_phys_to_virt(phys);
}

static void env_free(void *ctx, uint64_t phys)
{
    (void)ctx;
    nx_page_free(phys);
    tables_allocated--;
}

static const struct nx_pt_env ENV = {env_alloc, env_virt, env_free, 0};

const struct nx_pt_env *nx_vmm_env(void)
{
    return &ENV;
}

void nx_pmm_setup(const struct nx_mem_region *rg, uint32_t n)
{
    uint64_t npages = nx_pmm_span_pages(rg, n);
    uint64_t bytes = nx_pmm_storage_bytes(npages);
    uint64_t phys = nx_pmm_find_storage(rg, n, bytes);
    if (!phys)
        nx_panic("pmm: no room for %" NX_PRIu64 " bytes of bitmaps", bytes);
    int st = nx_pmm_init(&nx_pmm, rg, n, nx_phys_to_virt(phys), phys, bytes);
    if (st != NX_PMM_OK)
        nx_panic("pmm: init failed: %s", nx_pmm_strerror(st));
    nx_printf("NANOX: pmm ok span_pages=%" NX_PRIu64 " managed=%" NX_PRIu64 " free=%" NX_PRIu64
              " bitmaps=0x%" NX_PRIx64 "+%" NX_PRIu64 "\n",
              nx_pmm.npages, nx_pmm.managed_pages, nx_pmm.free_pages, phys, bytes);
}

uint64_t nx_page_alloc(void)
{
    uint64_t p = nx_pmm_alloc(&nx_pmm);
    if (!p)
        nx_panic("pmm: out of memory");
    return p;
}

void nx_page_free(uint64_t phys)
{
    int st = nx_pmm_free(&nx_pmm, phys);
    if (st != NX_PMM_OK)
        nx_panic("pmm: free of page 0x%" NX_PRIx64 " rejected: %s", phys, nx_pmm_strerror(st));
}

static void map_or_panic(uint64_t va, uint64_t pa, uint64_t len, uint64_t flags, int allow_2m,
                         const char *what)
{
    int st = nx_pt_map_range(&ENV, nx_kernel_root, va, pa, len, flags, allow_2m);
    if (st != NX_PT_OK)
        nx_panic("vmm: mapping %s failed: %s", what, nx_pt_strerror(st));
}

static uint64_t addr(const char *sym)
{
    return (uint64_t)(uintptr_t)sym;
}

/* Expected state of an address after the switch; panics on mismatch. */
static void expect_mapping(uint64_t va, int mapped, uint64_t must_set, uint64_t must_clear,
                           const char *what)
{
    uint64_t pa, flags, size;
    int st = nx_pt_query(&ENV, nx_kernel_root, va, &pa, &flags, &size);
    if (!mapped) {
        if (st != NX_PT_E_NOT_MAPPED)
            nx_panic("vmm: %s at 0x%" NX_PRIx64 " must not be mapped", what, va);
        return;
    }
    if (st != NX_PT_OK || (flags & must_set) != must_set || (flags & must_clear))
        nx_panic("vmm: %s at 0x%" NX_PRIx64 " has wrong mapping (st=%s flags=0x%" NX_PRIx64 ")",
                 what, va, nx_pt_strerror(st), flags);
}

static int physmap_type(uint32_t t)
{
    return t == NX_MEM_USABLE || t == NX_MEM_BOOT_RECLAIMABLE || t == NX_MEM_KERNEL_STACK ||
           t == NX_MEM_BOOT_INFO || t == NX_MEM_ACPI_RECLAIMABLE || t == NX_MEM_ACPI_NVS ||
           t == NX_MEM_FIRMWARE_RUNTIME || t == NX_MEM_INITRD;
}

void nx_vmm_init(const struct nx_mem_region *rg, uint32_t n)
{
    uint32_t r[4];
    nx_cpuid(0x80000000u, 0, r);
    uint32_t max_ext = r[0];
    nx_cpuid(0x80000001u, 0, r);
    if (max_ext < 0x80000001u || !(r[3] & (1u << 20)))
        nx_panic("vmm: CPU does not support NX (execute disable)");
    nx_wrmsr(NX_MSR_EFER, nx_rdmsr(NX_MSR_EFER) | NX_EFER_NXE);

    uint64_t root;
    int st = nx_pt_new_root(&ENV, &root);
    if (st != NX_PT_OK)
        nx_panic("vmm: no page for the root table");
    nx_kernel_root = root;

    const uint64_t G = NX_PTE_G, W = NX_PTE_W, X_OFF = NX_PTE_NX;
    map_or_panic(addr(__text_start), addr(__text_start), addr(__text_end) - addr(__text_start), G,
                 0, ".text");
    map_or_panic(addr(__rodata_start), addr(__rodata_start),
                 addr(__rodata_end) - addr(__rodata_start), G | X_OFF, 0, ".rodata");
    map_or_panic(addr(__data_start), addr(__data_start), addr(__data_end) - addr(__data_start),
                 G | W | X_OFF, 0, ".data/.bss");
    const char *const stacks[][2] = {{__boot_stack_bottom, __boot_stack_top},
                                     {__ist_df_bottom, __ist_df_top},
                                     {__ist_nmi_bottom, __ist_nmi_top},
                                     {__ist_mc_bottom, __ist_mc_top}};
    for (unsigned i = 0; i < 4; i++)
        map_or_panic(addr(stacks[i][0]), addr(stacks[i][0]),
                     addr(stacks[i][1]) - addr(stacks[i][0]), G | W | X_OFF, 0, "stack");

    /* Every higher-half PML4 slot gets its PDPT now, so address spaces
     * created later share all kernel mappings by copying slots 256..511. */
    uint64_t *pml4 = nx_phys_to_virt(root);
    for (unsigned i = 256; i < 512; i++) {
        uint64_t pdpt = env_alloc(0);
        if (!pdpt)
            nx_panic("vmm: no page for kernel PDPT %u", i);
        uint64_t *t = nx_phys_to_virt(pdpt);
        for (unsigned j = 0; j < 512; j++)
            t[j] = 0;
        pml4[i] = pdpt | NX_PTE_P | NX_PTE_W;
    }

    uint64_t ram = 0;
    for (uint32_t i = 0; i < n; i++) {
        if (!physmap_type(rg[i].type))
            continue;
        map_or_panic(NX_PHYSMAP_BASE + rg[i].base, rg[i].base, rg[i].length, G | W | X_OFF, 1,
                     "physmap");
        ram += rg[i].length;
    }

    /* Switch: from here on the UEFI identity mapping is gone. */
    nx_write_cr3(root);
    nx_write_cr0(nx_read_cr0() | NX_CR0_WP);
    nx_write_cr4(nx_read_cr4() | NX_CR4_PGE);
    nx_phys_offset = NX_PHYSMAP_BASE;
    nx_pmm_relocate(&nx_pmm, nx_phys_to_virt((uint64_t)(uintptr_t)nx_pmm.used));

    /* Verify the permissions actually installed. */
    expect_mapping(addr(__text_start), 1, 0, NX_PTE_W | NX_PTE_NX, ".text");
    expect_mapping(addr(__rodata_start), 1, NX_PTE_NX, NX_PTE_W, ".rodata");
    expect_mapping(addr(__data_start), 1, NX_PTE_W | NX_PTE_NX, 0, ".data");
    expect_mapping(addr(__boot_stack_guard), 0, 0, 0, "boot stack guard");
    expect_mapping(addr(__ist_df_guard), 0, 0, 0, "#DF stack guard");
    expect_mapping(addr(__ist_nmi_guard), 0, 0, 0, "NMI stack guard");
    expect_mapping(addr(__ist_mc_guard), 0, 0, 0, "#MC stack guard");
    expect_mapping(0, 0, 0, 0, "null page");
    expect_mapping(0xDEAD0000ull, 0, 0, 0, "low half");
    expect_mapping(NX_PHYSMAP_BASE + addr(__kernel_start), 0, 0, 0, "kernel image alias");

    nx_printf("NANOX: vmm ok root=0x%" NX_PRIx64 " physmap=0x%016" NX_PRIx64
              " ram_mapped=%" NX_PRIu64 "KiB tables=%" NX_PRIu64
              " text=r-x rodata=r-- data=rw- guards=4 null=unmapped\n",
              root, NX_PHYSMAP_BASE, ram / 1024, tables_allocated);
}

int nx_vmm_map_page(uint64_t va, uint64_t pa, uint64_t flags)
{
    int st = nx_pt_map(&ENV, nx_kernel_root, va, pa, NX_PAGE_4K, flags);
    if (st == NX_PT_OK)
        nx_invlpg(va);
    return st;
}

int nx_vmm_unmap_page(uint64_t va, uint64_t *pa)
{
    uint64_t size;
    int st = nx_pt_unmap(&ENV, nx_kernel_root, va, pa, &size);
    if (st == NX_PT_OK)
        nx_invlpg(va);
    return st;
}

int nx_vmm_query(uint64_t va, uint64_t *pa, uint64_t *flags, uint64_t *size)
{
    return nx_pt_query(&ENV, nx_kernel_root, va, pa, flags, size);
}

int nx_vmm_map_page_in(uint64_t root, uint64_t va, uint64_t pa, uint64_t flags)
{
    int st = nx_pt_map(&ENV, root, va, pa, NX_PAGE_4K, flags);
    if (st == NX_PT_OK && root == nx_read_cr3())
        nx_invlpg(va);
    return st;
}

int nx_vmm_query_in(uint64_t root, uint64_t va, uint64_t *pa, uint64_t *flags, uint64_t *size)
{
    return nx_pt_query(&ENV, root, va, pa, flags, size);
}

uint64_t nx_as_create(void)
{
    uint64_t root;
    if (nx_pt_new_root(&ENV, &root) != NX_PT_OK)
        return 0;
    uint64_t *dst = nx_phys_to_virt(root);
    const uint64_t *src = nx_phys_to_virt(nx_kernel_root);
    dst[0] = src[0]; /* kernel image, supervisor-only */
    for (unsigned i = 256; i < 512; i++)
        dst[i] = src[i];
    return root;
}

void nx_as_destroy(uint64_t root, void (*leaf)(void *ctx, uint64_t pa, uint64_t flags, uint64_t size),
                   void *ctx)
{
    nx_pt_destroy_slots(&ENV, root, NX_AS_USER_SLOT_FIRST, NX_AS_USER_SLOT_LAST, leaf, ctx);
    nx_page_free(root);
    tables_allocated--;
}

uint64_t nx_vmm_tables(void)
{
    return tables_allocated;
}

void *nx_vmm_map_mmio(uint64_t phys)
{
    uint64_t va = NX_PHYSMAP_BASE + phys, pa, flags, size;
    if (nx_vmm_query(va, &pa, &flags, &size) == NX_PT_OK)
        return (void *)(uintptr_t)va;
    int st = nx_vmm_map_page(va, phys, NX_PTE_W | NX_PTE_NX | NX_PTE_G | NX_PTE_PCD | NX_PTE_PWT);
    if (st != NX_PT_OK)
        nx_panic("vmm: mapping MMIO 0x%" NX_PRIx64 " failed: %s", phys, nx_pt_strerror(st));
    return (void *)(uintptr_t)va;
}

void nx_vmm_describe(const char *prefix, uint64_t va)
{
    if (!nx_kernel_root) {
        nx_printf("%s va=0x%016" NX_PRIx64 " (firmware page tables)\n", prefix, va);
        return;
    }
    uint64_t pa, flags, size;
    int st = nx_vmm_query(va, &pa, &flags, &size);
    if (st != NX_PT_OK) {
        nx_printf("%s va=0x%016" NX_PRIx64 " %s\n", prefix, va,
                  st == NX_PT_E_NOT_MAPPED ? "not-mapped" : nx_pt_strerror(st));
        return;
    }
    nx_printf("%s va=0x%016" NX_PRIx64 " pa=0x%016" NX_PRIx64 " size=%" NX_PRIu64 "K perms=r%c%c\n",
              prefix, va, pa, size / 1024, (flags & NX_PTE_W) ? 'w' : '-',
              (flags & NX_PTE_NX) ? '-' : 'x');
}
