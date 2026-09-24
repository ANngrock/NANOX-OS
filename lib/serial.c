#include <nanox/port.h>
#include <nanox/serial.h>

#define COM1 0x3F8u

enum {
    UART_DATA = 0, /* THR / RBR, DLL when DLAB=1 */
    UART_IER = 1,  /* DLM when DLAB=1 */
    UART_FCR = 2,
    UART_LCR = 3,
    UART_MCR = 4,
    UART_LSR = 5,
};

#define LSR_THRE 0x20u
/* Bounded wait so a missing UART cannot hang the boot path forever. */
#define TX_SPIN_LIMIT 100000u

void nx_serial_init(void)
{
    nx_outb(COM1 + UART_IER, 0x00);  /* no interrupts */
    nx_outb(COM1 + UART_LCR, 0x80);  /* DLAB */
    nx_outb(COM1 + UART_DATA, 0x01); /* divisor 1 = 115200 baud */
    nx_outb(COM1 + UART_IER, 0x00);
    nx_outb(COM1 + UART_LCR, 0x03); /* 8N1 */
    nx_outb(COM1 + UART_FCR, 0xC7); /* FIFO on, cleared, 14-byte threshold */
    nx_outb(COM1 + UART_MCR, 0x03); /* DTR, RTS */
}

static void tx(char c)
{
    for (unsigned i = 0; i < TX_SPIN_LIMIT; i++) {
        if (nx_inb(COM1 + UART_LSR) & LSR_THRE)
            break;
    }
    nx_outb(COM1 + UART_DATA, (unsigned char)c);
}

void nx_serial_putc(char c)
{
    if (c == '\n')
        tx('\r');
    tx(c);
}

void nx_serial_write(const char *s)
{
    while (*s)
        nx_serial_putc(*s++);
}
