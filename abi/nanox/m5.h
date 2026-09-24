/*
 * Agreement between the kernel's M5 test modes (kernel/m5test.c,
 * nanox.test=m5-*) and bin/core (user/core/).  Not part of the system call
 * ABI.  Design: docs/m5-net.md.
 *
 * bin/core arguments in M5 modes: a0 = Sovereign handle, a1 = console
 * channel handle (COM2), a2 = flags (low 32 bits: NX_M3_CORE_*,
 * NX_M4_CORE_*, NX_M5_CORE_*) | network device handle << 32, a3 = block
 * device handle of the data disk (0: no store).
 */
#ifndef NANOX_ABI_M5_H
#define NANOX_ABI_M5_H

#define NX_M5_CORE_NET 8u        /* a network device handle is in a2 >> 32 */
/* Negative controls (each must make a scenario check fail). */
#define NX_M5_CORE_NORETX 16u    /* TCP never retransmits */
#define NX_M5_CORE_NORETRY 32u   /* the provider client makes one attempt only */
#define NX_M5_CORE_FLATTEL 64u   /* telemetry does not classify failures */

#define NX_M5_CORE_FLAGS_MASK 0xFFFFFFFFull
#define NX_M5_CORE_NET_SHIFT 32u

#endif
