/*
 * virtio-blk over the virtio 1.x PCI transport, polled.  See virtio_blk.h.
 */
#include <nanox/printf.h>
#include <nanox/string.h>

#include "kernel.h"
#include "mm/mm.h"
#include "virtio_blk.h"

#define VIRTIO_VENDOR 0x1AF4u
#define VIRTIO_DEV_BLK_LEGACY 0x1001u /* transitional */
#define VIRTIO_DEV_BLK_MODERN 0x1042u

/* virtio_pci_cap cfg_type */
#define CAP_COMMON 1u
#define CAP_NOTIFY 2u
#define CAP_DEVICE 4u

/* virtio_pci_common_cfg offsets */
#define CC_DFSELECT 0x00u
#define CC_DF 0x04u
#define CC_GFSELECT 0x08u
#define CC_GF 0x0Cu
#define CC_STATUS 0x14u
#define CC_QSELECT 0x16u
#define CC_QSIZE 0x18u
#define CC_QMSIX 0x1Au
#define CC_QENABLE 0x1Cu
#define CC_QNOTIFYOFF 0x1Eu
#define CC_QDESC 0x20u
#define CC_QDRIVER 0x28u
#define CC_QDEVICE 0x30u
#define CC_CFGGEN 0x15u

#define ST_ACK 1u
#define ST_DRIVER 2u
#define ST_DRIVER_OK 4u
#define ST_FEATURES_OK 8u
#define ST_FAILED 0x80u

#define F_BLK_RO (1ull << 5)
#define F_BLK_BLK_SIZE (1ull << 6)
#define F_BLK_FLUSH (1ull << 9)
#define F_VERSION_1 (1ull << 32)

#define T_IN 0u
#define T_OUT 1u
#define T_FLUSH 4u
#define T_GET_ID 8u

#define DESC_NEXT 1u
#define DESC_WRITE 2u

#define QSIZE_MAX 16u
#define RING_AVAIL_OFF 512u
#define RING_USED_OFF 1024u
#define POLL_LIMIT 400000000ull

struct vq_desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags, next;
};

struct vq_avail {
    uint16_t flags, idx;
    uint16_t ring[QSIZE_MAX];
};

struct vq_used_elem {
    uint32_t id, len;
};

struct vq_used {
    uint16_t flags, idx;
    struct vq_used_elem ring[QSIZE_MAX];
};

struct blk_req {
    uint32_t type, reserved;
    uint64_t sector;
    uint8_t status;
};

struct nx_vblk nx_vblk_dev[NX_VBLK_MAX];
uint32_t nx_vblk_count;
static int probed;

static inline void mb(void)
{
    __asm__ volatile("mfence" ::: "memory");
}

static inline uint8_t r8(volatile uint8_t *b, uint32_t o)
{
    return *(volatile uint8_t *)(b + o);
}
static inline uint16_t r16(volatile uint8_t *b, uint32_t o)
{
    return *(volatile uint16_t *)(b + o);
}
static inline uint32_t r32(volatile uint8_t *b, uint32_t o)
{
    return *(volatile uint32_t *)(b + o);
}
static inline void w8(volatile uint8_t *b, uint32_t o, uint8_t v)
{
    *(volatile uint8_t *)(b + o) = v;
}
static inline void w16(volatile uint8_t *b, uint32_t o, uint16_t v)
{
    *(volatile uint16_t *)(b + o) = v;
}
static inline void w32(volatile uint8_t *b, uint32_t o, uint32_t v)
{
    *(volatile uint32_t *)(b + o) = v;
}
static void w64(volatile uint8_t *b, uint32_t o, uint64_t v)
{
    w32(b, o, (uint32_t)v);
    w32(b, o + 4, (uint32_t)(v >> 32));
}

/* Maps [phys, phys + len) uncached; returns the virtual address of phys. */
static volatile uint8_t *map_region(uint64_t phys, uint32_t len)
{
    for (uint64_t p = phys & ~0xFFFull; p < phys + len; p += 4096)
        nx_vmm_map_mmio(p);
    return (volatile uint8_t *)(uintptr_t)(NX_PHYSMAP_BASE + phys);
}

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
    uint8_t *ring = nx_phys_to_virt(d->ring_pa);
    struct vq_desc *desc = (struct vq_desc *)ring;
    volatile struct vq_avail *avail = (volatile struct vq_avail *)(ring + RING_AVAIL_OFF);
    volatile struct vq_used *used = (volatile struct vq_used *)(ring + RING_USED_OFF);
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
    desc[n].flags = DESC_NEXT;
    desc[n].next = (uint16_t)(n + 1);
    n++;
    for (uint32_t i = 0; i < pages; i++) {
        uint32_t len = bytes - i * 4096u < 4096u ? bytes - i * 4096u : 4096u;
        desc[n].addr = d->data_pa[i];
        desc[n].len = len;
        desc[n].flags = (uint16_t)(DESC_NEXT | (type == T_OUT ? 0u : DESC_WRITE));
        desc[n].next = (uint16_t)(n + 1);
        n++;
    }
    desc[n].addr = d->req_pa + 16;
    desc[n].len = 1;
    desc[n].flags = DESC_WRITE;
    desc[n].next = 0;
    avail->ring[d->avail_idx % d->qsize] = 0;
    mb();
    d->avail_idx++;
    avail->idx = d->avail_idx;
    mb();
    w16(d->notify_base, (uint32_t)d->notify_off * d->notify_mult, 0);
    for (uint64_t spins = 0; used->idx == d->used_seen; spins++) {
        if (spins >= POLL_LIMIT) {
            d->errors++;
            return NX_VBLK_E_TIMEOUT;
        }
        __asm__ volatile("pause");
    }
    mb();
    d->used_seen++;
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

/* Reads the capabilities, maps the regions, negotiates features and sets
 * up queue 0.  Returns NULL on success or a reason. */
static const char *init_device(struct nx_vblk *d)
{
    struct nx_pci_addr a = d->pci;
    if (!(nx_pci_read16(a, NX_PCI_STATUS) & NX_PCI_STATUS_CAPS))
        return "no capability list";
    uint32_t have = 0;
    for (uint8_t p = nx_pci_read8(a, NX_PCI_CAP_PTR) & 0xFCu, guard = 0; p && guard < 48;
         p = nx_pci_read8(a, p + 1u) & 0xFCu, guard++) {
        if (nx_pci_read8(a, p) != 0x09u)
            continue; /* not vendor specific */
        uint32_t type = nx_pci_read8(a, p + 3u), bar = nx_pci_read8(a, p + 4u);
        uint32_t off = nx_pci_read32(a, p + 8u), len = nx_pci_read32(a, p + 12u);
        if ((type != CAP_COMMON && type != CAP_NOTIFY && type != CAP_DEVICE) ||
            (have & (1u << type)))
            continue;
        uint64_t base = nx_pci_bar(a, bar);
        if (!base)
            return "capability in an unassigned or I/O BAR";
        volatile uint8_t *va = map_region(base + off, len);
        if (type == CAP_COMMON)
            d->common = va;
        else if (type == CAP_NOTIFY) {
            d->notify_base = va;
            d->notify_mult = nx_pci_read32(a, p + 16u);
        } else
            d->devcfg = va;
        have |= 1u << type;
    }
    if (!d->common || !d->notify_base || !d->devcfg)
        return "virtio 1.x capabilities missing";
    nx_pci_write16(a, NX_PCI_COMMAND,
                   (uint16_t)(nx_pci_read16(a, NX_PCI_COMMAND) | NX_PCI_CMD_MEM |
                              NX_PCI_CMD_MASTER));
    volatile uint8_t *cc = d->common;
    w8(cc, CC_STATUS, 0);
    for (unsigned i = 0; r8(cc, CC_STATUS) != 0; i++)
        if (i > 1000000u)
            return "reset did not complete";
    w8(cc, CC_STATUS, ST_ACK);
    w8(cc, CC_STATUS, ST_ACK | ST_DRIVER);
    w32(cc, CC_DFSELECT, 0);
    uint64_t f = r32(cc, CC_DF);
    w32(cc, CC_DFSELECT, 1);
    f |= (uint64_t)r32(cc, CC_DF) << 32;
    d->features = f;
    if (!(f & F_VERSION_1)) {
        w8(cc, CC_STATUS, ST_FAILED);
        return "VIRTIO_F_VERSION_1 not offered";
    }
    uint64_t want = F_VERSION_1 | (f & (F_BLK_RO | F_BLK_BLK_SIZE | F_BLK_FLUSH));
    w32(cc, CC_GFSELECT, 0);
    w32(cc, CC_GF, (uint32_t)want);
    w32(cc, CC_GFSELECT, 1);
    w32(cc, CC_GF, (uint32_t)(want >> 32));
    w8(cc, CC_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK);
    if (!(r8(cc, CC_STATUS) & ST_FEATURES_OK))
        return "FEATURES_OK not accepted";
    d->read_only = (want & F_BLK_RO) != 0;
    d->has_flush = (want & F_BLK_FLUSH) != 0;
    w16(cc, CC_QSELECT, 0);
    uint16_t qs = r16(cc, CC_QSIZE);
    if (qs == 0)
        return "queue 0 missing";
    d->qsize = qs < QSIZE_MAX ? qs : QSIZE_MAX;
    if (d->qsize < NX_VBLK_MAX_PAGES + 2u)
        return "queue too small";
    w16(cc, CC_QSIZE, d->qsize);
    d->ring_pa = nx_page_alloc();
    d->req_pa = nx_page_alloc();
    memset(nx_phys_to_virt(d->ring_pa), 0, 4096);
    memset(nx_phys_to_virt(d->req_pa), 0, 4096);
    for (uint32_t i = 0; i < NX_VBLK_MAX_PAGES; i++)
        d->data_pa[i] = nx_page_alloc();
    w64(cc, CC_QDESC, d->ring_pa);
    w64(cc, CC_QDRIVER, d->ring_pa + RING_AVAIL_OFF);
    w64(cc, CC_QDEVICE, d->ring_pa + RING_USED_OFF);
    w16(cc, CC_QMSIX, 0xFFFFu);
    d->notify_off = r16(cc, CC_QNOTIFYOFF);
    w16(cc, CC_QENABLE, 1);
    w8(cc, CC_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
    /* Capacity (le64), read until the configuration generation is stable. */
    uint8_t gen;
    do {
        gen = r8(cc, CC_CFGGEN);
        d->sectors = r32(d->devcfg, 0) | (uint64_t)r32(d->devcfg, 4) << 32;
    } while (gen != r8(cc, CC_CFGGEN));
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
    d->pci = a;
    d->device_id = device;
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
              a.bus, a.dev, a.fn, device, d->serial, d->sectors, d->features,
              d->read_only ? "yes" : "no", d->has_flush ? "yes" : "no", d->qsize);
    return 0;
}

uint32_t nx_vblk_probe(void)
{
    if (!probed) {
        probed = 1;
        nx_pci_scan(VIRTIO_VENDOR, probe_one, 0);
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
