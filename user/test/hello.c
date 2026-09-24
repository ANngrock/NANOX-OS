/*
 * bin/hello — M2 criterion 1 (scenario m2-user).  Runs in ring 3 in its own
 * address space, reports its privilege level, arguments and addresses
 * through NX_SYS_DEBUG_WRITE and exits with code 0 when everything it can
 * check itself holds (docs/m2-kernel.md).
 */
#include <nanox/m2test.h>

#include "nanox_user.h"

static volatile uint64_t counter = 5; /* .data: loaded from the ELF file, writable */
static volatile uint64_t zeroed[64]; /* .bss: zero-filled by the kernel */
static const char banner[] = "hello from ring 3";

int64_t umain(uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3)
{
    uint64_t cpl = u_cpl();
    int64_t self = nx_task_self();
    uint64_t sp = (uint64_t)(uintptr_t)__builtin_frame_address(0);
    int args_ok = a0 == NX_M2_HELLO_ARG0 && a1 == NX_M2_HELLO_ARG1 && a2 == NX_M2_HELLO_ARG2 &&
                  a3 == NX_M2_HELLO_ARG3;
    int bss_ok = 1;
    for (unsigned i = 0; i < 64; i++)
        bss_ok &= zeroed[i] == 0;
    counter *= 7; /* writes .data */

    u_printf("%s: cpl=%lu task=%ld\n", banner, cpl, self);
    u_printf("args %s: 0x%lx 0x%lx 0x%lx 0x%lx\n", args_ok ? "ok" : "WRONG", a0, a1, a2, a3);
    u_printf("code=0x%lx data=0x%lx stack=0x%lx data_ok=%s bss_ok=%s\n",
             (uint64_t)(uintptr_t)umain, (uint64_t)(uintptr_t)&counter, sp,
             counter == 35 ? "yes" : "no", bss_ok ? "yes" : "no");
    int ok = cpl == 3 && self > 0 && args_ok && counter == 35 && bss_ok;
    u_printf("exiting with code %d\n", ok ? 0 : 1);
    return ok ? 0 : 1;
}
