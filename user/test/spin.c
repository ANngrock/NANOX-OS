/*
 * bin/spin — M2 criterion 2 (scenario m2-sched).  A compute loop that never
 * yields and never blocks: only the timer can take the CPU away from it.
 * Prints a progress line after every quarter and the result at the end.
 * a0 = index, a1 = iterations, a2 = seed, a3 = expected result.  Exit code
 * 0 if the result matches (the loop state survived every preemption),
 * 3 otherwise.
 */
#include <nanox/m2test.h>

#include "nanox_user.h"

int64_t umain(uint64_t index, uint64_t iterations, uint64_t seed, uint64_t expected)
{
    uint64_t x = seed, chunk = iterations / NX_M2_SPIN_REPORTS;
    for (unsigned r = 1; r <= NX_M2_SPIN_REPORTS; r++) {
        uint64_t n = r == NX_M2_SPIN_REPORTS ? iterations - chunk * (NX_M2_SPIN_REPORTS - 1) : chunk;
        for (uint64_t i = 0; i < n; i++)
            x = nx_m2_spin_step(x);
        u_printf("progress %u/%u\n", r, NX_M2_SPIN_REPORTS);
    }
    int ok = x == expected;
    u_printf("done index=%lu iterations=%lu result=0x%016lx %s\n", index, iterations, x,
             ok ? "ok" : "MISMATCH");
    return ok ? 0 : 3;
}
