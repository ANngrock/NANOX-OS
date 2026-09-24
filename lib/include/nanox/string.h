/* Freestanding memory/string helpers shared by the loader and the kernel.
 * The compiler may also emit calls to memcpy/memset/memmove/memcmp. */
#ifndef NANOX_LIB_STRING_H
#define NANOX_LIB_STRING_H

#include <stddef.h>

void *memcpy(void *restrict dst, const void *restrict src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
void *memset(void *dst, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);
size_t nx_strlen(const char *s);

#endif
