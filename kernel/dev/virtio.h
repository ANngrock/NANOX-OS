/*
 * virtio 1.x PCI transport ("modern" interface, VIRTIO spec v1.3 §4.1),
 * shared by the drivers of the fixed QEMU configuration: virtio-blk (M4),
 * virtio-net and virtio-rng (M5).  Split virtqueues of at most
 * NX_VIRTQ_MAX descriptors, one page per queue (descriptor table at 0,
 * available ring at 512, used ring at 1024), completion by polling the used
 * ring: the kernel has no interrupt routing for devices.
 *
 * DMA: a device reads and writes only pages its driver allocated (rings and
 * bounce buffers); the virtual devices of QEMU are trusted (ARCHITECTURE.md
 * §5.1: no IOMMU on the first bench).
 */
#ifndef NANOX_KERNEL_DEV_VIRTIO_H
#define NANOX_KERNEL_DEV_VIRTIO_H

#include <stdint.h>

#include "mm/mm.h"
#include "pci.h"

#define NX_VIRTIO_VENDOR 0x1AF4u
#define NX_VIRTQ_MAX 16u

#define NX_VIRTIO_F_VERSION_1 (1ull << 32)

#define NX_VIRTQ_DESC_NEXT 1u
#define NX_VIRTQ_DESC_WRITE 2u

struct nx_virtq_desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags, next;
};

struct nx_virtq_avail {
    uint16_t flags, idx;
    uint16_t ring[NX_VIRTQ_MAX];
};

struct nx_virtq_used_elem {
    uint32_t id, len;
};

struct nx_virtq_used {
    uint16_t flags, idx;
    struct nx_virtq_used_elem ring[NX_VIRTQ_MAX];
};

#define NX_VIRTQ_AVAIL_OFF 512u
#define NX_VIRTQ_USED_OFF 1024u

struct nx_virtio {
    struct nx_pci_addr pci;
    uint16_t device_id;
    volatile uint8_t *common, *notify_base, *devcfg;
    uint32_t notify_mult;
    uint64_t features; /* offered by the device */
    uint64_t accepted; /* negotiated */
};

struct nx_virtq {
    uint16_t index, size, notify_off;
    uint64_t ring_pa; /* one page */
    uint16_t avail_idx, used_seen;
};

/* Finds the capabilities, maps them, enables memory space and bus
 * mastering, resets the device and negotiates `wanted` (VERSION_1 is
 * required and always added) with what the device offers.  NULL on
 * success (the device is in state FEATURES_OK), else a reason. */
const char *nx_virtio_start(struct nx_virtio *v, uint64_t wanted);
/* Sets up queue `index` with at most `max` (<= NX_VIRTQ_MAX) descriptors
 * on a freshly allocated, zeroed page.  NULL or a reason. */
const char *nx_virtio_queue(struct nx_virtio *v, struct nx_virtq *q, uint16_t index,
                            uint16_t max);
/* DRIVER_OK: the device may start using the queues. */
void nx_virtio_ready(struct nx_virtio *v);
/* Makes descriptor chain `head` available and notifies the device. */
void nx_virtq_submit(struct nx_virtio *v, struct nx_virtq *q, uint16_t head);
/* 1 and the element if the device returned a chain since the last call. */
int nx_virtq_poll(struct nx_virtq *q, struct nx_virtq_used_elem *out);
/* Configuration generation (to read multi-byte device configuration
 * consistently). */
uint8_t nx_virtio_cfg_gen(struct nx_virtio *v);

/* Descriptor table of the queue. */
static inline struct nx_virtq_desc *nx_virtq_desc(struct nx_virtq *q)
{
    return (struct nx_virtq_desc *)nx_phys_to_virt(q->ring_pa);
}

static inline void nx_virtio_mb(void)
{
    __asm__ volatile("mfence" ::: "memory");
}

#endif
