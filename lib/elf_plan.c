#include <nanox/elf_plan.h>
#include <nanox/syscall.h>

#define EHDR_SIZE 64u
#define PHDR_SIZE 56u
#define PT_LOAD 1u
#define PF_X 1u
#define PF_W 2u
#define PAGE_MASK 0xFFFull

static uint16_t rd16(const uint8_t *p)
{
    return (uint16_t)(p[0] | p[1] << 8);
}
static uint32_t rd32(const uint8_t *p)
{
    return (uint32_t)rd16(p) | (uint32_t)rd16(p + 2) << 16;
}
static uint64_t rd64(const uint8_t *p)
{
    return (uint64_t)rd32(p) | (uint64_t)rd32(p + 4) << 32;
}

int nx_elf_plan(const uint8_t *img, uint64_t size, struct nx_elf_plan *plan)
{
    static const struct nx_elf_rules kernel = {NX_KERNEL_LOAD_MIN, NX_KERNEL_LOAD_MAX,
                                               NX_KERNEL_SPAN_MAX, 1, 0};
    return nx_elf_plan_rules(img, size, &kernel, plan);
}

int nx_elf_plan_user(const uint8_t *img, uint64_t size, struct nx_elf_plan *plan)
{
    /* The user half above PML4 slot 0, below the user stack and one unmapped
     * page under it; p_paddr is ignored; no writable+executable segment. */
    static const struct nx_elf_rules user = {
        NX_USER_BASE, NX_USER_STACK_TOP - (NX_USER_STACK_PAGES + 1) * 0x1000ull,
        NX_USER_SPAN_MAX, 0, 1};
    return nx_elf_plan_rules(img, size, &user, plan);
}

int nx_elf_plan_rules(const uint8_t *img, uint64_t size, const struct nx_elf_rules *rules,
                      struct nx_elf_plan *plan)
{
    plan->segment_count = 0;
    if (size < EHDR_SIZE)
        return NX_ELF_E_TRUNCATED;
    if (img[0] != 0x7F || img[1] != 'E' || img[2] != 'L' || img[3] != 'F' ||
        img[4] != 2 /* ELFCLASS64 */ || img[5] != 1 /* ELFDATA2LSB */ ||
        img[6] != 1 /* EV_CURRENT */)
        return NX_ELF_E_IDENT;
    if (rd16(img + 16) != 2 /* ET_EXEC */ || rd16(img + 18) != 62 /* EM_X86_64 */ ||
        rd32(img + 20) != 1)
        return NX_ELF_E_TYPE;

    uint64_t entry = rd64(img + 24);
    uint64_t phoff = rd64(img + 32);
    uint16_t ehsize = rd16(img + 52);
    uint16_t phentsize = rd16(img + 54);
    uint16_t phnum = rd16(img + 56);
    if (ehsize != EHDR_SIZE || phentsize != PHDR_SIZE || phnum == 0 || phnum > 64)
        return NX_ELF_E_PHDR;
    if (phoff > size || (uint64_t)phnum * PHDR_SIZE > size - phoff)
        return NX_ELF_E_PHDR;

    uint64_t prev_end = 0;
    for (uint16_t i = 0; i < phnum; i++) {
        const uint8_t *ph = img + phoff + (uint64_t)i * PHDR_SIZE;
        if (rd32(ph) != PT_LOAD)
            continue;
        uint32_t flags = rd32(ph + 4);
        uint64_t off = rd64(ph + 8), vaddr = rd64(ph + 16), paddr = rd64(ph + 24);
        uint64_t filesz = rd64(ph + 32), memsz = rd64(ph + 40);
        if (memsz == 0)
            continue; /* nothing to load */
        if (filesz > memsz || off > size || filesz > size - off)
            return NX_ELF_E_SEG_BOUNDS;
        if ((rules->require_identity && vaddr != paddr) || vaddr > UINT64_MAX - memsz ||
            vaddr < rules->min_addr || vaddr + memsz > rules->max_addr)
            return NX_ELF_E_SEG_ADDR;
        if (rules->forbid_wx && (flags & PF_W) && (flags & PF_X))
            return NX_ELF_E_WX;
        if (plan->segment_count && vaddr < prev_end)
            return NX_ELF_E_SEG_ORDER;
        if (plan->segment_count == NX_ELF_MAX_SEGMENTS)
            return NX_ELF_E_NO_LOAD;
        struct nx_elf_segment *s = &plan->segments[plan->segment_count++];
        s->file_offset = off;
        s->file_size = filesz;
        s->addr = vaddr;
        s->mem_size = memsz;
        s->flags = flags;
        prev_end = vaddr + memsz;
    }
    if (plan->segment_count == 0)
        return NX_ELF_E_NO_LOAD;

    int entry_ok = 0;
    for (uint32_t i = 0; i < plan->segment_count; i++) {
        const struct nx_elf_segment *s = &plan->segments[i];
        if ((s->flags & PF_X) && entry >= s->addr && entry - s->addr < s->mem_size)
            entry_ok = 1;
    }
    if (!entry_ok)
        return NX_ELF_E_ENTRY;

    /* prev_end <= rules->max_addr <= 2^64 - 4096 is required of callers. */
    plan->entry = entry;
    plan->span_base = plan->segments[0].addr & ~PAGE_MASK;
    plan->span_end = (prev_end + PAGE_MASK) & ~PAGE_MASK;
    if (plan->span_end - plan->span_base > rules->span_max)
        return NX_ELF_E_SPAN;
    return NX_ELF_OK;
}

const char *nx_elf_strerror(int err)
{
    switch (err) {
    case NX_ELF_OK: return "ok";
    case NX_ELF_E_TRUNCATED: return "truncated header";
    case NX_ELF_E_IDENT: return "bad ident (magic/class/data/version)";
    case NX_ELF_E_TYPE: return "not an x86-64 ET_EXEC";
    case NX_ELF_E_PHDR: return "bad program header table";
    case NX_ELF_E_SEG_BOUNDS: return "segment outside file or filesz > memsz";
    case NX_ELF_E_SEG_ADDR: return "segment address invalid or outside load window";
    case NX_ELF_E_SEG_ORDER: return "PT_LOAD segments unsorted or overlapping";
    case NX_ELF_E_NO_LOAD: return "no loadable segments or too many";
    case NX_ELF_E_ENTRY: return "entry point not in an executable segment";
    case NX_ELF_E_SPAN: return "span too large";
    case NX_ELF_E_WX: return "segment both writable and executable";
    default: return "unknown";
    }
}
