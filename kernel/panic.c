#include <stdarg.h>

#include <nanox/diag.h>
#include <nanox/port.h>
#include <nanox/printf.h>

#include "kernel.h"

void nx_debug_exit(uint8_t code)
{
    nx_outb(NX_DEBUG_EXIT_PORT, code);
    /* Not under QEMU (or the device is missing): stop here. */
    for (;;)
        __asm__ volatile("cli; hlt");
}

void nx_panic(const char *fmt, ...)
{
    __asm__ volatile("cli");
    va_list ap;
    va_start(ap, fmt);
    nx_printf("NANOX: PANIC ");
    nx_vprintf(fmt, ap);
    nx_printf("\n");
    va_end(ap);
    nx_debug_exit(NX_EXIT_PANIC);
}
