/*
 * Agreement between the kernel's M3 test modes (kernel/m3test.c,
 * nanox.test=m3-*) and the Cognitive Core executor bin/core
 * (user/core/).  Not part of the system call ABI.  Design:
 * docs/m3-core.md.
 */
#ifndef NANOX_ABI_M3_H
#define NANOX_ABI_M3_H

/* bin/core arguments: a0 = Sovereign handle, a1 = bridge channel handle,
 * a2 = flags below, a3 = 0. */
#define NX_M3_CORE_NODEDUP 1u /* negative control: requests are not deduplicated */

/* bin/core exit codes. */
#define NX_M3_CORE_OK 0            /* session closed, host checks ok, no verification failure */
#define NX_M3_CORE_VERIFY_FAILED 3 /* at least one action failed its end-state verification */
#define NX_M3_CORE_HOST_FAILED 4   /* the host bridge reported failed host-side checks */
#define NX_M3_CORE_BRIDGE_LOST 5   /* no request from the host within the idle limit */
#define NX_M3_CORE_BAD_ARGS 6

#endif
