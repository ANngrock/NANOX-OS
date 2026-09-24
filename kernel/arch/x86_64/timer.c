/*
 * Timer (M1): local APIC timer in periodic mode at NX_TIMER_HZ, calibrated
 * against PIT channel 2 (fixed 1.193182 MHz input, polled through port 0x61
 * without interrupts).  The legacy 8259 PICs are remapped away from the
 * exception vectors and fully masked.  Details: docs/m1-kernel.md ("Таймер").
 */
#include <nanox/port.h>

#include "cpu.h"
#include "kernel.h"
#include "mm/mm.h"
#include "timer.h"
#include "trap.h"

#define LAPIC_EOI 0x0B0u
#define LAPIC_SVR 0x0F0u
#define LAPIC_TPR 0x080u
#define LAPIC_LVT_TIMER 0x320u
#define LAPIC_TIMER_INIT 0x380u
#define LAPIC_TIMER_CUR 0x390u
#define LAPIC_TIMER_DIV 0x3E0u
#define LVT_MASKED (1u << 16)
#define LVT_PERIODIC (1u << 17)
#define APIC_BASE_ENABLE (1ull << 11)

#define PIT_HZ 1193182u
#define PIT_PERIOD_COUNT ((PIT_HZ + NX_TIMER_HZ / 2) / NX_TIMER_HZ) /* 11932 */
/* Upper bound on port reads while waiting for one PIT period. */
#define PIT_SPIN_LIMIT 50000000u

volatile uint64_t nx_timer_ticks;
static volatile uint32_t *lapic;
static uint32_t calibrated_count;
static void (*periodic_tick)(struct nx_trap_frame *f);

static uint32_t lapic_read(uint32_t reg)
{
    return lapic[reg / 4];
}

static void lapic_write(uint32_t reg, uint32_t v)
{
    lapic[reg / 4] = v;
    (void)lapic[0x20 / 4]; /* read back (ID) to post the write */
}

static void on_timer(struct nx_trap_frame *f)
{
    (void)f;
    nx_timer_ticks++;
    lapic_write(LAPIC_EOI, 0);
}

static void on_periodic(struct nx_trap_frame *f)
{
    nx_timer_ticks++;
    lapic_write(LAPIC_EOI, 0);
    periodic_tick(f);
}

void nx_timer_start_periodic(void (*tick)(struct nx_trap_frame *f))
{
    if (!calibrated_count)
        nx_panic("timer: start before a successful calibration");
    periodic_tick = tick;
    nx_timer_handler = on_periodic;
    lapic_write(LAPIC_LVT_TIMER, LVT_PERIODIC | NX_VEC_TIMER);
    lapic_write(LAPIC_TIMER_INIT, calibrated_count);
}

void nx_pic_disable(void)
{
    nx_outb(0x20, 0x11); /* ICW1: init, expect ICW4 */
    nx_outb(0xA0, 0x11);
    nx_outb(0x21, NX_VEC_PIC_BASE); /* ICW2: vector offsets */
    nx_outb(0xA1, NX_VEC_PIC_BASE + 8);
    nx_outb(0x21, 0x04); /* ICW3: slave on IRQ2 */
    nx_outb(0xA1, 0x02);
    nx_outb(0x21, 0x01); /* ICW4: 8086 mode */
    nx_outb(0xA1, 0x01);
    nx_outb(0x21, 0xFF); /* mask everything */
    nx_outb(0xA1, 0xFF);
}

/* One PIT channel 2 period (mode 0, one shot); 0 if OUT2 never rises. */
static int pit_wait_period(void)
{
    nx_outb(0x61, nx_inb(0x61) & (uint8_t)~0x03); /* gate low, speaker off */
    nx_outb(0x43, 0xB0);                          /* channel 2, lo/hi byte, mode 0 */
    nx_outb(0x42, PIT_PERIOD_COUNT & 0xFF);
    nx_outb(0x42, PIT_PERIOD_COUNT >> 8);
    nx_outb(0x61, (nx_inb(0x61) & (uint8_t)~0x02) | 0x01); /* gate high: count */
    for (uint32_t i = 0; i < PIT_SPIN_LIMIT; i++)
        if (nx_inb(0x61) & 0x20) /* OUT2 high: terminal count reached */
            return 1;
    return 0;
}

const char *nx_timer_check(int mask_for_test, struct nx_timer_result *out)
{
    uint32_t r[4];
    nx_cpuid(1, 0, r);
    if (!(r[3] & (1u << 9)))
        return "no local APIC";
    uint64_t base = nx_rdmsr(NX_MSR_APIC_BASE);
    nx_wrmsr(NX_MSR_APIC_BASE, base | APIC_BASE_ENABLE);
    lapic = nx_vmm_map_mmio(base & 0xFFFFF000ull);

    nx_pic_disable();
    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_SVR, 0x100u | NX_VEC_SPURIOUS); /* software enable */
    lapic_write(LAPIC_TIMER_DIV, 0x3);                /* divide by 16 */

    /* Calibrate: LAPIC counts during one PIT period. */
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED | NX_VEC_TIMER);
    lapic_write(LAPIC_TIMER_INIT, 0xFFFFFFFFu);
    if (!pit_wait_period())
        return "PIT channel 2 did not reach terminal count";
    uint32_t elapsed = 0xFFFFFFFFu - lapic_read(LAPIC_TIMER_CUR);
    lapic_write(LAPIC_TIMER_INIT, 0);
    if (elapsed < 100)
        return "LAPIC timer did not count during calibration";
    out->lapic_per_period = elapsed;
    calibrated_count = elapsed;

    /* Periodic interrupts during a PIT-timed window with interrupts enabled. */
    nx_timer_handler = on_timer;
    nx_timer_ticks = 0;
    lapic_write(LAPIC_LVT_TIMER, (mask_for_test ? LVT_MASKED : 0) | LVT_PERIODIC | NX_VEC_TIMER);
    lapic_write(LAPIC_TIMER_INIT, elapsed);
    uint64_t tsc0 = nx_rdtsc();
    nx_sti();
    int pit_ok = 1;
    for (unsigned i = 0; i < NX_TIMER_WINDOW_PERIODS && pit_ok; i++)
        pit_ok = pit_wait_period();
    nx_cli();
    out->tsc_delta = nx_rdtsc() - tsc0;
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED | NX_VEC_TIMER);
    lapic_write(LAPIC_TIMER_INIT, 0);
    out->ticks = nx_timer_ticks;
    if (!pit_ok)
        return "PIT channel 2 stopped during the window";
    if (out->ticks == 0)
        return "no timer interrupts in the window";
    /* Both clocks are emulated from the same virtual clock; allow +-1/3. */
    if (out->ticks < NX_TIMER_WINDOW_PERIODS * 2 / 3 ||
        out->ticks > NX_TIMER_WINDOW_PERIODS * 4 / 3)
        return "timer interrupt rate outside +-33% of the PIT reference";
    return 0;
}
