/* COM1 (0x3F8) 16550 UART, polled output only. */
#ifndef NANOX_LIB_SERIAL_H
#define NANOX_LIB_SERIAL_H

void nx_serial_init(void);
/* Writes one byte; '\n' is sent as "\r\n". */
void nx_serial_putc(char c);
void nx_serial_write(const char *s);

#endif
