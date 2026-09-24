#ifndef NANOX_KERNEL_H
#define NANOX_KERNEL_H

#include <stdint.h>

#include <nanox/bootinfo.h>

/* Linker-provided bounds of the loaded kernel span (kernel.ld). */
extern char __kernel_start[];
extern char __kernel_end[];

/* C entry, called from _start (arch/x86_64/entry.S). */
__attribute__((noreturn)) void kernel_main(const struct nx_boot_info *bi);

/* Prints "NANOX: PANIC <message>", exits QEMU with NX_EXIT_PANIC, halts. */
__attribute__((noreturn, format(printf, 1, 2))) void nx_panic(const char *fmt, ...);

/* Writes `code` to isa-debug-exit; halts if the device is absent. */
__attribute__((noreturn)) void nx_debug_exit(uint8_t code);

#endif
