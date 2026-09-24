/*
 * JSON writer and span reader of the M5 provider client (nanox/json.h).
 */
#include <nanox/json.h>

static uint32_t cstrlen(const char *s)
{
    uint32_t n = 0;
    while (s[n])
        n++;
    return n;
}

/* ---- writer ------------------------------------------------------------------------ */

void jw_init(struct jw *w, char *mem, uint32_t cap)
{
    w->p = mem;
    w->cap = cap;
    w->len = 0;
    w->overflow = 0;
    if (cap)
        mem[0] = 0;
}

void jw_raw(struct jw *w, const char *s, uint32_t n)
{
    if (w->overflow || n > w->cap - w->len || w->cap - w->len - n < 1) {
        w->overflow = 1;
        return;
    }
    for (uint32_t i = 0; i < n; i++)
        w->p[w->len + i] = s[i];
    w->len += n;
    w->p[w->len] = 0;
}

void jw_rawz(struct jw *w, const char *s)
{
    jw_raw(w, s, cstrlen(s));
}

/* Length of the valid UTF-8 sequence at s[0..n) (1..4), 0 if invalid. */
static uint32_t utf8_seq(const unsigned char *s, uint32_t n)
{
    unsigned char c = s[0];
    uint32_t len, cp;
    if (c < 0x80)
        return 1;
    if (c >= 0xC2 && c <= 0xDF) {
        len = 2;
        cp = c & 0x1F;
    } else if (c >= 0xE0 && c <= 0xEF) {
        len = 3;
        cp = c & 0x0F;
    } else if (c >= 0xF0 && c <= 0xF4) {
        len = 4;
        cp = c & 0x07;
    } else {
        return 0;
    }
    if (len > n)
        return 0;
    for (uint32_t i = 1; i < len; i++) {
        if ((s[i] & 0xC0) != 0x80)
            return 0;
        cp = (cp << 6) | (s[i] & 0x3F);
    }
    if ((len == 3 && cp < 0x800) || (len == 4 && (cp < 0x10000 || cp > 0x10FFFF)) ||
        (cp >= 0xD800 && cp <= 0xDFFF))
        return 0;
    return len;
}

void jw_str(struct jw *w, const char *s, uint32_t n)
{
    static const char hex[] = "0123456789abcdef";
    jw_raw(w, "\"", 1);
    uint32_t i = 0;
    while (i < n && !w->overflow) {
        unsigned char c = (unsigned char)s[i];
        if (c == '"' || c == '\\') {
            char e[2] = {'\\', (char)c};
            jw_raw(w, e, 2);
            i++;
        } else if (c == '\n') {
            jw_raw(w, "\\n", 2);
            i++;
        } else if (c == '\r') {
            jw_raw(w, "\\r", 2);
            i++;
        } else if (c == '\t') {
            jw_raw(w, "\\t", 2);
            i++;
        } else if (c < 0x20 || c == 0x7F) {
            char e[6] = {'\\', 'u', '0', '0', hex[c >> 4], hex[c & 15]};
            jw_raw(w, e, 6);
            i++;
        } else {
            uint32_t k = utf8_seq((const unsigned char *)s + i, n - i);
            if (k == 0) {
                jw_raw(w, "\\ufffd", 6);
                i++;
            } else {
                jw_raw(w, s + i, k);
                i += k;
            }
        }
    }
    jw_raw(w, "\"", 1);
}

void jw_strz(struct jw *w, const char *s)
{
    jw_str(w, s, cstrlen(s));
}

void jw_u64(struct jw *w, uint64_t v)
{
    char t[24];
    int n = 0;
    do {
        t[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    char r[24];
    for (int i = 0; i < n; i++)
        r[i] = t[n - 1 - i];
    jw_raw(w, r, (uint32_t)n);
}

/* ---- reader ------------------------------------------------------------------------ */

static int is_ws(char c)
{
    return c == ' ' || c == '\t' || c == '\n' || c == '\r';
}

static void ws(const char *s, uint32_t len, uint32_t *i)
{
    while (*i < len && is_ws(s[*i]))
        (*i)++;
}

static int hexval(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    if (c >= 'A' && c <= 'F')
        return c - 'A' + 10;
    return -1;
}

static int skip_string(const char *s, uint32_t len, uint32_t *i)
{
    if (*i >= len || s[*i] != '"')
        return 0;
    (*i)++;
    while (*i < len) {
        unsigned char c = (unsigned char)s[*i];
        if (c == '"') {
            (*i)++;
            return 1;
        }
        if (c < 0x20)
            return 0;
        if (c == '\\') {
            if (++(*i) >= len)
                return 0;
            char e = s[*i];
            if (e == 'u') {
                if (*i + 4 >= len)
                    return 0;
                for (int k = 1; k <= 4; k++)
                    if (hexval(s[*i + (uint32_t)k]) < 0)
                        return 0;
                *i += 4;
            } else if (e != '"' && e != '\\' && e != '/' && e != 'b' && e != 'f' && e != 'n' &&
                       e != 'r' && e != 't') {
                return 0;
            }
        }
        (*i)++;
    }
    return 0;
}

static int digits(const char *s, uint32_t len, uint32_t *i)
{
    uint32_t start = *i;
    while (*i < len && s[*i] >= '0' && s[*i] <= '9')
        (*i)++;
    return *i > start;
}

static int skip_number(const char *s, uint32_t len, uint32_t *i)
{
    if (*i < len && s[*i] == '-')
        (*i)++;
    if (*i < len && s[*i] == '0')
        (*i)++;
    else if (!digits(s, len, i))
        return 0;
    if (*i < len && s[*i] == '.') {
        (*i)++;
        if (!digits(s, len, i))
            return 0;
    }
    if (*i < len && (s[*i] == 'e' || s[*i] == 'E')) {
        (*i)++;
        if (*i < len && (s[*i] == '+' || s[*i] == '-'))
            (*i)++;
        if (!digits(s, len, i))
            return 0;
    }
    return 1;
}

static int skip_lit(const char *s, uint32_t len, uint32_t *i, const char *lit)
{
    uint32_t n = cstrlen(lit);
    if (len - *i < n)
        return 0;
    for (uint32_t k = 0; k < n; k++)
        if (s[*i + k] != lit[k])
            return 0;
    *i += n;
    return 1;
}

/* Skips one value at s[*i] (no leading whitespace): 1 ok, 0 malformed. */
static int skip_value(const char *s, uint32_t len, uint32_t *i, uint32_t depth)
{
    if (*i >= len)
        return 0;
    char c = s[*i];
    if (c == '"')
        return skip_string(s, len, i);
    if (c == '-' || (c >= '0' && c <= '9'))
        return skip_number(s, len, i);
    if (c == 't')
        return skip_lit(s, len, i, "true");
    if (c == 'f')
        return skip_lit(s, len, i, "false");
    if (c == 'n')
        return skip_lit(s, len, i, "null");
    if (c != '{' && c != '[')
        return 0;
    if (depth >= JSON_DEPTH_MAX)
        return 0;
    char close = c == '{' ? '}' : ']';
    (*i)++;
    ws(s, len, i);
    if (*i < len && s[*i] == close) {
        (*i)++;
        return 1;
    }
    for (;;) {
        if (c == '{') {
            if (!skip_string(s, len, i))
                return 0;
            ws(s, len, i);
            if (*i >= len || s[*i] != ':')
                return 0;
            (*i)++;
            ws(s, len, i);
        }
        if (!skip_value(s, len, i, depth + 1))
            return 0;
        ws(s, len, i);
        if (*i >= len)
            return 0;
        if (s[*i] == close) {
            (*i)++;
            return 1;
        }
        if (s[*i] != ',')
            return 0;
        (*i)++;
        ws(s, len, i);
    }
}

int json_valid(const char *s, uint32_t len)
{
    uint32_t i = 0;
    ws(s, len, &i);
    if (!skip_value(s, len, &i, 0))
        return 0;
    ws(s, len, &i);
    return i == len;
}

static struct jspan trim(struct jspan v)
{
    while (v.len && is_ws(v.p[0])) {
        v.p++;
        v.len--;
    }
    while (v.len && is_ws(v.p[v.len - 1]))
        v.len--;
    return v;
}

int json_type(struct jspan v)
{
    v = trim(v);
    uint32_t i = 0;
    if (!v.len || !skip_value(v.p, v.len, &i, 0) || i != v.len)
        return JSON_BAD;
    switch (v.p[0]) {
    case '{': return JSON_OBJECT;
    case '[': return JSON_ARRAY;
    case '"': return JSON_STRING;
    case 't': return JSON_TRUE;
    case 'f': return JSON_FALSE;
    case 'n': return JSON_NULL;
    default: return JSON_NUMBER;
    }
}

/* Decodes the string literal at s[0..len) (quotes included); see json_string. */
static int decode_string(const char *s, uint32_t len, char *out, uint32_t cap)
{
    uint32_t i = 0, o = 0;
    if (!skip_string(s, len, &i) || i != len || cap == 0)
        return -1;
    for (i = 1; i + 1 < len;) {
        unsigned char c = (unsigned char)s[i];
        uint32_t cp;
        if (c != '\\') {
            if (o + 1 >= cap)
                return -1;
            out[o++] = (char)c;
            i++;
            continue;
        }
        char e = s[i + 1];
        i += 2;
        switch (e) {
        case 'b': cp = 8; break;
        case 'f': cp = 12; break;
        case 'n': cp = 10; break;
        case 'r': cp = 13; break;
        case 't': cp = 9; break;
        case 'u':
            cp = (uint32_t)(hexval(s[i]) << 12 | hexval(s[i + 1]) << 8 | hexval(s[i + 2]) << 4 |
                            hexval(s[i + 3]));
            i += 4;
            if (cp >= 0xD800 && cp <= 0xDBFF && i + 5 < len && s[i] == '\\' && s[i + 1] == 'u') {
                uint32_t lo = (uint32_t)(hexval(s[i + 2]) << 12 | hexval(s[i + 3]) << 8 |
                                         hexval(s[i + 4]) << 4 | hexval(s[i + 5]));
                if (lo >= 0xDC00 && lo <= 0xDFFF) {
                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    i += 6;
                }
            }
            if (cp >= 0xD800 && cp <= 0xDFFF)
                cp = 0xFFFD; /* unpaired surrogate */
            break;
        default: cp = (uint32_t)e; break; /* " \ / */
        }
        if (cp == 0)
            return -1;
        char u[4];
        uint32_t k;
        if (cp < 0x80) {
            u[0] = (char)cp;
            k = 1;
        } else if (cp < 0x800) {
            u[0] = (char)(0xC0 | cp >> 6);
            u[1] = (char)(0x80 | (cp & 0x3F));
            k = 2;
        } else if (cp < 0x10000) {
            u[0] = (char)(0xE0 | cp >> 12);
            u[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
            u[2] = (char)(0x80 | (cp & 0x3F));
            k = 3;
        } else {
            u[0] = (char)(0xF0 | cp >> 18);
            u[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
            u[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
            u[3] = (char)(0x80 | (cp & 0x3F));
            k = 4;
        }
        if (o + k >= cap)
            return -1;
        for (uint32_t j = 0; j < k; j++)
            out[o++] = u[j];
    }
    out[o] = 0;
    return (int)o;
}

int json_string(struct jspan v, char *out, uint32_t cap)
{
    v = trim(v);
    return decode_string(v.p, v.len, out, cap);
}

int json_member(struct jspan v, const char *key, struct jspan *out)
{
    v = trim(v);
    const char *s = v.p;
    uint32_t len = v.len, i = 0;
    if (!len || s[0] != '{')
        return -1;
    i = 1;
    ws(s, len, &i);
    if (i < len && s[i] == '}')
        return i + 1 == len ? 0 : -1;
    uint32_t klen = cstrlen(key);
    for (;;) {
        uint32_t ks = i;
        if (!skip_string(s, len, &i))
            return -1;
        /* compare the decoded key (keys of the protocol are short) */
        char kb[64];
        int kn = (i - ks <= 2 * sizeof(kb)) ? decode_string(s + ks, i - ks, kb, sizeof(kb)) : -1;
        int match = kn == (int)klen;
        for (uint32_t k = 0; match && k < klen; k++)
            match = kb[k] == key[k];
        ws(s, len, &i);
        if (i >= len || s[i] != ':')
            return -1;
        i++;
        ws(s, len, &i);
        uint32_t vs = i;
        if (!skip_value(s, len, &i, 1))
            return -1;
        if (match) {
            out->p = s + vs;
            out->len = i - vs;
            return 1;
        }
        ws(s, len, &i);
        if (i >= len)
            return -1;
        if (s[i] == '}')
            return 0;
        if (s[i] != ',')
            return -1;
        i++;
        ws(s, len, &i);
    }
}

int json_element(struct jspan v, uint32_t index, uint32_t *count, struct jspan *out)
{
    v = trim(v);
    const char *s = v.p;
    uint32_t len = v.len, i = 1, n = 0;
    int found = 0;
    if (!len || s[0] != '[')
        return -1;
    ws(s, len, &i);
    if (i < len && s[i] == ']') {
        if (count)
            *count = 0;
        return 0;
    }
    for (;;) {
        uint32_t vs = i;
        if (!skip_value(s, len, &i, 1))
            return -1;
        if (n == index && out) {
            out->p = s + vs;
            out->len = i - vs;
            found = 1;
        }
        n++;
        ws(s, len, &i);
        if (i >= len)
            return -1;
        if (s[i] == ']')
            break;
        if (s[i] != ',')
            return -1;
        i++;
        ws(s, len, &i);
    }
    if (count)
        *count = n;
    return found;
}

int json_u64(struct jspan v, uint64_t *out)
{
    v = trim(v);
    if (!v.len || v.len > 19)
        return 0;
    uint64_t r = 0;
    for (uint32_t i = 0; i < v.len; i++) {
        if (v.p[i] < '0' || v.p[i] > '9')
            return 0;
        r = r * 10 + (uint64_t)(v.p[i] - '0');
    }
    if (v.len > 1 && v.p[0] == '0')
        return 0;
    *out = r;
    return 1;
}

int json_member_str(struct jspan v, const char *key, char *out, uint32_t cap)
{
    struct jspan m;
    if (json_member(v, key, &m) != 1)
        return 0;
    return json_string(m, out, cap) >= 0;
}

int json_next_member(struct jspan v, uint32_t *pos, char *key, uint32_t kcap,
                     struct jspan *val)
{
    v = trim(v);
    const char *s = v.p;
    uint32_t len = v.len, i = *pos;
    if (!len || s[0] != '{')
        return -1;
    if (i == 0) {
        i = 1;
        ws(s, len, &i);
        if (i < len && s[i] == '}')
            return i + 1 == len ? 0 : -1;
    } else {
        ws(s, len, &i);
        if (i >= len)
            return -1;
        if (s[i] == '}')
            return 0;
        if (s[i] != ',')
            return -1;
        i++;
        ws(s, len, &i);
    }
    uint32_t ks = i;
    if (!skip_string(s, len, &i) || decode_string(s + ks, i - ks, key, kcap) < 0)
        return -1;
    ws(s, len, &i);
    if (i >= len || s[i] != ':')
        return -1;
    i++;
    ws(s, len, &i);
    uint32_t vs = i;
    if (!skip_value(s, len, &i, 1))
        return -1;
    val->p = s + vs;
    val->len = i - vs;
    *pos = i;
    return 1;
}
