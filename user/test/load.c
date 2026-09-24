/*
 * bin/load — M3 test workload (docs/m3-core.md).  Announces itself once,
 * then computes forever without system calls: it never exits on its own,
 * so only a terminate request can stop it, and the CPU time it gets is
 * what task.measure observes.
 */
#include <nanox/m2test.h>

#include "nanox_user.h"

int64_t umain(uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3)
{
    u_printf("load started: computing until terminated\n");
    volatile uint64_t x = 0x9E3779B97F4A7C15ull;
    for (;;)
        x = nx_m2_spin_step(x);
}
