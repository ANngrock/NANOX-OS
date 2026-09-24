/*
 * HTTP/1.1 response parser and event-stream parser (nanox/http.h).
 */
#include <nanox/http.h>
#include <nanox/nerr.h>

enum { S_HEAD = 0, S_BODY_LEN, S_BODY_CHUNKED, S_BODY_CLOSE, S_DONE };
enum { C_SIZE = 0, C_EXT, C_SIZE_LF, C_DATA, C_DATA_CR, C_DATA_LF, C_TRAILER };

static char lower(char c)
{
    return c >= 'A' && c <= 'Z' ? (char)(c - 'A' + 'a') : c;
}

/* Case-insensitive comparison of s[0..n) with the lower-case word w. */
static int word_is(const char *s, uint32_t n, const char *w)
{
    uint32_t i = 0;
    for (; i < n && w[i]; i++)
        if (lower(s[i]) != w[i])
            return 0;
    return i == n && !w[i];
}

/* Whether the comma-separated list s[0..n) contains the token w. */
static int list_has(const char *s, uint32_t n, const char *w)
{
    uint32_t i = 0;
    while (i < n) {
        while (i < n && (s[i] == ' ' || s[i] == '\t' || s[i] == ','))
            i++;
        uint32_t b = i;
        while (i < n && s[i] != ',' && s[i] != ';')
            i++;
        uint32_t e = i;
        while (e > b && (s[e - 1] == ' ' || s[e - 1] == '\t'))
            e--;
        if (word_is(s + b, e - b, w))
            return 1;
        while (i < n && s[i] != ',')
            i++;
    }
    return 0;
}

static void copy_value(char *out, uint32_t cap, const char *s, uint32_t n, int lowercase)
{
    uint32_t k = 0;
    for (; k < n && k + 1 < cap; k++) {
        char c = s[k];
        if (c < 0x21 || c > 0x7E)
            c = '_';
        out[k] = lowercase ? lower(c) : c;
    }
    out[k] = 0;
}

void http_resp_init(struct http_resp *r)
{
    char *p = (char *)r;
    for (uint32_t i = 0; i < sizeof(*r); i++)
        p[i] = 0;
    r->content_length = -1;
}

int http_head_done(const struct http_resp *r)
{
    return r->state != S_HEAD;
}

/* Parses the complete header section in r->head: 0 or -NXE_*. */
static int parse_head(struct http_resp *r)
{
    const char *h = r->head;
    uint32_t n = r->head_len, i = 0;
    /* status line: HTTP/1.x SP 3DIGIT SP reason */
    if (n < 12 || !(word_is(h, 8, "http/1.1") || word_is(h, 8, "http/1.0")) || h[8] != ' ')
        return -NXE_PRV_MALFORMED_HTTP;
    for (int k = 9; k < 12; k++)
        if (h[k] < '0' || h[k] > '9')
            return -NXE_PRV_MALFORMED_HTTP;
    r->status = (uint32_t)((h[9] - '0') * 100 + (h[10] - '0') * 10 + (h[11] - '0'));
    if (r->status < 100)
        return -NXE_PRV_MALFORMED_HTTP;
    while (i < n && h[i] != '\n')
        i++;
    i++;
    while (i < n) {
        uint32_t b = i;
        while (i < n && h[i] != '\n')
            i++;
        uint32_t e = i++;
        if (e > b && h[e - 1] == '\r')
            e--;
        if (e == b)
            break; /* the empty line */
        uint32_t c = b;
        while (c < e && h[c] != ':')
            c++;
        if (c == e || c == b || h[b] == ' ' || h[b] == '\t')
            return -NXE_PRV_MALFORMED_HTTP; /* no colon, empty name, obsolete folding */
        for (uint32_t k = b; k < c; k++)
            if (h[k] <= ' ' || h[k] > '~')
                return -NXE_PRV_MALFORMED_HTTP;
        uint32_t vb = c + 1, ve = e;
        while (vb < ve && (h[vb] == ' ' || h[vb] == '\t'))
            vb++;
        while (ve > vb && (h[ve - 1] == ' ' || h[ve - 1] == '\t'))
            ve--;
        const char *name = h + b, *v = h + vb;
        uint32_t nl = c - b, vl = ve - vb;
        if (word_is(name, nl, "content-length")) {
            if (vl == 0 || vl > 12)
                return -NXE_PRV_MALFORMED_HTTP;
            int64_t len = 0;
            for (uint32_t k = 0; k < vl; k++) {
                if (v[k] < '0' || v[k] > '9')
                    return -NXE_PRV_MALFORMED_HTTP;
                len = len * 10 + (v[k] - '0');
            }
            if (r->content_length >= 0 && r->content_length != len)
                return -NXE_PRV_MALFORMED_HTTP;
            r->content_length = len;
        } else if (word_is(name, nl, "transfer-encoding")) {
            if (!list_has(v, vl, "chunked"))
                return -NXE_PRV_MALFORMED_HTTP; /* no other coding is accepted */
            r->chunked = 1;
        } else if (word_is(name, nl, "connection")) {
            if (list_has(v, vl, "close"))
                r->conn_close = 1;
        } else if (word_is(name, nl, "retry-after")) {
            uint32_t s = 0, k = 0;
            for (; k < vl && v[k] >= '0' && v[k] <= '9' && s < 100000; k++)
                s = s * 10 + (uint32_t)(v[k] - '0');
            r->retry_after_s = (k == vl) ? s : 0;
        } else if (word_is(name, nl, "content-type")) {
            uint32_t k = 0;
            while (k < vl && v[k] != ';' && v[k] != ' ')
                k++;
            copy_value(r->content_type, sizeof(r->content_type), v, k, 1);
        } else if (word_is(name, nl, "request-id")) {
            copy_value(r->request_id, sizeof(r->request_id), v, vl, 0);
        }
    }
    if (r->head[8] == ' ' && r->head[7] == '0')
        r->conn_close = 1; /* HTTP/1.0: no persistent connection */
    return 0;
}

static int emit(http_sink sink, void *ctx, struct http_resp *r, const char *p, uint32_t n)
{
    r->body_bytes += n;
    if (!n || !sink)
        return 0;
    int e = sink(ctx, p, n);
    return e ? -e : 0;
}

int http_feed(struct http_resp *r, const char *in, uint32_t n, uint32_t *used, http_sink sink,
              void *ctx)
{
    uint32_t i = 0;
    int e;
    *used = 0;
    while (i < n) {
        switch (r->state) {
        case S_HEAD: {
            if (r->head_len >= HTTP_HEAD_MAX - 1)
                return -NXE_PRV_TOO_LARGE;
            char c = in[i++];
            r->head[r->head_len++] = c;
            r->head[r->head_len] = 0;
            uint32_t L = r->head_len;
            int end = c == '\n' && ((L >= 2 && r->head[L - 2] == '\n') ||
                                    (L >= 3 && r->head[L - 2] == '\r' && r->head[L - 3] == '\n'));
            if (!end)
                break;
            if ((e = parse_head(r)) != 0)
                return e;
            if (r->status >= 100 && r->status < 200) {
                if (r->status == 101)
                    return -NXE_PRV_MALFORMED_HTTP; /* no upgrade was asked for */
                http_resp_init(r); /* interim response: skip it */
                break;
            }
            if (r->status == 204 || r->status == 304 || (!r->chunked && r->content_length == 0)) {
                r->state = S_DONE;
                *used = i;
                return HTTP_DONE;
            }
            r->state = r->chunked ? S_BODY_CHUNKED
                       : r->content_length > 0 ? S_BODY_LEN
                                               : S_BODY_CLOSE;
            break;
        }
        case S_BODY_LEN: {
            uint64_t left = (uint64_t)r->content_length - r->body_bytes;
            uint32_t k = n - i < left ? n - i : (uint32_t)left;
            if ((e = emit(sink, ctx, r, in + i, k)) != 0)
                return e;
            i += k;
            if (r->body_bytes == (uint64_t)r->content_length) {
                r->state = S_DONE;
                *used = i;
                return HTTP_DONE;
            }
            break;
        }
        case S_BODY_CLOSE:
            if ((e = emit(sink, ctx, r, in + i, n - i)) != 0)
                return e;
            i = n;
            break;
        case S_BODY_CHUNKED: {
            char c = in[i];
            switch (r->cstate) {
            case C_SIZE: {
                int v = c >= '0' && c <= '9'   ? c - '0'
                        : c >= 'a' && c <= 'f' ? c - 'a' + 10
                        : c >= 'A' && c <= 'F' ? c - 'A' + 10
                                               : -1;
                i++;
                if (v >= 0) {
                    if (r->crem > 0x0FFFFFFFull)
                        return -NXE_PRV_MALFORMED_HTTP;
                    r->crem = r->crem * 16 + (uint64_t)v;
                    r->cline++;
                    break;
                }
                if (!r->cline)
                    return -NXE_PRV_MALFORMED_HTTP;
                if (c == ';' || c == ' ' || c == '\t')
                    r->cstate = C_EXT;
                else if (c == '\r')
                    r->cstate = C_SIZE_LF;
                else if (c == '\n')
                    goto size_done;
                else
                    return -NXE_PRV_MALFORMED_HTTP;
                break;
            }
            case C_EXT:
                i++;
                if (c == '\r')
                    r->cstate = C_SIZE_LF;
                else if (c == '\n')
                    goto size_done;
                break;
            case C_SIZE_LF:
                i++;
                if (c != '\n')
                    return -NXE_PRV_MALFORMED_HTTP;
            size_done:
                r->cline = 0;
                r->cstate = r->crem ? C_DATA : C_TRAILER;
                break;
            case C_DATA: {
                uint32_t k = n - i < r->crem ? n - i : (uint32_t)r->crem;
                if ((e = emit(sink, ctx, r, in + i, k)) != 0)
                    return e;
                i += k;
                r->crem -= k;
                if (!r->crem)
                    r->cstate = C_DATA_CR;
                break;
            }
            case C_DATA_CR:
                i++;
                if (c == '\r')
                    r->cstate = C_DATA_LF;
                else if (c == '\n')
                    r->cstate = C_SIZE;
                else
                    return -NXE_PRV_MALFORMED_HTTP;
                break;
            case C_DATA_LF:
                i++;
                if (c != '\n')
                    return -NXE_PRV_MALFORMED_HTTP;
                r->cstate = C_SIZE;
                break;
            case C_TRAILER:
                i++;
                if (c == '\n') {
                    if (r->cline == 0) {
                        r->state = S_DONE;
                        *used = i;
                        return HTTP_DONE;
                    }
                    r->cline = 0;
                } else if (c != '\r') {
                    if (++r->cline > 1024)
                        return -NXE_PRV_TOO_LARGE;
                }
                break;
            }
            break;
        }
        case S_DONE:
        default:
            *used = i;
            return HTTP_DONE;
        }
    }
    *used = n;
    return r->state == S_DONE ? HTTP_DONE : HTTP_MORE;
}

int http_eof_completes(const struct http_resp *r)
{
    return r->state == S_BODY_CLOSE || r->state == S_DONE;
}

/* ---- event stream ------------------------------------------------------------------ */

void sse_init(struct sse *s)
{
    s->event[0] = 0;
    s->data_len = 0;
    s->data[0] = 0;
    s->have_data = 0;
    s->line_len = 0;
    s->skip_lf = 0;
    s->overflow = 0;
    s->events = 0;
}

static int starts(const char *line, uint32_t n, const char *field, uint32_t *vpos)
{
    uint32_t k = 0;
    while (field[k]) {
        if (k >= n || line[k] != field[k])
            return 0;
        k++;
    }
    if (k == n) {
        *vpos = k; /* "data" alone: empty value */
        return 1;
    }
    if (line[k] != ':')
        return 0;
    k++;
    if (k < n && line[k] == ' ')
        k++;
    *vpos = k;
    return 1;
}

static int sse_line(struct sse *s, sse_handler h, void *ctx)
{
    const char *l = s->line;
    uint32_t n = s->line_len, v;
    s->line_len = 0;
    if (n == 0) { /* dispatch */
        int e = 0;
        if (s->have_data) {
            s->data[s->data_len] = 0;
            s->events++;
            e = h(ctx, s->event[0] ? s->event : "message", s->data, s->data_len);
        }
        s->event[0] = 0;
        s->data_len = 0;
        s->have_data = 0;
        return e;
    }
    if (l[0] == ':')
        return 0; /* comment */
    if (starts(l, n, "data", &v)) {
        uint32_t add = n - v + (s->have_data ? 1 : 0);
        if (s->data_len + add >= SSE_DATA_MAX) {
            s->overflow = 1;
            return NXE_PRV_TOO_LARGE;
        }
        if (s->have_data)
            s->data[s->data_len++] = '\n';
        for (uint32_t k = v; k < n; k++)
            s->data[s->data_len++] = l[k];
        s->have_data = 1;
    } else if (starts(l, n, "event", &v)) {
        uint32_t k = 0;
        for (; v + k < n && k + 1 < sizeof(s->event); k++)
            s->event[k] = l[v + k];
        s->event[k] = 0;
    }
    /* id, retry and unknown fields are ignored */
    return 0;
}

int sse_feed(struct sse *s, const char *in, uint32_t n, sse_handler h, void *ctx)
{
    for (uint32_t i = 0; i < n; i++) {
        char c = in[i];
        if (s->skip_lf) {
            s->skip_lf = 0;
            if (c == '\n')
                continue;
        }
        if (c == '\r' || c == '\n') {
            s->skip_lf = c == '\r';
            s->line[s->line_len] = 0;
            int e = sse_line(s, h, ctx);
            if (e)
                return e;
            continue;
        }
        if (s->line_len + 1 >= SSE_LINE_MAX) {
            s->overflow = 1;
            return NXE_PRV_TOO_LARGE;
        }
        s->line[s->line_len++] = c;
    }
    return NXE_OK;
}
