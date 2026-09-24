/*
 * Minimal printf to the serial port.
 * Supported: %s %c %d %u %x %X %p %%, length modifiers "l" and "ll" with
 * their C meaning, optional '0' flag and field width.  `long` is 32-bit in
 * the loader (LLP64) and 64-bit in the kernel (LP64), so uint64_t values are
 * printed with the NX_PRI* macros below.
 */
#ifndef NANOX_LIB_PRINTF_H
#define NANOX_LIB_PRINTF_H

#include <stdarg.h>
#include <stdint.h>

#if defined(__LP64__)
#define NX_PRIu64 "lu"
#define NX_PRIx64 "lx"
#else
#define NX_PRIu64 "llu"
#define NX_PRIx64 "llx"
#endif

__attribute__((format(printf, 1, 2))) void nx_printf(const char *fmt, ...);
void nx_vprintf(const char *fmt, va_list ap);
/* Prints `len` bytes as lowercase hex (no newline). */
void nx_print_hex_bytes(const uint8_t *p, uint32_t len);

#endif
