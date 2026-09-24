/*
 * virtio-blk driver (M4): virtio 1.x PCI transport ("modern" interface,
 * VIRTIO spec v1.3 §4.1), one request queue, polled completion (the kernel
 * has no interrupt routing for devices yet; the request is issued and the
 * used ring is polled inside the system call, with interrupts disabled).
 * Requests: IN, OUT, FLUSH, GET_ID.  Decisions: docs/m4-store.md §3.
 *
 * DMA: the device reads and writes only the driver's own pages (queue,
 * request header, bounce pages); the virtual device is trusted
 * (ARCHITECTURE.md §5.1: no IOMMU on the first bench).
 */
#ifndef NANOX_KERNEL_DEV_VIRTIO_BLK_H
#define NANOX_KERNEL_DEV_VIRTIO_BLK_H

#include <stdint.h>

#include "virtio.h"

#define NX_VBLK_MAX 2u          /* devices the driver manages */
#define NX_VBLK_SECTOR 512u
#define NX_VBLK_MAX_PAGES 8u    /* data pages per request */
#define NX_VBLK_SERIAL_MAX 20u

enum nx_vblk_status {
    NX_VBLK_OK = 0,
    NX_VBLK_E_IO = 1,      /* device reported VIRTIO_BLK_S_IOERR */
    NX_VBLK_E_UNSUPP = 2,  /* device reported VIRTIO_BLK_S_UNSUPP (e.g. write to read-only) */
    NX_VBLK_E_TIMEOUT = 3, /* no completion within the polling limit */
    NX_VBLK_E_RANGE = 4,   /* request beyond the capacity */
};

struct nx_vblk {
    int present;
    struct nx_virtio v;
    struct nx_virtq q;
    uint64_t sectors;
    int read_only, has_flush;
    char serial[NX_VBLK_SERIAL_MAX + 1];
    uint64_t req_pa;
    uint64_t data_pa[NX_VBLK_MAX_PAGES];
    uint64_t requests, errors;
};

extern struct nx_vblk nx_vblk_dev[NX_VBLK_MAX];
extern uint32_t nx_vblk_count;

/* Finds and initialises every virtio-blk function on bus 0 (idempotent).
 * Prints one "NANOX: blk ..." line per device.  Returns the count. */
uint32_t nx_vblk_probe(void);
/* Device whose GET_ID serial equals `serial`, or NULL. */
struct nx_vblk *nx_vblk_by_serial(const char *serial);

/* Synchronous requests on up to NX_VBLK_MAX_PAGES bounce pages; the data of
 * page i is at nx_vblk_page(d, i).  `bytes` is a multiple of 512. */
void *nx_vblk_page(struct nx_vblk *d, uint32_t i);
int nx_vblk_read(struct nx_vblk *d, uint64_t sector, uint32_t bytes);
int nx_vblk_write(struct nx_vblk *d, uint64_t sector, uint32_t bytes);
int nx_vblk_flush(struct nx_vblk *d);
const char *nx_vblk_strerror(int st);

#endif
