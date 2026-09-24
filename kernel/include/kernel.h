#ifndef NANOX_KERNEL_H
#define NANOX_KERNEL_H

#include <stdint.h>

#include <nanox/bootinfo.h>

/* Linker-provided symbols (kernel/arch/x86_64/kernel.ld). */
extern char __kernel_start[], __kernel_end[];
extern char __text_start[], __text_end[];
extern char __rodata_start[], __rodata_end[];
extern char __data_start[], __data_end[];
extern char __stacks_start[], __stacks_end[];
extern char __boot_stack_guard[], __boot_stack_bottom[], __boot_stack_top[];
extern char __ist_df_guard[], __ist_df_bottom[], __ist_df_top[];
extern char __ist_nmi_guard[], __ist_nmi_bottom[], __ist_nmi_top[];
extern char __ist_mc_guard[], __ist_mc_bottom[], __ist_mc_top[];

/* RSP handed over by the loader, saved by _start before switching stacks. */
extern uint64_t nx_boot_entry_rsp;

struct nx_trap_frame;

/* C entry, called from _start (arch/x86_64/entry.S) on the kernel boot stack. */
__attribute__((noreturn)) void kernel_main(const struct nx_boot_info *bi);

/* Prints "NANOX: PANIC <message>" and a backtrace, exits QEMU with
 * NX_EXIT_PANIC, halts. */
__attribute__((noreturn, format(printf, 1, 2))) void nx_panic(const char *fmt, ...);

/* Reports an unhandled trap (exception report, registers, backtrace) and
 * exits QEMU with NX_EXIT_EXCEPTION. */
__attribute__((noreturn)) void nx_fatal_trap(struct nx_trap_frame *f);

/* Writes `code` to isa-debug-exit; halts if the device is absent. */
__attribute__((noreturn)) void nx_debug_exit(uint8_t code);

#endif
