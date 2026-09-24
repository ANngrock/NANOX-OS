/*
 * CMOS real-time clock (M5): the wall clock of NX_SYS_CLOCK.  Read once
 * (nx_rtc_init); later values are that reading plus the timer ticks since,
 * so the clock is monotonic within a boot and never jumps with the RTC.
 * The bench fixes the RTC at 2026-01-01T00:00:00 UTC with clock=vm
 * (tools/bench/qemu.py), so certificate validity checks are deterministic.
 */
#ifndef NANOX_KERNEL_DEV_RTC_H
#define NANOX_KERNEL_DEV_RTC_H

#include <stdint.h>

/* Reads the RTC (binary or BCD, 24 h or 12 h, century register 0x32).
 * Returns the time in Unix seconds, 0 if the RTC gave an invalid date. */
uint64_t nx_rtc_read(void);
/* Reads the RTC and remembers it with the current tick count. */
void nx_rtc_init(void);
/* Unix seconds now (0 before nx_rtc_init or after an invalid reading). */
uint64_t nx_rtc_now(uint64_t ticks, uint32_t hz);

#endif
