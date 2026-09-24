#include <nanox/port.h>

#include "chan.h"

#define COM2 0x2F8u

enum { UART_DATA = 0, UART_IER = 1, UART_FCR = 2, UART_LCR = 3, UART_MCR = 4, UART_LSR = 5,
       UART_SCR = 7 };

#define LSR_DR 0x01u
#define LSR_THRE 0x20u
#define TX_SPIN_LIMIT 1000000u

static struct nx_chan chan = {{NX_OBJ_CHANNEL, 0, 0}, COM2, 0, 0}; /* refs 0: never freed */

struct nx_chan *nx_chan_init(void)
{
    /* An absent port reads back 0xFF; a 16550 keeps the scratch value. */
    nx_outb(COM2 + UART_SCR, 0x5A);
    if (nx_inb(COM2 + UART_SCR) != 0x5A)
        return 0;
    nx_outb(COM2 + UART_SCR, 0xA5);
    if (nx_inb(COM2 + UART_SCR) != 0xA5)
        return 0;
    nx_outb(COM2 + UART_IER, 0x00);  /* polled */
    nx_outb(COM2 + UART_LCR, 0x80);  /* DLAB */
    nx_outb(COM2 + UART_DATA, 0x01); /* 115200 baud (irrelevant for QEMU) */
    nx_outb(COM2 + UART_IER, 0x00);
    nx_outb(COM2 + UART_LCR, 0x03);  /* 8N1 */
    nx_outb(COM2 + UART_FCR, 0x07);  /* FIFO on, both cleared */
    nx_outb(COM2 + UART_MCR, 0x03);  /* DTR, RTS */
    return &chan;
}

uint32_t nx_chan_poll(struct nx_chan *c, uint8_t *buf, uint32_t cap)
{
    uint32_t n = 0;
    while (n < cap && (nx_inb(c->port + UART_LSR) & LSR_DR))
        buf[n++] = nx_inb(c->port + UART_DATA);
    c->rx_bytes += n;
    return n;
}

void nx_chan_write(struct nx_chan *c, const uint8_t *buf, uint32_t len)
{
    for (uint32_t i = 0; i < len; i++) {
        for (unsigned s = 0; s < TX_SPIN_LIMIT; s++)
            if (nx_inb(c->port + UART_LSR) & LSR_THRE)
                break;
        nx_outb(c->port + UART_DATA, buf[i]);
    }
    c->tx_bytes += len;
}
