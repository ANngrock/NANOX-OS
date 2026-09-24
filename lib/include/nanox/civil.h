/* Proleptic Gregorian calendar -> seconds since 1970-01-01T00:00:00Z (UTC,
 * no leap seconds).  Shared by the kernel's RTC clock and the X.509 time
 * parser (M5); pure, host-tested. */
#ifndef NANOX_LIB_CIVIL_H
#define NANOX_LIB_CIVIL_H

#include <stdint.h>

/* Days since 1970-01-01 of year-month-day (month 1..12, day 1..31), after
 * H. Hinnant's days_from_civil.  Valid for years 1..9999. */
static inline int64_t nx_days_from_civil(int64_t y, uint32_t m, uint32_t d)
{
    y -= m <= 2;
    int64_t era = (y >= 0 ? y : y - 399) / 400;
    uint32_t yoe = (uint32_t)(y - era * 400);
    uint32_t doy = (153u * (m > 2 ? m - 3 : m + 9) + 2u) / 5u + d - 1u;
    uint32_t doe = yoe * 365u + yoe / 4u - yoe / 100u + doy;
    return era * 146097 + (int64_t)doe - 719468;
}

static inline int64_t nx_unix_time(int64_t y, uint32_t mo, uint32_t d, uint32_t h, uint32_t mi,
                                   uint32_t s)
{
    return nx_days_from_civil(y, mo, d) * 86400 + (int64_t)h * 3600 + (int64_t)mi * 60 + s;
}

#endif
