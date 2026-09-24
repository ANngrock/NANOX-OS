/*
 * virtio-blk over the virtio 1.x PCI transport, polled.  See virtio_blk.h.
 */
#include <nanox/printf.h>
#include <nanox/string.h>

#include "kernel.h"
#include "mm/mm.h"
#include "virtio_blk.h"

#define VIRTIO_DEV_BLK_LEGACY 0x1001u /* transitional */
#define VIRTIO_DEV_BLK_MODERN 0x1042u

#define F_BLK_RO (1ull << 5)
#define F_BLK_BLK_SIZE (1ull << 6)
#define F_BLK_FLUSH (1ull << 9)

#define T_IN 0u
#define T_OUT 1u
#define T_FLUSH 4u
#define T_GET_ID 8u

#define POLL_LIMIT 400000000ull

struct blk_req {
    uint32_t type, reserved;
    uint64_t sector;
    uint8_t status;
};

struct nx_vblk nx_vblk_dev[NX_VBLK_MAX];
uint32_t nx_vblk_count;
static int probed;

const char *nx_vblk_strerror(int st)
{
    switch (st) {
    case NX_VBLK_OK: return "ok";
    case NX_VBLK_E_IO: return "ioerr";
    case NX_VBLK_E_UNSUPP: return "unsupp";
    case NX_VBLK_E_TIMEOUT: return "timeout";
    case NX_VBLK_E_RANGE: return "range";
    default: return "?";
    }
}

void *nx_vblk_page(struct nx_vblk *d, uint32_t i)
{
    return nx_phys_to_virt(d->data_pa[i]);
}

/* One request: header, `pages` data descriptors covering `bytes`, status. */
static int request(struct nx_vblk *d, uint32_t type, uint64_t sector, uint32_t bytes)
{
    struct nx_virtq_desc *desc = nx_virtq_desc(&d->q);
    struct blk_req *req = nx_phys_to_virt(d->req_pa);
    uint32_t pages = (bytes + 4095u) / 4096u;
    if (pages > NX_VBLK_MAX_PAGES)
        return NX_VBLK_E_RANGE;
    req->type = type;
    req->reserved = 0;
    req->sector = sector;
    req->status = 0xFF;
    uint16_t n = 0;
    desc[n].addr = d->req_pa;
    desc[n].len = 16;
    desc[n].flags = NX_VIRTQ_DESC_NEXT;
    desc[n].next = (uint16_t)(n + 1);
    n++;
    for (uint32_t i = 0; i < pages; i++) {
        uint32_t len = bytes - i * 4096u < 4096u ? bytes - i * 4096u : 4096u;
        desc[n].addr = d->data_pa[i];
        desc[n].len = len;
        desc[n].flags = (uint16_t)(NX_VIRTQ_DESC_NEXT | (type == T_OUT ? 0u : NX_VIRTQ_DESC_WRITE));
        desc[n].next = (uint16_t)(n + 1);
        n++;
    }
    desc[n].addr = d->req_pa + 16;
    desc[n].len = 1;
    desc[n].flags = NX_VIRTQ_DESC_WRITE;
    desc[n].next = 0;
    nx_virtq_submit(&d->v, &d->q, 0);
    struct nx_virtq_used_elem e;
    for (uint64_t spins = 0; !nx_virtq_poll(&d->q, &e); spins++) {
        if (spins >= POLL_LIMIT) {
            d->errors++;
            return NX_VBLK_E_TIMEOUT;
        }
        __asm__ volatile("pause");
    }
    d->requests++;
    uint8_t s = ((volatile struct blk_req *)req)->status;
    if (s != 0)
        d->errors++;
    return s == 0 ? NX_VBLK_OK : s == 2 ? NX_VBLK_E_UNSUPP : NX_VBLK_E_IO;
}

int nx_vblk_read(struct nx_vblk *d, uint64_t sector, uint32_t bytes)
{
    if (bytes == 0 || bytes % NX_VBLK_SECTOR || sector + bytes / NX_VBLK_SECTOR > d->sectors)
        return NX_VBLK_E_RANGE;
    return request(d, T_IN, sector, bytes);
}

int nx_vblk_write(struct nx_vblk *d, uint64_t sector, uint32_t bytes)
{
    if (bytes == 0 || bytes % NX_VBLK_SECTOR || sector + bytes / NX_VBLK_SECTOR > d->sectors)
        return NX_VBLK_E_RANGE;
    return request(d, T_OUT, sector, bytes);
}

int nx_vblk_flush(struct nx_vblk *d)
{
    if (!d->has_flush)
        return NX_VBLK_E_UNSUPP;
    return request(d, T_FLUSH, 0, 0);
}

/* Negotiates features, sets up queue 0 and the request pages.  Returns
 * NULL on success or a reason. */
static const char *init_device(struct nx_vblk *d)
{
    const char *err = nx_virtio_start(&d->v, F_BLK_RO | F_BLK_BLK_SIZE | F_BLK_FLUSH);
    if (err)
        return err;
    if (!d->v.devcfg)
        return "device configuration capability missing";
    d->read_only = (d->v.accepted & F_BLK_RO) != 0;
    d->has_flush = (d->v.accepted & F_BLK_FLUSH) != 0;
    err = nx_virtio_queue(&d->v, &d->q, 0, NX_VIRTQ_MAX);
    if (err)
        return err;
    if (d->q.size < NX_VBLK_MAX_PAGES + 2u)
        return "queue too small";
    d->req_pa = nx_page_alloc();
    memset(nx_phys_to_virt(d->req_pa), 0, 4096);
    for (uint32_t i = 0; i < NX_VBLK_MAX_PAGES; i++)
        d->data_pa[i] = nx_page_alloc();
    nx_virtio_ready(&d->v);
    /* Capacity (le64), read until the configuration generation is stable. */
    uint8_t gen;
    do {
        gen = nx_virtio_cfg_gen(&d->v);
        d->sectors = *(volatile uint32_t *)d->v.devcfg |
                     (uint64_t)*(volatile uint32_t *)(d->v.devcfg + 4) << 32;
    } while (gen != nx_virtio_cfg_gen(&d->v));
    /* Serial number (GET_ID): 20 bytes, NUL-padded. */
    int st = request(d, T_GET_ID, 0, NX_VBLK_SERIAL_MAX);
    const char *id = nx_vblk_page(d, 0);
    for (uint32_t i = 0; i < NX_VBLK_SERIAL_MAX; i++)
        d->serial[i] = st == NX_VBLK_OK && id[i] >= 0x21 && id[i] < 0x7F ? id[i] : 0;
    return 0;
}

static int probe_one(void *ctx, struct nx_pci_addr a, uint16_t device)
{
    (void)ctx;
    if (device != VIRTIO_DEV_BLK_LEGACY && device != VIRTIO_DEV_BLK_MODERN)
        return 0;
    if (nx_vblk_count == NX_VBLK_MAX) {
        nx_printf("NANOX: blk %02x:%02x.%u ignored: at most %u devices\n", a.bus, a.dev, a.fn,
                  NX_VBLK_MAX);
        return 0;
    }
    struct nx_vblk *d = &nx_vblk_dev[nx_vblk_count];
    memset(d, 0, sizeof(*d));
    d->v.pci = a;
    d->v.device_id = device;
    const char *err = init_device(d);
    if (err) {
        nx_printf("NANOX: blk %02x:%02x.%u virtio-blk init failed: %s\n", a.bus, a.dev, a.fn,
                  err);
        return 0;
    }
    d->present = 1;
    nx_vblk_count++;
    nx_printf("NANOX: blk %02x:%02x.%u virtio-blk id=0x%x serial=\"%s\" sectors=%" NX_PRIu64
              " features=0x%" NX_PRIx64 " ro=%s flush=%s queue=%u\n",
              a.bus, a.dev, a.fn, device, d->serial, d->sectors, d->v.features,
              d->read_only ? "yes" : "no", d->has_flush ? "yes" : "no", d->q.size);
    return 0;
}

uint32_t nx_vblk_probe(void)
{
    if (!probed) {
        probed = 1;
        nx_pci_scan(NX_VIRTIO_VENDOR, probe_one, 0);
    }
    return nx_vblk_count;
}

struct nx_vblk *nx_vblk_by_serial(const char *serial)
{
    for (uint32_t i = 0; i < nx_vblk_count; i++) {
        const char *a = nx_vblk_dev[i].serial, *b = serial;
        while (*a && *a == *b) {
            a++;
            b++;
        }
        if (*a == *b)
            return &nx_vblk_dev[i];
    }
    return 0;
}
