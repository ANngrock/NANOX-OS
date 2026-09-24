/*
 * initramfs reader: cpio "newc" archives (magic 070701), as produced by
 * tools/image/mkinitrd.py.  Pure code over a byte buffer; compiled into the
 * kernel and into tests/host.  Format and rules: docs/m1-kernel.md.
 */
#ifndef NANOX_KERNEL_INITRAMFS_H
#define NANOX_KERNEL_INITRAMFS_H

#include <stdint.h>

#define NX_CPIO_HEADER_SIZE 110u
#define NX_CPIO_NAME_MAX 4096u
#define NX_CPIO_MODE_TYPE 0170000u
#define NX_CPIO_MODE_DIR 0040000u
#define NX_CPIO_MODE_REG 0100000u

enum nx_cpio_status {
    NX_CPIO_OK = 0,       /* entry returned */
    NX_CPIO_END,          /* TRAILER!!! reached */
    NX_CPIO_E_TRUNCATED,  /* header or name runs past the end */
    NX_CPIO_E_MAGIC,      /* not "070701" */
    NX_CPIO_E_HEX,        /* non-hex digit in a header field */
    NX_CPIO_E_NAME,       /* namesize 0/1/too large, missing or embedded NUL, absolute */
    NX_CPIO_E_BOUNDS,     /* file data runs past the end */
    NX_CPIO_E_TYPE,       /* entry is neither a regular file nor a directory */
    NX_CPIO_E_NO_TRAILER, /* archive ends without TRAILER!!! */
    NX_CPIO_E_NOT_FOUND,  /* nx_cpio_find: no such entry */
    NX_CPIO_E__COUNT
};

struct nx_cpio_entry {
    const char *name; /* NUL-terminated, inside the archive */
    uint32_t name_len;
    uint32_t mode;
    const uint8_t *data;
    uint64_t size;
};

/* Reads the entry at *offset and advances it.  Returns NX_CPIO_OK,
 * NX_CPIO_END or an error; on error *offset points at the bad header. */
int nx_cpio_next(const uint8_t *base, uint64_t len, uint64_t *offset, struct nx_cpio_entry *e);
/* Walks the whole archive; *entries counts entries before the trailer. */
int nx_cpio_validate(const uint8_t *base, uint64_t len, uint32_t *entries, uint64_t *bad_offset);
/* Finds an entry by exact name. */
int nx_cpio_find(const uint8_t *base, uint64_t len, const char *name, struct nx_cpio_entry *e);
const char *nx_cpio_strerror(int status);

#endif
