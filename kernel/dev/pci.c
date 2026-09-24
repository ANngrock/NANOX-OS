#include <nanox/port.h>

#include "pci.h"

#define CONFIG_ADDRESS 0xCF8u
#define CONFIG_DATA 0xCFCu

static void cfg_select(struct nx_pci_addr a, uint32_t off)
{
    nx_outl(CONFIG_ADDRESS, 0x80000000u | (uint32_t)a.bus << 16 | (uint32_t)a.dev << 11 |
                                (uint32_t)a.fn << 8 | (off & 0xFCu));
}

uint32_t nx_pci_read32(struct nx_pci_addr a, uint32_t off)
{
    cfg_select(a, off);
    return nx_inl(CONFIG_DATA);
}

uint16_t nx_pci_read16(struct nx_pci_addr a, uint32_t off)
{
    return (uint16_t)(nx_pci_read32(a, off) >> ((off & 2u) * 8));
}

uint8_t nx_pci_read8(struct nx_pci_addr a, uint32_t off)
{
    return (uint8_t)(nx_pci_read32(a, off) >> ((off & 3u) * 8));
}

void nx_pci_write16(struct nx_pci_addr a, uint32_t off, uint16_t v)
{
    cfg_select(a, off);
    nx_outw((uint16_t)(CONFIG_DATA + (off & 2u)), v);
}

uint32_t nx_pci_scan(uint16_t vendor, int (*fn)(void *ctx, struct nx_pci_addr a, uint16_t device),
                     void *ctx)
{
    uint32_t n = 0;
    for (uint8_t dev = 0; dev < 32; dev++) {
        for (uint8_t f = 0; f < 8; f++) {
            struct nx_pci_addr a = {0, dev, f};
            uint32_t id = nx_pci_read32(a, 0);
            if ((id & 0xFFFFu) == 0xFFFFu) {
                if (f == 0)
                    break; /* no device in this slot */
                continue;
            }
            if ((id & 0xFFFFu) == vendor) {
                n++;
                if (fn(ctx, a, (uint16_t)(id >> 16)))
                    return n;
            }
            if (f == 0 && !(nx_pci_read8(a, 0x0E) & 0x80u))
                break; /* single-function device */
        }
    }
    return n;
}

uint64_t nx_pci_bar(struct nx_pci_addr a, uint32_t bar)
{
    if (bar > 5)
        return 0;
    uint32_t lo = nx_pci_read32(a, 0x10u + bar * 4u);
    if (lo & 1u)
        return 0; /* I/O BAR */
    uint64_t base = lo & ~0xFull;
    if (((lo >> 1) & 3u) == 2u && bar < 5)
        base |= (uint64_t)nx_pci_read32(a, 0x10u + (bar + 1u) * 4u) << 32;
    return base;
}
