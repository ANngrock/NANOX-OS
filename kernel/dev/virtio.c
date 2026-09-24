/*
 * virtio 1.x PCI transport; see virtio.h.
 */
#include <nanox/string.h>

#include "virtio.h"

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
#define CC_CFGGEN 0x15u
#define CC_QSELECT 0x16u
#define CC_QSIZE 0x18u
#define CC_QMSIX 0x1Au
#define CC_QENABLE 0x1Cu
#define CC_QNOTIFYOFF 0x1Eu
#define CC_QDESC 0x20u
#define CC_QDRIVER 0x28u
#define CC_QDEVICE 0x30u

#define ST_ACK 1u
#define ST_DRIVER 2u
#define ST_DRIVER_OK 4u
#define ST_FEATURES_OK 8u
#define ST_FAILED 0x80u

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

const char *nx_virtio_start(struct nx_virtio *v, uint64_t wanted)
{
    struct nx_pci_addr a = v->pci;
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
            v->common = va;
        else if (type == CAP_NOTIFY) {
            v->notify_base = va;
            v->notify_mult = nx_pci_read32(a, p + 16u);
        } else
            v->devcfg = va;
        have |= 1u << type;
    }
    if (!v->common || !v->notify_base)
        return "virtio 1.x capabilities missing";
    nx_pci_write16(a, NX_PCI_COMMAND,
                   (uint16_t)(nx_pci_read16(a, NX_PCI_COMMAND) | NX_PCI_CMD_MEM |
                              NX_PCI_CMD_MASTER));
    volatile uint8_t *cc = v->common;
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
    v->features = f;
    if (!(f & NX_VIRTIO_F_VERSION_1)) {
        w8(cc, CC_STATUS, ST_FAILED);
        return "VIRTIO_F_VERSION_1 not offered";
    }
    uint64_t want = NX_VIRTIO_F_VERSION_1 | (f & wanted);
    w32(cc, CC_GFSELECT, 0);
    w32(cc, CC_GF, (uint32_t)want);
    w32(cc, CC_GFSELECT, 1);
    w32(cc, CC_GF, (uint32_t)(want >> 32));
    w8(cc, CC_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK);
    if (!(r8(cc, CC_STATUS) & ST_FEATURES_OK))
        return "FEATURES_OK not accepted";
    v->accepted = want;
    return 0;
}

const char *nx_virtio_queue(struct nx_virtio *v, struct nx_virtq *q, uint16_t index,
                            uint16_t max)
{
    volatile uint8_t *cc = v->common;
    w16(cc, CC_QSELECT, index);
    uint16_t qs = r16(cc, CC_QSIZE);
    if (qs == 0)
        return "queue missing";
    if (max > NX_VIRTQ_MAX)
        max = NX_VIRTQ_MAX;
    q->index = index;
    q->size = qs < max ? qs : max;
    w16(cc, CC_QSIZE, q->size);
    q->ring_pa = nx_page_alloc();
    memset(nx_phys_to_virt(q->ring_pa), 0, 4096);
    q->avail_idx = 0;
    q->used_seen = 0;
    w64(cc, CC_QDESC, q->ring_pa);
    w64(cc, CC_QDRIVER, q->ring_pa + NX_VIRTQ_AVAIL_OFF);
    w64(cc, CC_QDEVICE, q->ring_pa + NX_VIRTQ_USED_OFF);
    w16(cc, CC_QMSIX, 0xFFFFu);
    q->notify_off = r16(cc, CC_QNOTIFYOFF);
    w16(cc, CC_QENABLE, 1);
    return 0;
}

void nx_virtio_ready(struct nx_virtio *v)
{
    w8(v->common, CC_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
}

uint8_t nx_virtio_cfg_gen(struct nx_virtio *v)
{
    return r8(v->common, CC_CFGGEN);
}

void nx_virtq_submit(struct nx_virtio *v, struct nx_virtq *q, uint16_t head)
{
    uint8_t *ring = nx_phys_to_virt(q->ring_pa);
    volatile struct nx_virtq_avail *avail = (volatile struct nx_virtq_avail *)(ring +
                                                                           NX_VIRTQ_AVAIL_OFF);
    avail->ring[q->avail_idx % q->size] = head;
    nx_virtio_mb();
    q->avail_idx++;
    avail->idx = q->avail_idx;
    nx_virtio_mb();
    w16(v->notify_base, (uint32_t)q->notify_off * v->notify_mult, q->index);
}

int nx_virtq_poll(struct nx_virtq *q, struct nx_virtq_used_elem *out)
{
    uint8_t *ring = nx_phys_to_virt(q->ring_pa);
    volatile struct nx_virtq_used *used = (volatile struct nx_virtq_used *)(ring +
                                                                        NX_VIRTQ_USED_OFF);
    if (used->idx == q->used_seen)
        return 0;
    nx_virtio_mb();
    out->id = used->ring[q->used_seen % q->size].id;
    out->len = used->ring[q->used_seen % q->size].len;
    q->used_seen++;
    return 1;
}
