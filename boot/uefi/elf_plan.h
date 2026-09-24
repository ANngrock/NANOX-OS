/*
 * ELF64 kernel image checks and load plan.  Pure function without UEFI
 * dependencies so tests/host can exercise it with crafted images.
 */
#ifndef NANOX_BOOT_ELF_PLAN_H
#define NANOX_BOOT_ELF_PLAN_H

#include <stdint.h>

#define NX_ELF_MAX_SEGMENTS 16u
/* Physical window where the kernel span must lie (identity mapped in M0). */
#define NX_KERNEL_LOAD_MIN 0x00100000ull
#define NX_KERNEL_LOAD_MAX 0x40000000ull
/* Upper bound on the loaded span (sanity limit). */
#define NX_KERNEL_SPAN_MAX (64ull << 20)

struct nx_elf_segment {
    uint64_t file_offset;
    uint64_t file_size;
    uint64_t phys_addr;
    uint64_t mem_size;
    uint32_t flags; /* PF_X = 1, PF_W = 2, PF_R = 4 */
};

struct nx_elf_plan {
    uint64_t entry;
    uint64_t span_base; /* page aligned */
    uint64_t span_end;  /* page aligned, exclusive */
    uint32_t segment_count;
    struct nx_elf_segment segments[NX_ELF_MAX_SEGMENTS];
};

enum nx_elf_error {
    NX_ELF_OK = 0,
    NX_ELF_E_TRUNCATED,   /* file shorter than the ELF header */
    NX_ELF_E_IDENT,       /* magic / class / data / version */
    NX_ELF_E_TYPE,        /* not ET_EXEC for EM_X86_64 */
    NX_ELF_E_PHDR,        /* program header table size / bounds */
    NX_ELF_E_SEG_BOUNDS,  /* segment file range outside the file, filesz > memsz */
    NX_ELF_E_SEG_ADDR,    /* vaddr != paddr, overflow, outside the load window */
    NX_ELF_E_SEG_ORDER,   /* PT_LOAD not ascending or overlapping */
    NX_ELF_E_NO_LOAD,     /* no PT_LOAD or too many */
    NX_ELF_E_ENTRY,       /* entry not inside an executable segment */
    NX_ELF_E_SPAN,        /* span too large */
};

int nx_elf_plan(const uint8_t *image, uint64_t size, struct nx_elf_plan *plan);
const char *nx_elf_strerror(int err);

#endif
