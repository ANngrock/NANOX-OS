/*
 * Test protocol between the kernel's M2 test modes (kernel/m2test.c,
 * nanox.test=m2-*) and the test programs in user/test/.  Not part of the
 * system ABI: these are the arguments and expected values both sides agree
 * on.  Scenarios: docs/m2-kernel.md.
 */
#ifndef NANOX_ABI_M2TEST_H
#define NANOX_ABI_M2TEST_H

#include <stdint.h>

#include <nanox/syscall.h>

/* bin/hello: the four argument registers the kernel sets (RDI, RSI, RDX, RCX). */
#define NX_M2_HELLO_ARG0 ((uint64_t)0x48454C4C4F000000ull)
#define NX_M2_HELLO_ARG1 ((uint64_t)0x48454C4C4F000001ull)
#define NX_M2_HELLO_ARG2 ((uint64_t)0x48454C4C4F000002ull)
#define NX_M2_HELLO_ARG3 ((uint64_t)0x48454C4C4F000003ull)

/* bin/spin: a0 = index, a1 = iterations, a2 = seed, a3 = expected result.
 * The loop is xorshift64 (Marsaglia), which the compiler cannot reduce to a
 * closed form; the kernel computes the expected result independently. */
#define NX_M2_SPIN_REPORTS 4u
static inline uint64_t nx_m2_spin_step(uint64_t x)
{
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    return x;
}

/* bin/ipc-send and bin/ipc-recv.  ipc-send creates a one-page memory
 * object, fills it, maps nothing else and sends NX_M2_IPC_MSG with the
 * object's handle reduced to NX_M2_IPC_GRANT; ipc-recv checks both and maps
 * the page read-only.  Arguments: ipc-send a0 = endpoint handle (SEND);
 * ipc-recv a0 = endpoint handle (RECV), a1 = task id of ipc-send. */
#define NX_M2_IPC_MSG "NANOX M2: one page of memory for you (read-only)"
#define NX_M2_IPC_GRANT (NX_RIGHT_READ | NX_RIGHT_MAP)
#define NX_M2_IPC_SEND_VA ((uint64_t)0x0000010000000000ull)
#define NX_M2_IPC_RECV_VA ((uint64_t)0x0000020000000000ull)
#define NX_M2_IPC_RECV_VA2 ((uint64_t)0x0000020000100000ull)
static inline uint8_t nx_m2_ipc_byte(uint32_t i)
{
    return (uint8_t)(i * 7u + 3u);
}

#endif
