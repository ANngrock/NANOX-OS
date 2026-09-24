/*
 * virtio-net and virtio-rng, polled; see virtio_net.h.
 *
 * Runs inside system calls with interrupts disabled (single CPU, no
 * preemption) or in the kernel's test controller before bin/core starts,
 * so the static state needs no locking.
 */
#include <nanox/printf.h>
#include <nanox/string.h>

#include "kernel.h"
#include "virtio_net.h"

#define VIRTIO_DEV_NET_LEGACY 0x1000u /* transitional */
#define VIRTIO_DEV_NET_MODERN 0x1041u
#define VIRTIO_DEV_RNG_LEGACY 0x1005u
#define VIRTIO_DEV_RNG_MODERN 0x1044u

#define F_NET_MAC (1ull << 5)
#define F_NET_STATUS (1ull << 16)
#define NET_S_LINK_UP 1u

#define RXQ 0u
#define TXQ 1u
#define TX_POLL_LIMIT 100000000ull
#define RNG_POLL_LIMIT 100000000ull

static struct nx_netdev net_dev; /* object never freed (refs from handles only) */
static int net_probed;

static void post_rx(struct nx_netdev *n, uint16_t i)
{
    struct nx_virtq_desc *d = nx_virtq_desc(&n->rxq);
    d[i].addr = n->rx_pa[i];
    d[i].len = 4096;
    d[i].flags = NX_VIRTQ_DESC_WRITE;
    d[i].next = 0;
    nx_virtq_submit(&n->v, &n->rxq, i);
}

static const char *init_net(struct nx_netdev *n)
{
    const char *err = nx_virtio_start(&n->v, F_NET_MAC | F_NET_STATUS);
    if (err)
        return err;
    if (!(n->v.accepted & F_NET_MAC) || !n->v.devcfg)
        return "no MAC address in the device configuration";
    n->has_status = (n->v.accepted & F_NET_STATUS) != 0;
    err = nx_virtio_queue(&n->v, &n->rxq, RXQ, NX_VNET_RX_BUFS);
    if (!err)
        err = nx_virtio_queue(&n->v, &n->txq, TXQ, 2);
    if (err)
        return err;
    if (n->rxq.size < NX_VNET_RX_BUFS)
        return "receive queue too small";
    for (uint32_t i = 0; i < NX_VNET_RX_BUFS; i++) {
        n->rx_pa[i] = nx_page_alloc();
        memset(nx_phys_to_virt(n->rx_pa[i]), 0, 4096);
    }
    n->tx_pa = nx_page_alloc();
    memset(nx_phys_to_virt(n->tx_pa), 0, 4096);
    uint8_t gen;
    do {
        gen = nx_virtio_cfg_gen(&n->v);
        for (uint32_t i = 0; i < 6; i++)
            n->mac[i] = *(volatile uint8_t *)(n->v.devcfg + i);
    } while (gen != nx_virtio_cfg_gen(&n->v));
    nx_virtio_ready(&n->v);
    for (uint16_t i = 0; i < NX_VNET_RX_BUFS; i++)
        post_rx(n, i);
    return 0;
}

static int probe_net(void *ctx, struct nx_pci_addr a, uint16_t device)
{
    (void)ctx;
    if (device != VIRTIO_DEV_NET_LEGACY && device != VIRTIO_DEV_NET_MODERN)
        return 0;
    struct nx_netdev *n = &net_dev;
    n->v.pci = a;
    n->v.device_id = device;
    const char *err = init_net(n);
    if (err) {
        nx_printf("NANOX: net %02x:%02x.%u virtio-net init failed: %s\n", a.bus, a.dev, a.fn,
                  err);
        return 1;
    }
    n->present = 1;
    nx_printf("NANOX: net %02x:%02x.%u virtio-net id=0x%x mac=%02x:%02x:%02x:%02x:%02x:%02x"
              " features=0x%" NX_PRIx64 " status=%s link=%s rxq=%u txq=%u\n",
              a.bus, a.dev, a.fn, device, n->mac[0], n->mac[1], n->mac[2], n->mac[3], n->mac[4],
              n->mac[5], n->v.features, n->has_status ? "yes" : "no",
              nx_net_link_up(n) ? "up" : "down", n->rxq.size, n->txq.size);
    return 1;
}

struct nx_netdev *nx_net_open(void)
{
    if (!net_probed) {
        net_probed = 1;
        net_dev.base.type = NX_OBJ_NETDEV;
        nx_pci_scan(NX_VIRTIO_VENDOR, probe_net, 0);
    }
    return net_dev.present ? &net_dev : 0;
}

int nx_net_link_up(struct nx_netdev *n)
{
    if (!n->has_status)
        return 1;
    uint16_t st = *(volatile uint16_t *)(n->v.devcfg + 6);
    return (st & NET_S_LINK_UP) != 0;
}

int nx_net_send(struct nx_netdev *n, const uint8_t *frame, uint32_t len)
{
    if (!nx_net_link_up(n)) {
        n->tx_link_down++;
        return NX_ENOLINK;
    }
    n->tx_seen++;
    if (n->loss_tx && n->tx_seen % n->loss_tx == 0) {
        n->tx_test_drops++;
        return NX_OK; /* lost on the wire: the sender cannot tell */
    }
    uint8_t *p = nx_phys_to_virt(n->tx_pa);
    memset(p, 0, NX_VNET_HDR);
    memcpy(p + NX_VNET_HDR, frame, len);
    struct nx_virtq_desc *d = nx_virtq_desc(&n->txq);
    d[0].addr = n->tx_pa;
    d[0].len = NX_VNET_HDR + len;
    d[0].flags = 0;
    d[0].next = 0;
    nx_virtq_submit(&n->v, &n->txq, 0);
    struct nx_virtq_used_elem e;
    for (uint64_t spins = 0; !nx_virtq_poll(&n->txq, &e); spins++) {
        if (spins >= TX_POLL_LIMIT) {
            n->tx_errors++;
            return NX_EIO;
        }
        __asm__ volatile("pause");
    }
    n->tx_frames++;
    n->tx_bytes += len;
    return NX_OK;
}

uint32_t nx_net_poll(struct nx_netdev *n, uint8_t *buf)
{
    struct nx_virtq_used_elem e;
    while (nx_virtq_poll(&n->rxq, &e)) {
        uint16_t id = (uint16_t)e.id;
        if (id >= NX_VNET_RX_BUFS)
            continue; /* not ours: never happens with a correct device */
        uint32_t len = e.len > NX_VNET_HDR ? e.len - NX_VNET_HDR : 0;
        int keep = 1;
        if (len < NX_NET_FRAME_MIN || len > NX_NET_FRAME_MAX) {
            n->rx_oversize++;
            keep = 0;
        } else {
            n->rx_seen++;
            if (n->loss_rx && n->rx_seen % n->loss_rx == 0) {
                n->rx_test_drops++;
                keep = 0;
            }
        }
        if (keep)
            memcpy(buf, (uint8_t *)nx_phys_to_virt(n->rx_pa[id]) + NX_VNET_HDR, len);
        post_rx(n, id);
        if (keep) {
            n->rx_frames++;
            n->rx_bytes += len;
            return len;
        }
    }
    return 0;
}

void nx_net_info(struct nx_netdev *n, struct nx_net_info *o)
{
    memset(o, 0, sizeof(*o));
    memcpy(o->mac, n->mac, 6);
    o->mtu = 1500;
    o->flags = (nx_net_link_up(n) ? NX_NET_INFO_LINK_UP : 0) |
               (n->has_status ? NX_NET_INFO_STATUS : 0) |
               (n->loss_rx || n->loss_tx ? NX_NET_INFO_TEST_LOSS : 0);
    o->loss_rx = n->loss_rx;
    o->loss_tx = n->loss_tx;
    o->rx_frames = n->rx_frames;
    o->tx_frames = n->tx_frames;
    o->rx_bytes = n->rx_bytes;
    o->tx_bytes = n->tx_bytes;
    o->rx_test_drops = n->rx_test_drops;
    o->tx_test_drops = n->tx_test_drops;
    o->tx_link_down = n->tx_link_down;
    o->rx_oversize = n->rx_oversize;
    o->tx_errors = n->tx_errors;
}

/* ---- virtio-rng ------------------------------------------------------------------ */

static struct {
    int present;
    struct nx_virtio v;
    struct nx_virtq q;
    uint64_t buf_pa;
} rng;
static int rng_probed;

static int probe_rng(void *ctx, struct nx_pci_addr a, uint16_t device)
{
    (void)ctx;
    if (device != VIRTIO_DEV_RNG_LEGACY && device != VIRTIO_DEV_RNG_MODERN)
        return 0;
    rng.v.pci = a;
    rng.v.device_id = device;
    const char *err = nx_virtio_start(&rng.v, 0);
    if (!err)
        err = nx_virtio_queue(&rng.v, &rng.q, 0, 2);
    if (err) {
        nx_printf("NANOX: rng %02x:%02x.%u virtio-rng init failed: %s\n", a.bus, a.dev, a.fn,
                  err);
        return 1;
    }
    rng.buf_pa = nx_page_alloc();
    memset(nx_phys_to_virt(rng.buf_pa), 0, 4096);
    nx_virtio_ready(&rng.v);
    rng.present = 1;
    nx_printf("NANOX: rng %02x:%02x.%u virtio-rng id=0x%x features=0x%" NX_PRIx64 "\n", a.bus,
              a.dev, a.fn, device, rng.v.features);
    return 1;
}

int nx_rng_open(void)
{
    if (!rng_probed) {
        rng_probed = 1;
        nx_pci_scan(NX_VIRTIO_VENDOR, probe_rng, 0);
    }
    return rng.present;
}

int nx_rng_read(uint8_t *buf, uint32_t len)
{
    if (!rng.present)
        return NX_ENODEV;
    uint32_t got = 0, empty = 0;
    while (got < len) {
        struct nx_virtq_desc *d = nx_virtq_desc(&rng.q);
        d[0].addr = rng.buf_pa;
        d[0].len = len - got;
        d[0].flags = NX_VIRTQ_DESC_WRITE;
        d[0].next = 0;
        nx_virtq_submit(&rng.v, &rng.q, 0);
        struct nx_virtq_used_elem e;
        for (uint64_t spins = 0; !nx_virtq_poll(&rng.q, &e); spins++) {
            if (spins >= RNG_POLL_LIMIT)
                return NX_EIO;
            __asm__ volatile("pause");
        }
        if (e.len == 0 && ++empty > 16)
            return NX_EIO;
        uint32_t n = e.len < len - got ? e.len : len - got;
        memcpy(buf + got, nx_phys_to_virt(rng.buf_pa), n);
        got += n;
    }
    return NX_OK;
}
