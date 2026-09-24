/* CMOS real-time clock; see rtc.h. */
#include <nanox/civil.h>
#include <nanox/port.h>
#include <nanox/printf.h>

#include "arch/x86_64/timer.h"
#include "rtc.h"

#define CMOS_ADDR 0x70u
#define CMOS_DATA 0x71u

static uint64_t base_unix, base_ticks;

static uint8_t cmos(uint8_t reg)
{
    nx_outb(CMOS_ADDR, reg);
    return nx_inb(CMOS_DATA);
}

static uint32_t from_bcd(uint8_t v, int binary)
{
    return binary ? v : (uint32_t)(v & 0x0Fu) + (uint32_t)(v >> 4) * 10u;
}

struct rtc_regs {
    uint8_t s, mi, h, d, mo, y, c;
};

static void read_regs(struct rtc_regs *r)
{
    for (unsigned i = 0; i < 1000000u && (cmos(0x0A) & 0x80u); i++)
        ; /* update in progress */
    r->s = cmos(0x00);
    r->mi = cmos(0x02);
    r->h = cmos(0x04);
    r->d = cmos(0x07);
    r->mo = cmos(0x08);
    r->y = cmos(0x09);
    r->c = cmos(0x32);
}

uint64_t nx_rtc_read(void)
{
    struct rtc_regs a, b;
    read_regs(&a);
    for (unsigned tries = 0; tries < 8; tries++) { /* stable across two reads */
        read_regs(&b);
        if (a.s == b.s && a.mi == b.mi && a.h == b.h && a.d == b.d && a.mo == b.mo &&
            a.y == b.y && a.c == b.c)
            break;
        a = b;
    }
    uint8_t fmt = cmos(0x0B);
    int binary = (fmt & 0x04u) != 0, h24 = (fmt & 0x02u) != 0;
    uint32_t pm = !h24 && (a.h & 0x80u);
    uint32_t hour = from_bcd((uint8_t)(a.h & 0x7Fu), binary);
    if (!h24)
        hour = hour % 12u + (pm ? 12u : 0u);
    uint32_t year = from_bcd(a.y, binary) + (a.c ? from_bcd(a.c, binary) * 100u : 2000u);
    uint32_t mo = from_bcd(a.mo, binary), d = from_bcd(a.d, binary);
    uint32_t mi = from_bcd(a.mi, binary), s = from_bcd(a.s, binary);
    if (mo < 1 || mo > 12 || d < 1 || d > 31 || hour > 23 || mi > 59 || s > 60 || year < 1970)
        return 0;
    return (uint64_t)nx_unix_time(year, mo, d, hour, mi, s);
}

void nx_rtc_init(void)
{
    base_unix = nx_rtc_read();
    base_ticks = nx_timer_ticks;
}

uint64_t nx_rtc_now(uint64_t ticks, uint32_t hz)
{
    if (!base_unix || !hz)
        return 0;
    return base_unix + (ticks - base_ticks) / hz;
}
