#include "bootinfo_check.h"

#define PAGE_MASK ((uint64_t)NX_PAGE_SIZE - 1)
#define TYPE_BIT(t) (1u << (t))

static int fail(struct nx_bi_result *r, int err, uint32_t index)
{
    r->error = err;
    r->index = index;
    r->bi = 0;
    r->regions = 0;
    return err;
}

static int range_ok(uint64_t start, uint64_t len)
{
    return len != 0 && start <= UINT64_MAX - len;
}

/* True when every byte of [start, start+len) lies in consecutive regions of
 * type `type`.  Regions are already known to be sorted and disjoint. */
static int covered_by(const struct nx_mem_region *rg, uint32_t n, uint64_t start, uint64_t len,
                      uint32_t type)
{
    if (!range_ok(start, len))
        return 0;
    uint64_t cur = start, end = start + len;
    for (uint32_t i = 0; i < n; i++) {
        uint64_t r_end = rg[i].base + rg[i].length;
        if (r_end <= cur)
            continue;
        if (rg[i].base > cur || rg[i].type != type)
            return 0;
        cur = r_end;
        if (cur >= end)
            return 1;
    }
    return 0;
}

/* True when [start, start+len) intersects any region whose type is in `mask`. */
static int overlaps_types(const struct nx_mem_region *rg, uint32_t n, uint64_t start, uint64_t len,
                          uint32_t mask)
{
    uint64_t end = start + len;
    for (uint32_t i = 0; i < n; i++) {
        if ((mask & TYPE_BIT(rg[i].type)) && rg[i].base < end && start < rg[i].base + rg[i].length)
            return 1;
    }
    return 0;
}

static int checksum_ok(const uint8_t *p, uint32_t len)
{
    uint8_t sum = 0;
    for (uint32_t i = 0; i < len; i++)
        sum = (uint8_t)(sum + p[i]);
    return sum == 0;
}

static int check_rsdp(const struct nx_bi_check *c, const struct nx_boot_info *bi,
                      const struct nx_mem_region *rg, uint32_t n)
{
    if (!(bi->flags & NX_BI_HAS_ACPI_RSDP))
        return bi->acpi_rsdp_phys == 0;
    uint64_t p = bi->acpi_rsdp_phys;
    if (!range_ok(p, 20) || overlaps_types(rg, n, p, 20, TYPE_BIT(NX_MEM_USABLE)))
        return 0;
    const uint8_t *v1 = c->map(c->opaque, p, 20);
    static const char sig[8] = {'R', 'S', 'D', ' ', 'P', 'T', 'R', ' '};
    if (!v1)
        return 0;
    for (int i = 0; i < 8; i++)
        if (v1[i] != (uint8_t)sig[i])
            return 0;
    if (!checksum_ok(v1, 20))
        return 0;
    if (v1[15] >= 2) { /* ACPI 2.0+: extended structure */
        const uint8_t *v2 = c->map(c->opaque, p, 36);
        if (!v2)
            return 0;
        uint32_t length = (uint32_t)v2[20] | (uint32_t)v2[21] << 8 | (uint32_t)v2[22] << 16 |
                          (uint32_t)v2[23] << 24;
        if (length < 36 || length > 4096 || !range_ok(p, length) ||
            overlaps_types(rg, n, p, length, TYPE_BIT(NX_MEM_USABLE)))
            return 0;
        const uint8_t *full = c->map(c->opaque, p, length);
        if (!full || !checksum_ok(full, length))
            return 0;
    }
    return 1;
}

static int check_framebuffer(const struct nx_boot_info *bi, const struct nx_mem_region *rg,
                             uint32_t n)
{
    if (!(bi->flags & NX_BI_HAS_FRAMEBUFFER))
        return bi->fb_phys == 0 && bi->fb_size == 0 && bi->fb_width == 0 && bi->fb_height == 0 &&
               bi->fb_pitch == 0 && bi->fb_format == NX_FB_NONE;
    if (bi->fb_width == 0 || bi->fb_height == 0 || bi->fb_phys == 0)
        return 0;
    if (bi->fb_format != NX_FB_RGBX8888 && bi->fb_format != NX_FB_BGRX8888)
        return 0;
    if ((uint64_t)bi->fb_pitch < (uint64_t)bi->fb_width * 4u)
        return 0;
    if (bi->fb_size < (uint64_t)bi->fb_pitch * bi->fb_height || !range_ok(bi->fb_phys, bi->fb_size))
        return 0;
    uint32_t ram = TYPE_BIT(NX_MEM_USABLE) | TYPE_BIT(NX_MEM_KERNEL_IMAGE) |
                   TYPE_BIT(NX_MEM_KERNEL_STACK) | TYPE_BIT(NX_MEM_BOOT_INFO) |
                   TYPE_BIT(NX_MEM_BOOT_RECLAIMABLE);
    return !overlaps_types(rg, n, bi->fb_phys, bi->fb_size, ram);
}

int nx_bootinfo_check(const struct nx_bi_check *c, struct nx_bi_result *r)
{
    r->usable_bytes = 0;
    if (c->bi_phys == 0 || !range_ok(c->bi_phys, NX_BOOTINFO_V1_SIZE))
        return fail(r, NX_BI_E_NULL, 0);
    const struct nx_boot_info *hdr = c->map(c->opaque, c->bi_phys, 16);
    if (!hdr)
        return fail(r, NX_BI_E_NULL, 0);
    if (hdr->magic != NX_BOOTINFO_MAGIC)
        return fail(r, NX_BI_E_MAGIC, 0);
    if (hdr->version_major != NX_BOOTINFO_VERSION_MAJOR)
        return fail(r, NX_BI_E_VERSION, 0);
    if (hdr->size < NX_BOOTINFO_V1_SIZE || hdr->size > NX_BOOTINFO_MAX_SIZE ||
        (hdr->version_minor <= NX_BOOTINFO_VERSION_MINOR &&
         hdr->size != sizeof(struct nx_boot_info)))
        return fail(r, NX_BI_E_SIZE, 0);
    const struct nx_boot_info *bi = c->map(c->opaque, c->bi_phys, hdr->size);
    if (!bi)
        return fail(r, NX_BI_E_NULL, 0);

    if (bi->version_minor <= NX_BOOTINFO_VERSION_MINOR &&
        (bi->flags & ~(uint64_t)NX_BI_KNOWN_FLAGS_V1_0))
        return fail(r, NX_BI_E_FLAGS, 0);
    if (bi->reserved0 != 0 || bi->initrd_phys != 0 || bi->initrd_size != 0)
        return fail(r, NX_BI_E_RESERVED, 0);

    /* ---- memory map ---- */
    if (bi->mmap_entry_size != sizeof(struct nx_mem_region))
        return fail(r, NX_BI_E_MMAP_ENTRY_SIZE, 0);
    uint32_t n = bi->mmap_count;
    if (n == 0 || n > NX_MMAP_MAX_ENTRIES)
        return fail(r, NX_BI_E_MMAP_COUNT, 0);
    uint64_t mmap_bytes = (uint64_t)n * sizeof(struct nx_mem_region);
    if (bi->mmap_phys == 0 || (bi->mmap_phys & 7) || !range_ok(bi->mmap_phys, mmap_bytes))
        return fail(r, NX_BI_E_MMAP_PTR, 0);
    const struct nx_mem_region *rg = c->map(c->opaque, bi->mmap_phys, mmap_bytes);
    if (!rg)
        return fail(r, NX_BI_E_MMAP_PTR, 0);

    uint64_t usable = 0;
    for (uint32_t i = 0; i < n; i++) {
        const struct nx_mem_region *e = &rg[i];
        if (e->length == 0)
            return fail(r, NX_BI_E_MMAP_EMPTY, i);
        if ((e->base & PAGE_MASK) || (e->length & PAGE_MASK))
            return fail(r, NX_BI_E_MMAP_ALIGN, i);
        if (e->length > UINT64_MAX - e->base)
            return fail(r, NX_BI_E_MMAP_OVERFLOW, i);
        if (e->type < NX_MEM_TYPE_MIN || e->type > NX_MEM_TYPE_MAX)
            return fail(r, NX_BI_E_MMAP_TYPE, i);
        if (e->flags != 0)
            return fail(r, NX_BI_E_MMAP_FLAGS, i);
        if (i > 0) {
            const struct nx_mem_region *p = &rg[i - 1];
            if (e->base < p->base)
                return fail(r, NX_BI_E_MMAP_ORDER, i);
            if (e->base < p->base + p->length)
                return fail(r, NX_BI_E_MMAP_OVERLAP, i);
        }
        if (e->type == NX_MEM_USABLE)
            usable += e->length;
    }
    if (usable == 0)
        return fail(r, NX_BI_E_MMAP_NO_USABLE, 0);

    /* ---- kernel image ---- */
    if ((bi->kernel_phys_base & PAGE_MASK) || (bi->kernel_phys_size & PAGE_MASK) ||
        !covered_by(rg, n, bi->kernel_phys_base, bi->kernel_phys_size, NX_MEM_KERNEL_IMAGE))
        return fail(r, NX_BI_E_KERNEL_RANGE, 0);
    uint64_t k_end = bi->kernel_phys_base + bi->kernel_phys_size;
    if (bi->kernel_entry < bi->kernel_phys_base || bi->kernel_entry >= k_end)
        return fail(r, NX_BI_E_KERNEL_ENTRY, 0);
    if ((c->image_start || c->image_end) &&
        (c->image_start >= c->image_end || c->image_start < bi->kernel_phys_base ||
         c->image_end > k_end))
        return fail(r, NX_BI_E_KERNEL_SELF, 0);

    /* ---- stack ---- */
    if ((bi->stack_phys_base & PAGE_MASK) || (bi->stack_size & PAGE_MASK) ||
        !covered_by(rg, n, bi->stack_phys_base, bi->stack_size, NX_MEM_KERNEL_STACK))
        return fail(r, NX_BI_E_STACK_RANGE, 0);

    /* ---- boot info, memory map array, command line ---- */
    if (!covered_by(rg, n, c->bi_phys, bi->size, NX_MEM_BOOT_INFO) ||
        !covered_by(rg, n, bi->mmap_phys, mmap_bytes, NX_MEM_BOOT_INFO))
        return fail(r, NX_BI_E_BOOTINFO_RANGE, 0);
    if (bi->cmdline_len > NX_CMDLINE_MAX ||
        !covered_by(rg, n, bi->cmdline_phys, (uint64_t)bi->cmdline_len + 1, NX_MEM_BOOT_INFO))
        return fail(r, NX_BI_E_CMDLINE, 0);
    const char *cl = c->map(c->opaque, bi->cmdline_phys, (uint64_t)bi->cmdline_len + 1);
    if (!cl || cl[bi->cmdline_len] != '\0')
        return fail(r, NX_BI_E_CMDLINE, 0);
    for (uint32_t i = 0; i < bi->cmdline_len; i++)
        if (cl[i] < 0x20 || cl[i] > 0x7E)
            return fail(r, NX_BI_E_CMDLINE, 0);

    /* ---- optional platform data ---- */
    if (!check_rsdp(c, bi, rg, n))
        return fail(r, NX_BI_E_RSDP, 0);
    if (!check_framebuffer(bi, rg, n))
        return fail(r, NX_BI_E_FRAMEBUFFER, 0);

    r->error = NX_BI_OK;
    r->index = 0;
    r->bi = bi;
    r->regions = rg;
    r->usable_bytes = usable;
    return NX_BI_OK;
}

const char *nx_bi_strerror(int e)
{
    static const char *const names[NX_BI_E__COUNT] = {
        [NX_BI_OK] = "ok",
        [NX_BI_E_NULL] = "E_NULL",
        [NX_BI_E_MAGIC] = "E_MAGIC",
        [NX_BI_E_VERSION] = "E_VERSION",
        [NX_BI_E_SIZE] = "E_SIZE",
        [NX_BI_E_FLAGS] = "E_FLAGS",
        [NX_BI_E_RESERVED] = "E_RESERVED",
        [NX_BI_E_MMAP_PTR] = "E_MMAP_PTR",
        [NX_BI_E_MMAP_ENTRY_SIZE] = "E_MMAP_ENTRY_SIZE",
        [NX_BI_E_MMAP_COUNT] = "E_MMAP_COUNT",
        [NX_BI_E_MMAP_EMPTY] = "E_MMAP_EMPTY",
        [NX_BI_E_MMAP_ALIGN] = "E_MMAP_ALIGN",
        [NX_BI_E_MMAP_OVERFLOW] = "E_MMAP_OVERFLOW",
        [NX_BI_E_MMAP_TYPE] = "E_MMAP_TYPE",
        [NX_BI_E_MMAP_FLAGS] = "E_MMAP_FLAGS",
        [NX_BI_E_MMAP_ORDER] = "E_MMAP_ORDER",
        [NX_BI_E_MMAP_OVERLAP] = "E_MMAP_OVERLAP",
        [NX_BI_E_MMAP_NO_USABLE] = "E_MMAP_NO_USABLE",
        [NX_BI_E_KERNEL_RANGE] = "E_KERNEL_RANGE",
        [NX_BI_E_KERNEL_ENTRY] = "E_KERNEL_ENTRY",
        [NX_BI_E_KERNEL_SELF] = "E_KERNEL_SELF",
        [NX_BI_E_STACK_RANGE] = "E_STACK_RANGE",
        [NX_BI_E_BOOTINFO_RANGE] = "E_BOOTINFO_RANGE",
        [NX_BI_E_CMDLINE] = "E_CMDLINE",
        [NX_BI_E_RSDP] = "E_RSDP",
        [NX_BI_E_FRAMEBUFFER] = "E_FRAMEBUFFER",
    };
    if (e < 0 || e >= NX_BI_E__COUNT)
        return "E_UNKNOWN";
    return names[e];
}
