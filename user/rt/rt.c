/* Runtime helpers of NANOX user programs (M2); see nanox_user.h. */
#include <stdarg.h>

#include "nanox_user.h"

_Static_assert(NX_SYS_EXIT == 2, "start.S hard-codes the exit call number");

struct out {
    char buf[NX_DEBUG_WRITE_MAX];
    uint32_t len;
};

static void put(struct out *o, char c)
{
    if (o->len < sizeof(o->buf))
        o->buf[o->len++] = c;
}

static void put_uint(struct out *o, uint64_t v, unsigned base, int width, char pad)
{
    char tmp[24];
    int n = 0;
    do {
        tmp[n++] = "0123456789abcdef"[v % base];
        v /= base;
    } while (v);
    while (width-- > n)
        put(o, pad);
    while (n)
        put(o, tmp[--n]);
}

int64_t u_printf(const char *fmt, ...)
{
    struct out o;
    o.len = 0;
    va_list ap;
    va_start(ap, fmt);
    for (; *fmt; fmt++) {
        if (*fmt != '%') {
            put(&o, *fmt);
            continue;
        }
        fmt++;
        char pad = ' ';
        int width = 0, lng = 0;
        if (*fmt == '0') {
            pad = '0';
            fmt++;
        }
        while (*fmt >= '0' && *fmt <= '9')
            width = width * 10 + (*fmt++ - '0');
        while (*fmt == 'l' && lng < 2) {
            lng++;
            fmt++;
        }
        switch (*fmt) {
        case 's': {
            const char *s = va_arg(ap, const char *);
            for (s = s ? s : "(null)"; *s; s++)
                put(&o, *s);
            break;
        }
        case 'c': put(&o, (char)va_arg(ap, int)); break;
        case 'd': {
            int64_t v = lng ? va_arg(ap, long) : va_arg(ap, int);
            uint64_t mag = (uint64_t)v;
            if (v < 0) {
                put(&o, '-');
                mag = 0 - mag;
            }
            put_uint(&o, mag, 10, width, pad);
            break;
        }
        case 'u':
        case 'x': {
            uint64_t v = lng ? va_arg(ap, unsigned long) : va_arg(ap, unsigned);
            put_uint(&o, v, *fmt == 'u' ? 10 : 16, width, pad);
            break;
        }
        case '%': put(&o, '%'); break;
        case 0: fmt--; break;
        default:
            put(&o, '%');
            put(&o, *fmt);
            break;
        }
    }
    va_end(ap);
    return nx_debug_write(o.buf, o.len);
}

uint64_t u_strlen(const char *s)
{
    uint64_t n = 0;
    while (s[n])
        n++;
    return n;
}

int u_memeq(const void *a, const void *b, uint64_t n)
{
    const unsigned char *x = a, *y = b;
    for (uint64_t i = 0; i < n; i++)
        if (x[i] != y[i])
            return 0;
    return 1;
}

const char *u_err(int64_t r)
{
    static const char *const names[NX_E__COUNT] = {
        [NX_OK] = "ok",          [NX_ENOSYS] = "ENOSYS",       [NX_EINVAL] = "EINVAL",
        [NX_EFAULT] = "EFAULT",  [NX_EBADHANDLE] = "EBADHANDLE", [NX_EWRONGTYPE] = "EWRONGTYPE",
        [NX_EACCESS] = "EACCESS", [NX_ENOMEM] = "ENOMEM",        [NX_EFULL] = "EFULL",
        [NX_EEXISTS] = "EEXISTS", [NX_ERANGE] = "ERANGE",        [NX_EDEAD] = "EDEAD",
        [NX_ENOENT] = "ENOENT",  [NX_EIO] = "EIO",
    };
    if (r > 0)
        return "ok";
    if (r <= -(int64_t)NX_E__COUNT)
        return "E?";
    return names[-r];
}
