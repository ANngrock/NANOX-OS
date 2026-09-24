#ifndef NANOX_ARCH_X86_64_TIMER_H
#define NANOX_ARCH_X86_64_TIMER_H

#include <stdint.h>

struct nx_trap_frame;

#define NX_TIMER_HZ 100u
#define NX_TIMER_WINDOW_PERIODS 30u /* measurement window: 30 x 10 ms */

struct nx_timer_result {
    uint32_t lapic_per_period; /* LAPIC timer counts (divide 16) per 10 ms */
    uint64_t ticks;            /* interrupts counted in the window */
    uint64_t tsc_delta;        /* TSC cycles elapsed in the window */
};

/* Remaps the legacy 8259 PICs to vectors 0x20-0x2F and masks every line. */
void nx_pic_disable(void);
/* Enables the local APIC, calibrates its timer against PIT channel 2 and
 * counts periodic interrupts during a PIT-timed window with IF=1.  With
 * `mask_for_test` the timer LVT stays masked (negative scenario).  Returns 0
 * on success, otherwise a static reason string.  Leaves the timer masked and
 * interrupts disabled. */
const char *nx_timer_check(int mask_for_test, struct nx_timer_result *out);
extern volatile uint64_t nx_timer_ticks;

/* After a successful nx_timer_check: periodic interrupts at NX_TIMER_HZ
 * delivered to `tick` (called in interrupt context after the EOI). */
void nx_timer_start_periodic(void (*tick)(struct nx_trap_frame *f));

#endif
