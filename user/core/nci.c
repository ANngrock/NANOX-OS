#include "nci.h"

static int is_id_char(char c)
{
    return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') ||
           c == '.' || c == '_' || c == '-';
}

static int is_op_char(char c)
{
    return (c >= 'a' && c <= 'z') || c == '.';
}

static int is_key_char(char c)
{
    return (c >= 'a' && c <= 'z') || c == '_';
}

static int is_val_char(char c)
{
    return is_id_char(c) || c == '/' || c == ':' || c == ',';
}

int nci_streq(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

static void copy_tok(char *dst, const char *src, uint32_t n)
{
    for (uint32_t i = 0; i < n; i++)
        dst[i] = src[i];
    dst[n] = 0;
}

/* Next space-separated token of line[*pos..len). */
static int next_tok(const char *line, uint32_t len, uint32_t *pos, uint32_t *start,
                    uint32_t *tlen)
{
    uint32_t i = *pos;
    while (i < len && line[i] == ' ')
        i++;
    if (i == len)
        return 0;
    *start = i;
    while (i < len && line[i] != ' ')
        i++;
    *tlen = i - *start;
    *pos = i;
    return 1;
}

int nci_parse(const char *line, uint32_t len, struct nci_req *out)
{
    out->id[0] = '-';
    out->id[1] = 0;
    out->op[0] = 0;
    out->nkv = 0;
    if (len && line[len - 1] == '\r')
        len--;
    if (len > NCI_LINE_MAX)
        return NCI_E_TOO_LONG;
    uint32_t pos = 0, s, n;
    if (!next_tok(line, len, &pos, &s, &n) || n != 3 || line[s] != 'R' || line[s + 1] != 'E' ||
        line[s + 2] != 'Q')
        return NCI_E_SYNTAX;
    if (!next_tok(line, len, &pos, &s, &n))
        return NCI_E_SYNTAX;
    if (n > NCI_ID_MAX)
        return NCI_E_ID;
    for (uint32_t i = 0; i < n; i++)
        if (!is_id_char(line[s + i]))
            return NCI_E_ID;
    copy_tok(out->id, line + s, n);
    if (!next_tok(line, len, &pos, &s, &n))
        return NCI_E_SYNTAX;
    if (n > NCI_OP_MAX || line[s] == '.' || line[s + n - 1] == '.')
        return NCI_E_OP;
    for (uint32_t i = 0; i < n; i++)
        if (!is_op_char(line[s + i]))
            return NCI_E_OP;
    copy_tok(out->op, line + s, n);
    while (next_tok(line, len, &pos, &s, &n)) {
        uint32_t eq = 0;
        while (eq < n && line[s + eq] != '=')
            eq++;
        if (eq == 0 || eq > NCI_KEY_MAX || eq + 1 >= n || n - eq - 1 > NCI_VAL_MAX)
            return NCI_E_KV;
        for (uint32_t i = 0; i < eq; i++)
            if (!is_key_char(line[s + i]))
                return NCI_E_KV;
        for (uint32_t i = eq + 1; i < n; i++)
            if (!is_val_char(line[s + i]))
                return NCI_E_KV;
        if (out->nkv == NCI_KV_MAX)
            return NCI_E_TOO_MANY;
        struct nci_kv *kv = &out->kv[out->nkv];
        copy_tok(kv->key, line + s, eq);
        copy_tok(kv->val, line + s + eq + 1, n - eq - 1);
        for (uint32_t i = 0; i < out->nkv; i++)
            if (nci_streq(out->kv[i].key, kv->key))
                return NCI_E_DUPKEY;
        out->nkv++;
    }
    return NCI_OK;
}

const char *nci_strerror(int status)
{
    switch (status) {
    case NCI_OK: return "ok";
    case NCI_E_SYNTAX: return "syntax";
    case NCI_E_TOO_LONG: return "too_long";
    case NCI_E_ID: return "bad_id";
    case NCI_E_OP: return "bad_operation";
    case NCI_E_KV: return "bad_argument";
    case NCI_E_DUPKEY: return "duplicate_argument";
    case NCI_E_TOO_MANY: return "too_many_arguments";
    default: return "unknown";
    }
}

const char *nci_get(const struct nci_req *r, const char *key)
{
    for (uint32_t i = 0; i < r->nkv; i++)
        if (nci_streq(r->kv[i].key, key))
            return r->kv[i].val;
    return 0;
}

static int parse_dec(const char *s, uint64_t *out)
{
    uint64_t v = 0;
    if (!*s)
        return 0;
    for (; *s; s++) {
        if (*s < '0' || *s > '9')
            return 0;
        uint64_t d = (uint64_t)(*s - '0');
        if (v > (UINT64_MAX - d) / 10)
            return 0;
        v = v * 10 + d;
    }
    *out = v;
    return 1;
}

int nci_get_u64(const struct nci_req *r, const char *key, uint64_t *out)
{
    const char *v = nci_get(r, key);
    if (!v)
        return 0;
    return parse_dec(v, out) ? 1 : -1;
}

static uint64_t fnv(uint64_t h, const char *s)
{
    for (; *s; s++) {
        h ^= (uint8_t)*s;
        h *= 0x100000001B3ull;
    }
    h ^= 0xFF; /* separator: "a"+"bc" differs from "ab"+"c" */
    h *= 0x100000001B3ull;
    return h;
}

uint64_t nci_fingerprint(const struct nci_req *r)
{
    uint64_t h = fnv(0xCBF29CE484222325ull, r->op);
    for (uint32_t i = 0; i < r->nkv; i++)
        h = fnv(fnv(h, r->kv[i].key), r->kv[i].val);
    return h;
}

static int hexval(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    return -1;
}

int nci_ref_parse(const char *s, uint64_t *boot_id, uint32_t *task_id)
{
    if (s[0] != 't' || s[1] != 'a' || s[2] != 's' || s[3] != 'k' || s[4] != '/')
        return 0;
    s += 5;
    uint64_t b = 0;
    for (int i = 0; i < 16; i++) {
        int d = hexval(s[i]);
        if (d < 0)
            return 0;
        b = b << 4 | (uint64_t)d;
    }
    if (s[16] != '/')
        return 0;
    uint64_t id;
    if (!parse_dec(s + 17, &id) || id == 0 || id > UINT32_MAX)
        return 0;
    *boot_id = b;
    *task_id = (uint32_t)id;
    return 1;
}

void nb_init(struct nci_buf *b, char *mem, uint32_t cap)
{
    b->p = mem;
    b->cap = cap;
    b->len = 0;
    b->overflow = 0;
    if (cap)
        mem[0] = 0;
}

void nb_char(struct nci_buf *b, char c)
{
    if (b->len + 1 < b->cap) {
        b->p[b->len++] = c;
        b->p[b->len] = 0;
    } else {
        b->overflow = 1;
    }
}

void nb_str(struct nci_buf *b, const char *s)
{
    while (*s)
        nb_char(b, *s++);
}

void nb_u64(struct nci_buf *b, uint64_t v)
{
    char tmp[24];
    int n = 0;
    do {
        tmp[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    while (n)
        nb_char(b, tmp[--n]);
}

void nb_i64(struct nci_buf *b, int64_t v)
{
    if (v < 0) {
        nb_char(b, '-');
        nb_u64(b, 0 - (uint64_t)v);
    } else {
        nb_u64(b, (uint64_t)v);
    }
}

void nb_hex(struct nci_buf *b, uint64_t v, unsigned width)
{
    char tmp[16];
    unsigned n = 0;
    do {
        tmp[n++] = "0123456789abcdef"[v & 15];
        v >>= 4;
    } while (v && n < 16);
    for (unsigned i = n; i < width; i++)
        nb_char(b, '0');
    while (n)
        nb_char(b, tmp[--n]);
}

void nb_kv(struct nci_buf *b, const char *key, const char *val)
{
    nb_char(b, ' ');
    nb_str(b, key);
    nb_char(b, '=');
    nb_str(b, val);
}

void nb_kv_u64(struct nci_buf *b, const char *key, uint64_t v)
{
    nb_char(b, ' ');
    nb_str(b, key);
    nb_char(b, '=');
    nb_u64(b, v);
}

void nb_kv_i64(struct nci_buf *b, const char *key, int64_t v)
{
    nb_char(b, ' ');
    nb_str(b, key);
    nb_char(b, '=');
    nb_i64(b, v);
}

void nci_ref_format(char out[NCI_REF_MAX], uint64_t boot_id, uint32_t task_id)
{
    struct nci_buf b;
    nb_init(&b, out, NCI_REF_MAX);
    nb_str(&b, "task/");
    nb_hex(&b, boot_id, 16);
    nb_char(&b, '/');
    nb_u64(&b, task_id);
}

int nci_value_ok(const char *s)
{
    uint32_t n = 0;
    for (; s[n]; n++)
        if (!is_val_char(s[n]) || n >= NCI_VAL_MAX)
            return 0;
    return n > 0;
}
