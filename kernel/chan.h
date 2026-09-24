/*
 * Host bridge channel (M3): the second serial port (COM2, I/O 0x2F8) of the
 * QEMU bench, connected by the harness to the host-side bridge
 * (tools/bridge/).  Polled: no interrupts, the reader sleeps one timer tick
 * between polls.  The boundary between guest and host is described in
 * docs/m3-core.md.
 */
#ifndef NANOX_KERNEL_CHAN_H
#define NANOX_KERNEL_CHAN_H

#include <stdint.h>

#include "obj/handle.h"

struct nx_chan {
    struct nx_object base; /* type NX_OBJ_CHANNEL, never freed */
    uint16_t port;
    uint64_t rx_bytes, tx_bytes;
};

/* Initialises COM2 and checks that a UART answers there (scratch register
 * round trip).  Returns the channel object or NULL when there is no port. */
struct nx_chan *nx_chan_init(void);
/* Non-blocking: returns the number of bytes copied (0: nothing pending). */
uint32_t nx_chan_poll(struct nx_chan *c, uint8_t *buf, uint32_t cap);
void nx_chan_write(struct nx_chan *c, const uint8_t *buf, uint32_t len);

#endif
