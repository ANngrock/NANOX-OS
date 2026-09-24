#include <nanox/printf.h>
#include <nanox/serial.h>

static void put_uint(uint64_t v, unsigned base, int upper, int width, char pad)
{
    char buf[24];
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    int n = 0;
    do {
        buf[n++] = digits[v % base];
        v /= base;
    } while (v && n < (int)sizeof(buf));
    while (width > n) {
        nx_serial_putc(pad);
        width--;
    }
    while (n)
        nx_serial_putc(buf[--n]);
}

/* va_list may be an array type, so fetch arguments inline via a macro. */
#define NEXT_UNSIGNED(ap, lng)                                                                     \
    ((lng) == 2   ? (uint64_t)va_arg(ap, unsigned long long)                                       \
     : (lng) == 1 ? (uint64_t)va_arg(ap, unsigned long)                                            \
                  : (uint64_t)va_arg(ap, unsigned))

void nx_vprintf(const char *fmt, va_list ap)
{
    for (; *fmt; fmt++) {
        if (*fmt != '%') {
            nx_serial_putc(*fmt);
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
            nx_serial_write(s ? s : "(null)");
            break;
        }
        case 'c': nx_serial_putc((char)va_arg(ap, int)); break;
        case 'd': {
            int64_t v = lng == 2   ? va_arg(ap, long long)
                        : lng == 1 ? va_arg(ap, long)
                                   : va_arg(ap, int);
            uint64_t mag = (uint64_t)v;
            if (v < 0) {
                nx_serial_putc('-');
                mag = 0 - mag;
            }
            put_uint(mag, 10, 0, width, pad);
            break;
        }
        case 'u': put_uint(NEXT_UNSIGNED(ap, lng), 10, 0, width, pad); break;
        case 'x':
        case 'X': put_uint(NEXT_UNSIGNED(ap, lng), 16, *fmt == 'X', width, pad); break;
        case 'p':
            nx_serial_write("0x");
            put_uint((uint64_t)(uintptr_t)va_arg(ap, void *), 16, 0, 16, '0');
            break;
        case '%': nx_serial_putc('%'); break;
        case '\0': return;
        default: nx_serial_putc('?'); break;
        }
    }
}

void nx_printf(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    nx_vprintf(fmt, ap);
    va_end(ap);
}

void nx_print_hex_bytes(const uint8_t *p, uint32_t len)
{
    static const char hex[] = "0123456789abcdef";
    for (uint32_t i = 0; i < len; i++) {
        nx_serial_putc(hex[p[i] >> 4]);
        nx_serial_putc(hex[p[i] & 15]);
    }
}
