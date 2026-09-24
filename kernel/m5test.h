/*
 * M5 test modes (nanox.test=m5-*): bin/core with the network device, the
 * data disk and the console channel; it talks to the model provider over
 * its own network stack.  Scenarios: docs/m5-net.md.
 */
#ifndef NANOX_KERNEL_M5TEST_H
#define NANOX_KERNEL_M5TEST_H

__attribute__((noreturn)) void nx_m5_serve(void);
__attribute__((noreturn)) void nx_m5_noretx(void);
__attribute__((noreturn)) void nx_m5_noretry(void);
__attribute__((noreturn)) void nx_m5_flattel(void);

#endif
