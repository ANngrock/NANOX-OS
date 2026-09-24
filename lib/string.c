/* Built with -fno-builtin so these loops are not turned back into calls. */
#include <stdint.h>

#include <nanox/string.h>

void *memcpy(void *restrict dst, const void *restrict src, size_t n)
{
    unsigned char *d = dst;
    const unsigned char *s = src;
    while (n--)
        *d++ = *s++;
    return dst;
}

void *memmove(void *dst, const void *src, size_t n)
{
    unsigned char *d = dst;
    const unsigned char *s = src;
    if (d == s || n == 0)
        return dst;
    if (d < s) {
        while (n--)
            *d++ = *s++;
    } else {
        d += n;
        s += n;
        while (n--)
            *--d = *--s;
    }
    return dst;
}

typedef uint64_t __attribute__((may_alias)) word_alias;

void *memset(void *dst, int c, size_t n)
{
    unsigned char *d = dst;
    while (n && ((uintptr_t)d & 7)) {
        *d++ = (unsigned char)c;
        n--;
    }
    /* Aligned middle in 8-byte stores (the kernel poisons megabytes). */
    uint64_t w = 0x0101010101010101ull * (unsigned char)c;
    for (; n >= 8; n -= 8, d += 8)
        *(word_alias *)d = w;
    while (n--)
        *d++ = (unsigned char)c;
    return dst;
}

int memcmp(const void *a, const void *b, size_t n)
{
    const unsigned char *x = a, *y = b;
    for (size_t i = 0; i < n; i++) {
        if (x[i] != y[i])
            return x[i] < y[i] ? -1 : 1;
    }
    return 0;
}

size_t nx_strlen(const char *s)
{
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}
