/*
 * virtio-net and virtio-rng drivers (M5) over the virtio 1.x PCI transport
 * (virtio.h), polled.  Decisions: docs/m5-net.md §3.
 *
 * virtio-net: features VERSION_1, MAC and STATUS; no checksum or
 * segmentation offload and no mergeable buffers, so every frame is a
 * complete Ethernet frame of at most 1514 bytes behind the 12-byte
 * virtio_net_hdr of VERSION_1.  Receive queue 0 keeps NX_VNET_RX_BUFS
 * device-writable pages posted; the transmit queue 1 sends one frame at a
 * time and waits for its completion.  The link state is the STATUS field
 * of the device configuration (QEMU `set_link`).
 *
 * The netdev object handed to bin/core (NX_OBJ_NETDEV, rights READ =
 * receive/info, WRITE = send) sits on top, together with the M5 test
 * layer: deterministic loss of every N-th received or sent frame
 * (nanox.m5.loss=rx:N,tx:M).
 *
 * virtio-rng: one request queue; the device fills a page-sized buffer
 * with bytes from QEMU's random source (backend rng-builtin, i.e. the
 * host's getrandom()).
 */
#ifndef NANOX_KERNEL_DEV_VIRTIO_NET_H
#define NANOX_KERNEL_DEV_VIRTIO_NET_H

#include <stdint.h>

#include <nanox/syscall.h>

#include "obj/handle.h"
#include "virtio.h"

#define NX_VNET_RX_BUFS 16u
#define NX_VNET_HDR 12u

struct nx_netdev {
    struct nx_object base; /* NX_OBJ_NETDEV, never freed */
    int present;
    struct nx_virtio v;
    struct nx_virtq rxq, txq;
    uint64_t rx_pa[NX_VNET_RX_BUFS];
    uint64_t tx_pa;
    uint8_t mac[6];
    int has_status;
    /* counters (struct nx_net_info) */
    uint64_t rx_frames, tx_frames, rx_bytes, tx_bytes;
    uint64_t rx_test_drops, tx_test_drops, tx_link_down, rx_oversize, tx_errors;
    /* test layer: drop every N-th frame (0: off) */
    uint32_t loss_rx, loss_tx;
    uint64_t rx_seen, tx_seen;
};

/* Finds and initialises the first virtio-net function (idempotent) and
 * prints "NANOX: net ..." about it.  NULL if there is none. */
struct nx_netdev *nx_net_open(void);
/* 1 if the device reports the link up (always 1 without STATUS). */
int nx_net_link_up(struct nx_netdev *n);
/* Sends one frame (14..NX_NET_FRAME_MAX bytes): NX_OK, NX_ENOLINK (link
 * down: nothing sent), NX_EIO. */
int nx_net_send(struct nx_netdev *n, const uint8_t *frame, uint32_t len);
/* Receives one frame into buf (NX_NET_FRAME_MAX bytes) without waiting:
 * its length, or 0 if none is pending. */
uint32_t nx_net_poll(struct nx_netdev *n, uint8_t *buf);
void nx_net_info(struct nx_netdev *n, struct nx_net_info *out);

/* virtio-rng: fills buf with len (<= NX_ENTROPY_MAX) bytes from the
 * device.  NX_OK, NX_ENODEV (no device), NX_EIO. */
int nx_rng_read(uint8_t *buf, uint32_t len);
/* Probes the virtio-rng device (idempotent); 1 if present.  Prints
 * "NANOX: rng ...". */
int nx_rng_open(void);

#endif
