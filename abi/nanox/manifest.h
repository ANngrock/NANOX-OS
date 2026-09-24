/*
 * NANOX image manifest, version 1.
 *
 * Written by tools/image/mkimage.py to \NANOX\MANIFEST.BIN on the ESP and
 * checked by the UEFI loader before the kernel ELF is parsed.  The manifest
 * detects a missing, truncated or corrupted kernel file.  It is NOT an
 * authenticity mechanism: whoever can rewrite KERNEL.ELF can rewrite the
 * manifest too (no signature in M0).  Layout: docs/boot-info.md.
 */
#ifndef NANOX_ABI_MANIFEST_H
#define NANOX_ABI_MANIFEST_H

#include <stddef.h>
#include <stdint.h>

/* "NXMF" in little-endian byte order. */
#define NX_MANIFEST_MAGIC 0x464D584Eu
#define NX_MANIFEST_VERSION 1u
#define NX_MANIFEST_SIZE 64u

struct nx_manifest {
    uint32_t magic;            /*  0 NX_MANIFEST_MAGIC */
    uint16_t version;          /*  4 NX_MANIFEST_VERSION */
    uint16_t size;             /*  6 NX_MANIFEST_SIZE */
    uint64_t kernel_size;      /*  8 exact byte size of KERNEL.ELF */
    uint8_t kernel_sha256[32]; /* 16 SHA-256 of KERNEL.ELF */
    uint8_t reserved[16];      /* 48 must be 0 */
};                             /* 64 */

_Static_assert(offsetof(struct nx_manifest, kernel_size) == 8, "manifest.kernel_size");
_Static_assert(offsetof(struct nx_manifest, kernel_sha256) == 16, "manifest.kernel_sha256");
_Static_assert(sizeof(struct nx_manifest) == NX_MANIFEST_SIZE, "manifest size");

#endif /* NANOX_ABI_MANIFEST_H */
