#include "initramfs.h"

static const char TRAILER[] = "TRAILER!!!";

static int parse_hex8(const uint8_t *p, uint32_t *out)
{
    uint32_t v = 0;
    for (int i = 0; i < 8; i++) {
        uint8_t c = p[i];
        uint32_t d;
        if (c >= '0' && c <= '9')
            d = c - '0';
        else if (c >= 'a' && c <= 'f')
            d = c - 'a' + 10;
        else if (c >= 'A' && c <= 'F')
            d = c - 'A' + 10;
        else
            return 0;
        v = v << 4 | d;
    }
    *out = v;
    return 1;
}

static uint64_t align4(uint64_t v)
{
    return (v + 3) & ~(uint64_t)3;
}

static int name_eq(const char *a, uint32_t alen, const char *b)
{
    uint32_t i = 0;
    for (; i < alen; i++)
        if (b[i] == '\0' || a[i] != b[i])
            return 0;
    return b[i] == '\0';
}

int nx_cpio_next(const uint8_t *base, uint64_t len, uint64_t *offset, struct nx_cpio_entry *e)
{
    uint64_t off = *offset;
    if (off > len || len - off < NX_CPIO_HEADER_SIZE)
        return NX_CPIO_E_TRUNCATED;
    const uint8_t *h = base + off;
    static const char magic[6] = {'0', '7', '0', '7', '0', '1'};
    for (int i = 0; i < 6; i++)
        if (h[i] != (uint8_t)magic[i])
            return NX_CPIO_E_MAGIC;
    /* Fields after the magic: ino mode uid gid nlink mtime filesize devmajor
     * devminor rdevmajor rdevminor namesize check. */
    uint32_t f[13];
    for (int i = 0; i < 13; i++)
        if (!parse_hex8(h + 6 + 8 * i, &f[i]))
            return NX_CPIO_E_HEX;
    uint32_t mode = f[1], filesize = f[6], namesize = f[11];
    if (namesize < 2 || namesize > NX_CPIO_NAME_MAX)
        return NX_CPIO_E_NAME;
    uint64_t name_off = off + NX_CPIO_HEADER_SIZE;
    if (len - name_off < namesize)
        return NX_CPIO_E_TRUNCATED;
    const char *name = (const char *)(base + name_off);
    if (name[namesize - 1] != '\0' || name[0] == '/')
        return NX_CPIO_E_NAME;
    for (uint32_t i = 0; i + 1 < namesize; i++)
        if (name[i] == '\0')
            return NX_CPIO_E_NAME;
    uint64_t data_off = align4(name_off + namesize);
    if (data_off > len || len - data_off < filesize)
        return NX_CPIO_E_BOUNDS;

    e->name = name;
    e->name_len = namesize - 1;
    e->mode = mode;
    e->data = base + data_off;
    e->size = filesize;
    *offset = align4(data_off + filesize);
    if (name_eq(name, namesize - 1, TRAILER))
        return NX_CPIO_END;
    uint32_t type = mode & NX_CPIO_MODE_TYPE;
    if (type != NX_CPIO_MODE_REG && type != NX_CPIO_MODE_DIR)
        return NX_CPIO_E_TYPE;
    return NX_CPIO_OK;
}

int nx_cpio_validate(const uint8_t *base, uint64_t len, uint32_t *entries, uint64_t *bad_offset)
{
    uint64_t off = 0;
    *entries = 0;
    for (;;) {
        struct nx_cpio_entry e;
        uint64_t here = off;
        if (off >= len) {
            *bad_offset = off;
            return NX_CPIO_E_NO_TRAILER;
        }
        int st = nx_cpio_next(base, len, &off, &e);
        if (st == NX_CPIO_END)
            return NX_CPIO_OK;
        if (st != NX_CPIO_OK) {
            *bad_offset = here;
            return st;
        }
        (*entries)++;
    }
}

int nx_cpio_find(const uint8_t *base, uint64_t len, const char *name, struct nx_cpio_entry *e)
{
    uint64_t off = 0;
    while (off < len) {
        int st = nx_cpio_next(base, len, &off, e);
        if (st == NX_CPIO_END)
            break;
        if (st != NX_CPIO_OK)
            return st;
        if (name_eq(e->name, e->name_len, name))
            return NX_CPIO_OK;
    }
    return NX_CPIO_E_NOT_FOUND;
}

const char *nx_cpio_strerror(int st)
{
    static const char *const names[NX_CPIO_E__COUNT] = {
        [NX_CPIO_OK] = "ok",
        [NX_CPIO_END] = "end",
        [NX_CPIO_E_TRUNCATED] = "E_TRUNCATED",
        [NX_CPIO_E_MAGIC] = "E_MAGIC",
        [NX_CPIO_E_HEX] = "E_HEX",
        [NX_CPIO_E_NAME] = "E_NAME",
        [NX_CPIO_E_BOUNDS] = "E_BOUNDS",
        [NX_CPIO_E_TYPE] = "E_TYPE",
        [NX_CPIO_E_NO_TRAILER] = "E_NO_TRAILER",
        [NX_CPIO_E_NOT_FOUND] = "E_NOT_FOUND",
    };
    if (st < 0 || st >= NX_CPIO_E__COUNT)
        return "E_UNKNOWN";
    return names[st];
}
