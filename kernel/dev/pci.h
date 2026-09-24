/*
 * PCI configuration space through the legacy mechanism #1 (ports 0xCF8 /
 * 0xCFC), which the q35 machine of the bench provides (M4).  Only bus 0 is
 * scanned: every device of the fixed QEMU configuration sits there.  The
 * firmware (OVMF) has already assigned the BARs; the kernel only reads them.
 */
#ifndef NANOX_KERNEL_DEV_PCI_H
#define NANOX_KERNEL_DEV_PCI_H

#include <stdint.h>

struct nx_pci_addr {
    uint8_t bus, dev, fn;
};

uint32_t nx_pci_read32(struct nx_pci_addr a, uint32_t off);
uint16_t nx_pci_read16(struct nx_pci_addr a, uint32_t off);
uint8_t nx_pci_read8(struct nx_pci_addr a, uint32_t off);
void nx_pci_write16(struct nx_pci_addr a, uint32_t off, uint16_t v);

/* Calls fn for every function on bus 0 with the given vendor id; stops and
 * returns the number of matches visited when fn returns nonzero. */
uint32_t nx_pci_scan(uint16_t vendor, int (*fn)(void *ctx, struct nx_pci_addr a, uint16_t device),
                     void *ctx);
/* Physical base of memory BAR `bar` (64-bit BARs combined); 0 if the BAR
 * is an I/O BAR or unassigned. */
uint64_t nx_pci_bar(struct nx_pci_addr a, uint32_t bar);

#define NX_PCI_COMMAND 0x04u
#define NX_PCI_STATUS 0x06u
#define NX_PCI_CAP_PTR 0x34u
#define NX_PCI_CMD_MEM (1u << 1)
#define NX_PCI_CMD_MASTER (1u << 2)
#define NX_PCI_STATUS_CAPS (1u << 4)

#endif
