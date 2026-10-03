//! virtio-blk and virtio-net through the platform bus, driven the way a guest
//! driver does it: PCI configuration cycles to find and enable the function,
//! BAR 0 memory accesses for the handshake and the queues, descriptors written
//! into guest RAM, a notification, and the device's answer in the used ring.

use std::collections::VecDeque;

use vmm_devices::ioapic;
use vmm_devices::lapic::{self, reg};
use vmm_devices::machine::*;
use vmm_devices::virtio::*;
use vmm_devices::virtio_blk::{self, BlockBackend, SECTOR};
use vmm_devices::virtio_net::{self, NetBackend, MAX_FRAME};

const T0: i64 = 1_790_944_496;
const BLK_BAR: u64 = 0xC000_0000;
const NET_BAR: u64 = 0xC000_4000;

struct Ram(Vec<u8>);

impl GuestMemory for Ram {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        let Some(end) = gpa.checked_add(buf.len() as u64) else {
            return false;
        };
        match self.0.get(gpa as usize..end as usize) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        let Some(end) = gpa.checked_add(data.len() as u64) else {
            return false;
        };
        match self.0.get_mut(gpa as usize..end as usize) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

struct Disk {
    data: Vec<u8>,
    flushes: u32,
    /// Reads and writes of this sector fail.
    bad: Option<u64>,
    writes: Vec<u64>,
}

impl Disk {
    fn new(sectors: usize) -> Self {
        let mut data = vec![0u8; sectors * SECTOR];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i / SECTOR) as u8 ^ (i % 251) as u8;
        }
        Self {
            data,
            flushes: 0,
            bad: None,
            writes: Vec::new(),
        }
    }
}

impl BlockBackend for Disk {
    fn sectors(&self) -> u64 {
        (self.data.len() / SECTOR) as u64
    }
    fn read(&mut self, s: u64, buf: &mut [u8; SECTOR]) -> bool {
        if self.bad == Some(s) {
            return false;
        }
        buf.copy_from_slice(&self.data[s as usize * SECTOR..][..SECTOR]);
        true
    }
    fn write(&mut self, s: u64, d: &[u8; SECTOR]) -> bool {
        if self.bad == Some(s) {
            return false;
        }
        self.data[s as usize * SECTOR..][..SECTOR].copy_from_slice(d);
        self.writes.push(s);
        true
    }
    fn flush(&mut self) -> bool {
        self.flushes += 1;
        true
    }
}

#[derive(Default)]
struct Wire {
    sent: Vec<Vec<u8>>,
    inbox: VecDeque<Vec<u8>>,
}

impl NetBackend for Wire {
    fn send(&mut self, frame: &[u8]) {
        self.sent.push(frame.to_vec());
    }
    fn recv(&mut self, buf: &mut [u8; MAX_FRAME]) -> Option<usize> {
        let f = self.inbox.pop_front()?;
        buf[..f.len()].copy_from_slice(&f);
        Some(f.len())
    }
}

fn cfg_addr(dev: u8, off: u32) -> u32 {
    (1 << 31) | (u32::from(dev) << 11) | (off & !3)
}

fn cfg_read(m: &mut Machine, dev: u8, off: u32, size: u8) -> u32 {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(dev, off), 0);
    m.io_in(PCI_DATA + (off & 3) as u16, size, 0)
}

fn cfg_write(m: &mut Machine, dev: u8, off: u32, size: u8, v: u32) {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(dev, off), 0);
    m.io_out(PCI_DATA + (off & 3) as u16, size, v, 0);
}

fn machine() -> Machine {
    let mut m = Machine::new(T0, 100_000_000);
    m.blk.set_capacity(64);
    m
}

/// A virtqueue in guest RAM with the layout a driver would allocate.
struct Q {
    n: u16,
    desc: u64,
    avail: u64,
    used: u64,
    avail_idx: u16,
    next_desc: u16,
    seen_used: u16,
}

impl Q {
    fn new(base: u64, n: u16) -> Self {
        Self {
            n,
            desc: base,
            avail: base + 0x1000,
            used: base + 0x2000,
            avail_idx: 0,
            next_desc: 0,
            seen_used: 0,
        }
    }

    /// Writes a chain of (addr, len, device-writes) descriptors and makes it available; returns the head.
    fn add(&mut self, ram: &mut Ram, bufs: &[(u64, u32, bool)]) -> u16 {
        let head = self.next_desc;
        for (i, &(addr, len, write)) in bufs.iter().enumerate() {
            let idx = self.next_desc;
            self.next_desc = (self.next_desc + 1) % self.n;
            let last = i + 1 == bufs.len();
            let flags: u16 = (if write { 2 } else { 0 }) | (if last { 0 } else { 1 });
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&addr.to_le_bytes());
            d[8..12].copy_from_slice(&len.to_le_bytes());
            d[12..14].copy_from_slice(&flags.to_le_bytes());
            d[14..16].copy_from_slice(&self.next_desc.to_le_bytes());
            assert!(ram.write(self.desc + 16 * u64::from(idx), &d));
        }
        let slot = self.avail + 4 + 2 * u64::from(self.avail_idx % self.n);
        assert!(ram.write(slot, &head.to_le_bytes()));
        self.avail_idx = self.avail_idx.wrapping_add(1);
        assert!(ram.write(self.avail + 2, &self.avail_idx.to_le_bytes()));
        head
    }

    /// The (head, written) entries the device returned since the last call.
    fn take_used(&mut self, ram: &Ram) -> Vec<(u32, u32)> {
        let mut idx = [0u8; 2];
        assert!(ram.read(self.used + 2, &mut idx));
        let idx = u16::from_le_bytes(idx);
        let mut out = Vec::new();
        while self.seen_used != idx {
            let mut e = [0u8; 8];
            assert!(ram.read(
                self.used + 4 + 8 * u64::from(self.seen_used % self.n),
                &mut e
            ));
            out.push((
                u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            ));
            self.seen_used = self.seen_used.wrapping_add(1);
        }
        out
    }
}

fn w(m: &mut Machine, bar: u64, off: u64, size: u8, v: u32) {
    m.mmio_write(bar + off, size, u64::from(v), 0);
}

fn r(m: &mut Machine, bar: u64, off: u64, size: u8) -> u32 {
    m.mmio_read(bar + off, size, 0) as u32
}

/// Places BAR 0, enables memory and bus mastering, and runs the handshake up to DRIVER_OK.
fn bring_up(m: &mut Machine, dev: u8, bar: u64, queues: &[&Q], accept: u64) -> u8 {
    cfg_write(m, dev, 0x10, 4, bar as u32);
    cfg_write(m, dev, 0x04, 2, 0x0006);
    w(m, bar, 0x14, 1, 0);
    w(m, bar, 0x14, 1, 1);
    w(m, bar, 0x14, 1, 3);
    w(m, bar, 0x08, 4, 0);
    w(m, bar, 0x0C, 4, accept as u32);
    w(m, bar, 0x08, 4, 1);
    w(m, bar, 0x0C, 4, (accept >> 32) as u32);
    w(m, bar, 0x14, 1, 0xB);
    for (i, q) in queues.iter().enumerate() {
        w(m, bar, 0x16, 2, i as u32);
        w(m, bar, 0x18, 2, u32::from(q.n));
        w(m, bar, 0x20, 4, q.desc as u32);
        w(m, bar, 0x24, 4, (q.desc >> 32) as u32);
        w(m, bar, 0x28, 4, q.avail as u32);
        w(m, bar, 0x2C, 4, (q.avail >> 32) as u32);
        w(m, bar, 0x30, 4, q.used as u32);
        w(m, bar, 0x34, 4, (q.used >> 32) as u32);
        w(m, bar, 0x1C, 2, 1);
    }
    let s = r(m, bar, 0x14, 1) as u8;
    if s & 8 != 0 {
        w(m, bar, 0x14, 1, u32::from(s | 4));
    }
    r(m, bar, 0x14, 1) as u8
}

fn blk_request(ram: &mut Ram, at: u64, kind: u32, sector: u64) {
    let mut h = [0u8; 16];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    h[8..].copy_from_slice(&sector.to_le_bytes());
    assert!(ram.write(at, &h));
}

const HDR: u64 = 0x8000;
const DATA: u64 = 0x9000;
const STATUS: u64 = 0x7000;
const BLK_FEATURES: u64 = F_VERSION_1 | virtio_blk::F_FLUSH;

struct Rig {
    m: Machine,
    ram: Ram,
    disk: Disk,
    q: Q,
}

fn blk_rig() -> Rig {
    blk_rig_with(8)
}

fn blk_rig_with(queue_size: u16) -> Rig {
    let mut m = machine();
    let q = Q::new(0x1000, queue_size);
    let s = bring_up(&mut m, 3, BLK_BAR, &[&q], BLK_FEATURES);
    assert_eq!(s, 0xF);
    let mut ram = Ram(vec![0; 0x40000]);
    // a status byte the device has not written yet
    assert!(ram.write(STATUS, &[0xEE]));
    Rig {
        m,
        ram,
        disk: Disk::new(64),
        q,
    }
}

impl Rig {
    fn kick_blk(&mut self) -> u32 {
        w(&mut self.m, BLK_BAR, 0x3000, 4, 0);
        self.m.service_blk(&mut self.ram, &mut self.disk, 0)
    }

    fn status(&self) -> u8 {
        let mut b = [0u8; 1];
        assert!(self.ram.read(STATUS, &mut b));
        b[0]
    }
}

// ------------------------------------------------------------ identification

#[test]
fn the_functions_identify_as_modern_virtio_with_the_four_capabilities() {
    let mut m = machine();
    for (dev, id, class, sub, queues) in [
        (3u8, 0x1042u32, 0x01_8000u32, 2u32, 1u32),
        (4, 0x1041, 0x02_0000, 1, 2),
    ] {
        assert_eq!(cfg_read(&mut m, dev, 0, 4), (id << 16) | 0x1AF4);
        assert_eq!(cfg_read(&mut m, dev, 0x08, 4) >> 8, class, "class code");
        assert_eq!(cfg_read(&mut m, dev, 0x08, 1), 1, "revision 1: modern only");
        assert_eq!(cfg_read(&mut m, dev, 0x2C, 4), (sub << 16) | 0x1AF4);
        assert_eq!(cfg_read(&mut m, dev, 0x3D, 1), 1, "INTA");
        assert_ne!(cfg_read(&mut m, dev, 0x06, 2) & 0x10, 0, "capability list");
        let mut p = cfg_read(&mut m, dev, 0x34, 1);
        let mut seen = Vec::new();
        while p != 0 {
            assert_eq!(cfg_read(&mut m, dev, p, 1), 9, "vendor capability");
            let kind = cfg_read(&mut m, dev, p + 3, 1);
            let bar = cfg_read(&mut m, dev, p + 4, 1);
            let off = cfg_read(&mut m, dev, p + 8, 4);
            let len = cfg_read(&mut m, dev, p + 12, 4);
            let cap_len = cfg_read(&mut m, dev, p + 2, 1);
            let mult = (cap_len == 20).then(|| cfg_read(&mut m, dev, p + 16, 4));
            seen.push((kind, bar, off, len, mult));
            p = cfg_read(&mut m, dev, p + 1, 1);
        }
        assert_eq!(
            seen,
            [
                (1, 0, 0x0000, 0x38, None),
                (2, 0, 0x3000, 4 * queues, Some(4)),
                (3, 0, 0x1000, 4, None),
                (4, 0, 0x2000, 64, None),
            ]
        );
    }
}

#[test]
fn bar_zero_is_sixteen_kib_of_32_bit_memory_and_sizes_the_standard_way() {
    let mut m = machine();
    for dev in [3, 4] {
        assert_eq!(cfg_read(&mut m, dev, 0x10, 4), 0, "unplaced");
        cfg_write(&mut m, dev, 0x10, 4, 0xFFFF_FFFF);
        assert_eq!(cfg_read(&mut m, dev, 0x10, 4), 0xFFFF_C000);
        assert_eq!(cfg_read(&mut m, dev, 0x14, 4), 0, "BAR 1 is unused");
    }
}

#[test]
fn the_bar_decodes_only_when_memory_space_is_enabled() {
    let mut m = machine();
    cfg_write(&mut m, 3, 0x10, 4, BLK_BAR as u32);
    assert_eq!(
        r(&mut m, BLK_BAR, 0x12, 2),
        0xFFFF,
        "not enabled: nobody answers"
    );
    assert_eq!(m.unclaimed_mmio, 1);
    cfg_write(&mut m, 3, 0x04, 2, 0x0002);
    assert_eq!(r(&mut m, BLK_BAR, 0x12, 2), 1, "num_queues of the disk");
    assert_eq!(
        r(&mut m, BLK_BAR + 0x4000 - 4, 0, 4),
        0,
        "inside the BAR, past the structures"
    );
    assert_eq!(m.unclaimed_mmio, 1);
    assert_eq!(
        r(&mut m, BLK_BAR + 0x4000, 0, 4),
        0xFFFF_FFFF,
        "one byte past the BAR"
    );
    assert_eq!(m.unclaimed_mmio, 2);
    cfg_write(&mut m, 4, 0x10, 4, NET_BAR as u32);
    cfg_write(&mut m, 4, 0x04, 2, 0x0002);
    assert_eq!(
        r(&mut m, NET_BAR, 0x12, 2),
        2,
        "the network card has its own window"
    );
    assert_eq!(r(&mut m, BLK_BAR, 0x12, 2), 1);
    assert_eq!(
        m.mmio_read(BLK_BAR + 0x12, 8, 0),
        0xFFFF_FFFF_FFFF_FFFF,
        "only 1, 2 and 4 byte accesses"
    );
}

// --------------------------------------------------------------- handshake

#[test]
fn features_are_offered_in_two_halves_and_negotiated() {
    let mut m = machine();
    cfg_write(&mut m, 3, 0x10, 4, BLK_BAR as u32);
    cfg_write(&mut m, 3, 0x04, 2, 2);
    w(&mut m, BLK_BAR, 0x00, 4, 0);
    assert_eq!(r(&mut m, BLK_BAR, 0x04, 4), 0x200, "FLUSH");
    w(&mut m, BLK_BAR, 0x00, 4, 1);
    assert_eq!(r(&mut m, BLK_BAR, 0x04, 4), 1, "VERSION_1");
    w(&mut m, BLK_BAR, 0x00, 4, 2);
    assert_eq!(r(&mut m, BLK_BAR, 0x04, 4), 0, "nothing in higher words");
    assert_eq!(
        r(&mut m, BLK_BAR, 0x00, 4),
        2,
        "the select register reads back"
    );

    cfg_write(&mut m, 4, 0x10, 4, NET_BAR as u32);
    cfg_write(&mut m, 4, 0x04, 2, 2);
    w(&mut m, NET_BAR, 0x00, 4, 0);
    assert_eq!(
        r(&mut m, NET_BAR, 0x04, 4),
        (1 << 5) | (1 << 16),
        "MAC and STATUS"
    );
    w(&mut m, NET_BAR, 0x00, 4, 1);
    assert_eq!(r(&mut m, NET_BAR, 0x04, 4), 1);
}

#[test]
fn features_ok_is_refused_without_version_1_or_with_an_unoffered_feature() {
    for (accept, ok) in [
        (BLK_FEATURES, true),
        (F_VERSION_1, true),
        (virtio_blk::F_FLUSH, false),
        (BLK_FEATURES | 1, false),
        (BLK_FEATURES | (1 << 33), false),
        (0, false),
    ] {
        let mut m = machine();
        let q = Q::new(0x1000, 8);
        let s = bring_up(&mut m, 3, BLK_BAR, &[&q], accept);
        assert_eq!(s & 8 != 0, ok, "accepting {accept:#x}");
        assert_eq!(s & 4 != 0, ok, "DRIVER_OK only after FEATURES_OK stuck");
    }
}

#[test]
fn the_driver_features_register_reads_back_what_was_selected() {
    let mut m = machine();
    cfg_write(&mut m, 3, 0x10, 4, BLK_BAR as u32);
    cfg_write(&mut m, 3, 0x04, 2, 2);
    w(&mut m, BLK_BAR, 0x08, 4, 0);
    w(&mut m, BLK_BAR, 0x0C, 4, 0x200);
    w(&mut m, BLK_BAR, 0x08, 4, 1);
    w(&mut m, BLK_BAR, 0x0C, 4, 1);
    assert_eq!(r(&mut m, BLK_BAR, 0x0C, 4), 1);
    w(&mut m, BLK_BAR, 0x08, 4, 0);
    assert_eq!(
        r(&mut m, BLK_BAR, 0x0C, 4),
        0x200,
        "the low half survived the high write"
    );
    w(&mut m, BLK_BAR, 0x08, 4, 2);
    w(&mut m, BLK_BAR, 0x0C, 4, 0xFFFF_FFFF);
    assert_eq!(r(&mut m, BLK_BAR, 0x0C, 4), 0, "no word 2");
    w(&mut m, BLK_BAR, 0x08, 4, 0);
    assert_eq!(
        r(&mut m, BLK_BAR, 0x0C, 4),
        0x200,
        "a write to word 2 changed nothing"
    );
}

#[test]
fn writing_zero_to_status_resets_the_device() {
    let mut r_ = blk_rig();
    assert!(r_.m.blk.t.driver_ok());
    let h =
        r_.q.add(&mut r_.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    blk_request(&mut r_.ram, HDR, 4, 0);
    assert_eq!(h, 0);
    w(&mut r_.m, BLK_BAR, 0x14, 1, 0);
    assert_eq!(r(&mut r_.m, BLK_BAR, 0x14, 1), 0);
    assert_eq!(r_.m.blk.t.resets, 2, "the bring-up reset and this one");
    w(&mut r_.m, BLK_BAR, 0x16, 2, 0);
    assert_eq!(
        r(&mut r_.m, BLK_BAR, 0x1C, 2),
        0,
        "queues are disabled again"
    );
    assert_eq!(r(&mut r_.m, BLK_BAR, 0x20, 4), 0, "and forgotten");
    assert_eq!(r_.kick_blk(), 0, "nothing is served after a reset");
    assert_eq!(r_.disk.flushes, 0);
}

#[test]
fn queue_registers_are_selected_sized_and_locked_once_enabled() {
    let mut m = machine();
    cfg_write(&mut m, 4, 0x10, 4, NET_BAR as u32);
    cfg_write(&mut m, 4, 0x04, 2, 2);
    assert_eq!(r(&mut m, NET_BAR, 0x18, 2), 128, "the device's maximum");
    w(&mut m, NET_BAR, 0x16, 2, 1);
    assert_eq!(r(&mut m, NET_BAR, 0x1E, 2), 1, "queue 1 notifies at 0x3004");
    w(&mut m, NET_BAR, 0x18, 2, 3);
    assert_eq!(r(&mut m, NET_BAR, 0x18, 2), 128, "not a power of two");
    w(&mut m, NET_BAR, 0x18, 2, 256);
    assert_eq!(r(&mut m, NET_BAR, 0x18, 2), 128, "too large");
    w(&mut m, NET_BAR, 0x18, 2, 0);
    assert_eq!(r(&mut m, NET_BAR, 0x18, 2), 128, "zero");
    w(&mut m, NET_BAR, 0x18, 2, 16);
    assert_eq!(r(&mut m, NET_BAR, 0x18, 2), 16);
    w(&mut m, NET_BAR, 0x1C, 2, 1);
    assert_eq!(
        r(&mut m, NET_BAR, 0x1C, 2),
        0,
        "cannot be enabled without addresses"
    );
    w(&mut m, NET_BAR, 0x20, 4, 0x1000);
    w(&mut m, NET_BAR, 0x28, 4, 0x2000);
    w(&mut m, NET_BAR, 0x30, 4, 0x3000);
    w(&mut m, NET_BAR, 0x24, 4, 1);
    assert_eq!(r(&mut m, NET_BAR, 0x20, 4), 0x1000);
    assert_eq!(r(&mut m, NET_BAR, 0x24, 4), 1, "high half");
    w(&mut m, NET_BAR, 0x24, 4, 0);
    w(&mut m, NET_BAR, 0x1C, 2, 1);
    assert_eq!(r(&mut m, NET_BAR, 0x1C, 2), 1);
    w(&mut m, NET_BAR, 0x18, 2, 8);
    w(&mut m, NET_BAR, 0x20, 4, 0x5000);
    assert_eq!(
        r(&mut m, NET_BAR, 0x18, 2),
        16,
        "size is locked while enabled"
    );
    assert_eq!(r(&mut m, NET_BAR, 0x20, 4), 0x1000, "so are the addresses");
    w(&mut m, NET_BAR, 0x16, 2, 0);
    assert_eq!(
        r(&mut m, NET_BAR, 0x1C, 2),
        0,
        "queue 0 is a different queue"
    );
    w(&mut m, NET_BAR, 0x16, 2, 2);
    assert_eq!(r(&mut m, NET_BAR, 0x18, 2), 0, "no queue 2");
    assert_eq!(r(&mut m, NET_BAR, 0x1E, 2), 0);
}

#[test]
fn the_device_configuration_holds_capacity_and_mac() {
    let mut m = machine();
    for (dev, bar) in [(3u8, BLK_BAR), (4, NET_BAR)] {
        cfg_write(&mut m, dev, 0x10, 4, bar as u32);
        cfg_write(&mut m, dev, 0x04, 2, 2);
    }
    assert_eq!(r(&mut m, BLK_BAR, 0x2000, 4), 64);
    assert_eq!(r(&mut m, BLK_BAR, 0x2004, 4), 0);
    assert_eq!(r(&mut m, BLK_BAR, 0x2000, 1), 64);
    assert_eq!(r(&mut m, BLK_BAR, 0x2000, 2), 64);
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = r(&mut m, NET_BAR, 0x2000 + i as u64, 1) as u8;
    }
    assert_eq!(mac, NET_MAC);
    assert_eq!(r(&mut m, NET_BAR, 0x2006, 2), 1, "link up");
    assert_eq!(r(&mut m, NET_BAR, 0x2040, 4), 0, "past the structure");
    w(&mut m, BLK_BAR, 0x2000, 4, 0xFFFF);
    assert_eq!(r(&mut m, BLK_BAR, 0x2000, 4), 64, "read-only");
}

// -------------------------------------------------------------- virtio-blk

#[test]
fn a_read_request_fills_the_buffers_and_reports_ok() {
    let mut t = blk_rig();
    blk_request(&mut t.ram, HDR, 0, 5);
    let head = t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (DATA, 1024, true), (STATUS, 1, true)],
    );
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 0);
    let mut got = vec![0u8; 1024];
    assert!(t.ram.read(DATA, &mut got));
    assert_eq!(&got[..], &t.disk.data[5 * SECTOR..7 * SECTOR]);
    assert_eq!(t.q.take_used(&t.ram), [(u32::from(head), 1025)]);
}

#[test]
fn a_write_request_stores_sectors_and_a_scattered_buffer_works() {
    let mut t = blk_rig();
    let mut payload = vec![0u8; 3 * SECTOR];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i * 7 + 3) as u8;
    }
    // three separate buffers, not adjacent in guest memory
    for (i, a) in [0x9000u64, 0xB000, 0xA000].iter().enumerate() {
        assert!(t.ram.write(*a, &payload[i * SECTOR..(i + 1) * SECTOR]));
    }
    blk_request(&mut t.ram, HDR, 1, 10);
    let head = t.q.add(
        &mut t.ram,
        &[
            (HDR, 16, false),
            (0x9000, 512, false),
            (0xB000, 512, false),
            (0xA000, 512, false),
            (STATUS, 1, true),
        ],
    );
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 0);
    assert_eq!(&t.disk.data[10 * SECTOR..13 * SECTOR], &payload[..]);
    assert_eq!(t.disk.writes, [10, 11, 12]);
    assert_eq!(
        t.q.take_used(&t.ram),
        [(u32::from(head), 1)],
        "a write returns only the status byte"
    );
}

#[test]
fn flush_and_get_id_requests() {
    let mut t = blk_rig();
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 0);
    assert_eq!(t.disk.flushes, 1);
    t.ram.write(STATUS, &[0xEE]);
    blk_request(&mut t.ram, HDR, 8, 0);
    let head = t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (DATA, 20, true), (STATUS, 1, true)],
    );
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 0);
    let mut id = [0u8; 20];
    assert!(t.ram.read(DATA, &mut id));
    assert_eq!(&id, b"nanox-virtio-blk\0\0\0\0");
    let used = t.q.take_used(&t.ram);
    assert_eq!(used.last(), Some(&(u32::from(head), 21)));
    // a buffer too small for the id is an I/O error
    blk_request(&mut t.ram, HDR, 8, 0);
    t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (DATA, 19, true), (STATUS, 1, true)],
    );
    t.kick_blk();
    assert_eq!(t.status(), 1);
}

#[test]
fn bad_requests_get_an_error_status_and_never_touch_the_disk() {
    type Case = (&'static str, u32, u64, Vec<(u64, u32, bool)>, u8);
    let cases: [Case; 9] = [
        (
            "unknown type",
            7,
            0,
            vec![(HDR, 16, false), (DATA, 512, true), (STATUS, 1, true)],
            2,
        ),
        (
            "discard is not offered",
            11,
            0,
            vec![(HDR, 16, false), (STATUS, 1, true)],
            2,
        ),
        (
            "read into a read-only buffer",
            0,
            0,
            vec![(HDR, 16, false), (DATA, 512, false), (STATUS, 1, true)],
            1,
        ),
        (
            "write from a write-only buffer",
            1,
            0,
            vec![(HDR, 16, false), (DATA, 512, true), (STATUS, 1, true)],
            1,
        ),
        (
            "not a whole sector",
            0,
            0,
            vec![(HDR, 16, false), (DATA, 500, true), (STATUS, 1, true)],
            1,
        ),
        (
            "second buffer not a whole sector",
            0,
            0,
            vec![
                (HDR, 16, false),
                (DATA, 512, true),
                (0xA000, 100, true),
                (STATUS, 1, true),
            ],
            1,
        ),
        (
            "past the end by one sector",
            0,
            63,
            vec![(HDR, 16, false), (DATA, 1024, true), (STATUS, 1, true)],
            1,
        ),
        (
            "sector number overflows",
            0,
            u64::MAX,
            vec![(HDR, 16, false), (DATA, 512, true), (STATUS, 1, true)],
            1,
        ),
        (
            "header too short",
            0,
            0,
            vec![(HDR, 8, false), (DATA, 512, true), (STATUS, 1, true)],
            1,
        ),
    ];
    for (name, kind, sector, bufs, want) in cases {
        let mut t = blk_rig();
        let before = t.disk.data.clone();
        blk_request(&mut t.ram, HDR, kind, sector);
        let head = t.q.add(&mut t.ram, &bufs);
        assert_eq!(t.kick_blk(), 1, "{name}");
        if name == "header too short" {
            // the status byte is still reachable but the request is not trusted: no status is promised
            assert_eq!(t.q.take_used(&t.ram).len(), 1, "{name}");
        } else {
            assert_eq!(t.status(), want, "{name}");
            assert_eq!(t.q.take_used(&t.ram)[0].0, u32::from(head), "{name}");
        }
        assert_eq!(t.disk.data, before, "{name}: disk untouched");
        assert!(t.disk.writes.is_empty(), "{name}");
        assert_eq!(t.m.blk.failed, 1, "{name}");
    }
}

#[test]
fn a_backend_failure_midway_is_an_io_error_with_the_partial_count() {
    let mut t = blk_rig();
    t.disk.bad = Some(6);
    blk_request(&mut t.ram, HDR, 0, 5);
    let head = t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (DATA, 1536, true), (STATUS, 1, true)],
    );
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 1);
    assert_eq!(
        t.q.take_used(&t.ram),
        [(u32::from(head), 513)],
        "sector 5 was delivered, 6 failed"
    );
    // the same for writes: sector 5 is written before 6 fails
    t.disk.bad = Some(6);
    blk_request(&mut t.ram, HDR, 1, 5);
    t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (DATA, 1536, false), (STATUS, 1, true)],
    );
    t.kick_blk();
    assert_eq!(t.status(), 1);
    assert_eq!(t.disk.writes, [5]);
}

#[test]
fn memory_the_guest_does_not_have_is_an_io_error_not_a_crash() {
    let mut t = blk_rig();
    blk_request(&mut t.ram, HDR, 0, 0);
    // the data buffer lies beyond the end of RAM
    t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (0x10_0000, 512, true), (STATUS, 1, true)],
    );
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 1);
    // the status itself is unreachable: the chain completes, the device does not fault
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(
        &mut t.ram,
        &[(HDR, 16, false), (0xFFFF_FFFF_FFFF_FFF0, 1, true)],
    );
    assert_eq!(t.kick_blk(), 1);
    assert!(t.m.blk.t.driver_ok(), "not a reason to reset");
}

#[test]
fn many_requests_in_one_kick_complete_in_order_and_the_rings_wrap() {
    let mut t = blk_rig_with(32);
    let mut heads = Vec::new();
    for round in 0..7u64 {
        for i in 0..6u64 {
            let hdr = HDR + 0x100 * i;
            blk_request(&mut t.ram, hdr, 0, round * 6 + i);
            heads.push(t.q.add(
                &mut t.ram,
                &[
                    (hdr, 16, false),
                    (DATA + 0x400 * i, 512, true),
                    (STATUS + 0x10 * i, 1, true),
                ],
            ));
        }
        assert_eq!(t.kick_blk(), 6);
        let used = t.q.take_used(&t.ram);
        assert_eq!(used.len(), 6, "round {round}");
        for (i, (head, n)) in used.iter().enumerate() {
            assert_eq!(*head, u32::from(heads[(round * 6) as usize + i]));
            assert_eq!(*n, 513);
            let mut got = [0u8; SECTOR];
            assert!(t.ram.read(DATA + 0x400 * i as u64, &mut got));
            let s = (round * 6) as usize + i;
            assert_eq!(
                &got[..],
                &t.disk.data[s * SECTOR..(s + 1) * SECTOR],
                "sector {s}"
            );
        }
    }
    assert_eq!(t.m.blk.requests, 42);
    assert_eq!(
        t.q.seen_used, 42,
        "the used index wrapped past the queue size"
    );
}

#[test]
fn a_notification_without_bus_mastering_serves_nothing() {
    let mut t = blk_rig();
    cfg_write(&mut t.m, 3, 0x04, 2, 0x0002); // memory only
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    assert_eq!(t.kick_blk(), 0);
    assert_eq!(t.disk.flushes, 0);
    cfg_write(&mut t.m, 3, 0x04, 2, 0x0006);
    assert_eq!(t.kick_blk(), 1, "the request was waiting");
    assert_eq!(t.disk.flushes, 1);
}

#[test]
fn a_chain_the_device_cannot_follow_needs_reset() {
    // a loop: descriptor 0 points to itself
    let mut t = blk_rig();
    let mut d = [0u8; 16];
    d[8..12].copy_from_slice(&16u32.to_le_bytes());
    d[12..14].copy_from_slice(&1u16.to_le_bytes());
    assert!(t.ram.write(t.q.desc, &d));
    assert!(t.ram.write(t.q.avail + 4, &0u16.to_le_bytes()));
    assert!(t.ram.write(t.q.avail + 2, &1u16.to_le_bytes()));
    assert_eq!(t.kick_blk(), 0);
    assert_ne!(t.m.blk.t.status() & 0x40, 0, "DEVICE_NEEDS_RESET");
    assert_eq!(r(&mut t.m, BLK_BAR, 0x14, 1) & 0x40, 0x40);
    assert!(!t.m.blk.t.driver_ok());
    // indirect descriptors are not supported
    let mut t = blk_rig();
    let mut d = [0u8; 16];
    d[12..14].copy_from_slice(&4u16.to_le_bytes());
    assert!(t.ram.write(t.q.desc, &d));
    assert!(t.ram.write(t.q.avail + 2, &1u16.to_le_bytes()));
    assert_eq!(t.kick_blk(), 0);
    assert_ne!(t.m.blk.t.status() & 0x40, 0);
    // a head beyond the queue size
    let mut t = blk_rig();
    assert!(t.ram.write(t.q.avail + 4, &9u16.to_le_bytes()));
    assert!(t.ram.write(t.q.avail + 2, &1u16.to_le_bytes()));
    assert_eq!(t.kick_blk(), 0);
    assert_ne!(t.m.blk.t.status() & 0x40, 0);
    // more chains made available than the queue holds
    let mut t = blk_rig();
    assert!(t.ram.write(t.q.avail + 2, &9u16.to_le_bytes()));
    assert_eq!(t.kick_blk(), 0);
    assert_ne!(t.m.blk.t.status() & 0x40, 0);
    // a reset clears the flag
    w(&mut t.m, BLK_BAR, 0x14, 1, 0);
    assert_eq!(r(&mut t.m, BLK_BAR, 0x14, 1), 0);
}

#[test]
fn a_chain_of_exactly_the_longest_length_is_followed() {
    let mut t = blk_rig_with(32);
    let mut q = Q::new(0x1000, 32);
    // header + 14 data buffers of one sector + status = 16 descriptors
    blk_request(&mut t.ram, HDR, 0, 0);
    let mut bufs = vec![(HDR, 16, false)];
    for i in 0..14u64 {
        bufs.push((DATA + 0x200 * i, 512, true));
    }
    bufs.push((STATUS, 1, true));
    assert_eq!(bufs.len(), MAX_CHAIN);
    q.add(&mut t.ram, &bufs);
    t.q = q;
    assert_eq!(t.kick_blk(), 1);
    assert_eq!(t.status(), 0);
    assert_eq!(t.m.blk.t.status() & 0x40, 0);
    // 17 descriptors is one too many
    let mut t2 = blk_rig_with(32);
    let mut q = Q::new(0x1000, 32);
    let mut bufs = vec![(HDR, 16, false)];
    for i in 0..15u64 {
        bufs.push((DATA + 0x200 * i, 512, true));
    }
    bufs.push((STATUS, 1, true));
    q.add(&mut t2.ram, &bufs);
    t2.q = q;
    assert_eq!(t2.kick_blk(), 0);
    assert_ne!(t2.m.blk.t.status() & 0x40, 0);
}

// -------------------------------------------------------------- interrupts

#[test]
fn completing_a_request_raises_intx_that_reading_the_isr_clears() {
    let mut t = blk_rig();
    assert!(!t.m.blk.t.irq());
    assert_eq!(
        cfg_read(&mut t.m, 3, 0x06, 2) & 8,
        0,
        "status: no interrupt pending"
    );
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    t.kick_blk();
    assert!(t.m.blk.t.irq());
    assert_eq!(
        cfg_read(&mut t.m, 3, 0x06, 2) & 8,
        8,
        "status: interrupt pending"
    );
    assert_eq!(
        r(&mut t.m, BLK_BAR, 0x1000, 1),
        1,
        "ISR bit 0: queue interrupt"
    );
    assert!(!t.m.blk.t.irq());
    assert_eq!(cfg_read(&mut t.m, 3, 0x06, 2) & 8, 0);
    assert_eq!(r(&mut t.m, BLK_BAR, 0x1000, 1), 0, "reading cleared it");
}

#[test]
fn the_driver_can_ask_not_to_be_interrupted_and_intx_can_be_disabled() {
    let mut t = blk_rig();
    t.ram.write(t.q.avail, &1u16.to_le_bytes()); // VRING_AVAIL_F_NO_INTERRUPT
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    t.kick_blk();
    assert_eq!(t.q.take_used(&t.ram).len(), 1, "the request completed");
    assert!(!t.m.blk.t.irq(), "but there is no interrupt");
    t.ram.write(t.q.avail, &0u16.to_le_bytes());
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    cfg_write(&mut t.m, 3, 0x04, 2, 0x0406); // INTx disable
    t.kick_blk();
    assert!(!t.m.blk.t.irq(), "the command register masks the line");
    assert_eq!(
        r(&mut t.m, BLK_BAR, 0x1000, 1),
        1,
        "yet the ISR still says why"
    );
    cfg_write(&mut t.m, 3, 0x04, 2, 0x0006);
}

fn program_pic(m: &mut Machine) {
    for (cmd, data, icw3, base) in [(0x20u16, 0x21u16, 4u8, 0x20u8), (0xA0, 0xA1, 2, 0x28)] {
        m.io_out(cmd, 1, 0x11, 0);
        m.io_out(data, 1, u32::from(base), 0);
        m.io_out(data, 1, u32::from(icw3), 0);
        m.io_out(data, 1, 1, 0);
        m.io_out(data, 1, 0xFF, 0);
    }
    // IRQ 10 and 11 are level (ELCR), unmask them and the cascade
    m.io_out(0x4D1, 1, 0x0C, 0);
    m.io_out(0x21, 1, 0xFB, 0);
    m.io_out(0xA1, 1, 0xF3, 0);
}

#[test]
fn intx_reaches_the_8259_on_the_line_the_interrupt_line_register_names() {
    let mut t = blk_rig();
    program_pic(&mut t.m);
    assert_eq!(cfg_read(&mut t.m, 3, 0x3C, 1), 11);
    assert_eq!(cfg_read(&mut t.m, 4, 0x3C, 1), 10);
    assert_eq!(t.m.pending(0), None);
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    t.kick_blk();
    assert_eq!(t.m.pending(0), Some(0x28 + 3), "IRQ 11 on the slave");
    assert_eq!(t.m.acknowledge(0), Some(0x2B));
    r(&mut t.m, BLK_BAR, 0x1000, 1); // the driver reads the ISR
    t.m.io_out(0xA0, 1, 0x20, 0);
    t.m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(t.m.pending(0), None, "level dropped with the ISR read");
    // the driver can move the line (BIOS/ACPI routing): the interrupt follows
    cfg_write(&mut t.m, 3, 0x3C, 1, 10);
    t.ram.write(STATUS, &[0xEE]);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    t.kick_blk();
    assert_eq!(t.m.pending(0), Some(0x28 + 2), "now IRQ 10");
}

#[test]
fn two_devices_on_one_8259_line_are_wired_or() {
    let mut t = blk_rig();
    program_pic(&mut t.m);
    // the card is moved onto the disk's line
    cfg_write(&mut t.m, 4, 0x3C, 1, 11);
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    t.kick_blk();
    assert_eq!(t.m.pending(0), Some(0x2B));
    // the quiet card on the same line must not pull it down
    assert_eq!(t.m.pending(0), Some(0x2B), "still the disk's interrupt");
    r(&mut t.m, BLK_BAR, 0x1000, 1);
    t.m.acknowledge(0);
    t.m.io_out(0xA0, 1, 0x20, 0);
    t.m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(t.m.pending(0), None);
}

#[test]
fn intx_reaches_the_io_apic_pin_by_slot_active_low_and_level() {
    let mut t = blk_rig();
    let ioapic_base = ioapic::DEFAULT_BASE;
    let lapic_base = lapic::DEFAULT_BASE;
    t.m.mmio_write(lapic_base + u64::from(reg::SVR), 4, 0x1FF, 0);
    t.m.mmio_write(lapic_base + u64::from(reg::LVT_LINT0), 4, 1 << 16, 0);
    let route = |m: &mut Machine, pin: u8, low: u32| {
        m.mmio_write(ioapic_base, 4, u64::from(0x10 + 2 * pin + 1), 0);
        m.mmio_write(ioapic_base + 0x10, 4, 0, 0);
        m.mmio_write(ioapic_base, 4, u64::from(0x10 + 2 * pin), 0);
        m.mmio_write(ioapic_base + 0x10, 4, u64::from(low), 0);
    };
    let level_low = (1 << 15) | (1 << 13);
    assert_eq!(pci_pin(3), 19);
    assert_eq!(pci_pin(4), 16);
    route(&mut t.m, 19, level_low | 0x51);
    route(&mut t.m, 16, level_low | 0x52);
    assert_eq!(t.m.pending(0), None, "idle: the active-low line is high");
    blk_request(&mut t.ram, HDR, 4, 0);
    t.q.add(&mut t.ram, &[(HDR, 16, false), (STATUS, 1, true)]);
    t.kick_blk();
    assert_eq!(t.m.pending(0), Some(0x51), "the disk is on pin 19");
    assert_eq!(t.m.acknowledge(0), Some(0x51));
    // still asserted: EOI re-delivers (remote IRR cleared, line still low)
    t.m.mmio_write(lapic_base + u64::from(reg::EOI), 4, 0, 0);
    assert_eq!(
        t.m.pending(0),
        Some(0x51),
        "level interrupt fires again until the device is quiet"
    );
    assert_eq!(t.m.acknowledge(0), Some(0x51));
    r(&mut t.m, BLK_BAR, 0x1000, 1);
    t.m.mmio_write(lapic_base + u64::from(reg::EOI), 4, 0, 0);
    assert_eq!(t.m.pending(0), None);
}

// -------------------------------------------------------------- virtio-net

struct NetRig {
    m: Machine,
    ram: Ram,
    wire: Wire,
    rx: Q,
    tx: Q,
}

const NET_FEATURES: u64 = F_VERSION_1 | virtio_net::F_MAC | virtio_net::F_STATUS;
const FRAMES: u64 = 0x20000;

fn net_rig() -> NetRig {
    let mut m = machine();
    let rx = Q::new(0x1000, 8);
    let tx = Q::new(0x4000, 8);
    assert_eq!(bring_up(&mut m, 4, NET_BAR, &[&rx, &tx], NET_FEATURES), 0xF);
    NetRig {
        m,
        ram: Ram(vec![0; 0x40000]),
        wire: Wire::default(),
        rx,
        tx,
    }
}

impl NetRig {
    fn kick(&mut self, q: u32) -> (u32, u32) {
        w(&mut self.m, NET_BAR, 0x3000 + 4 * u64::from(q), 4, q);
        self.m.service_net(&mut self.ram, &mut self.wire, 0)
    }

    fn tx(&mut self, frame: &[u8]) -> u16 {
        let mut p = vec![0u8; virtio_net::HEADER];
        p.extend_from_slice(frame);
        self.ram.write(FRAMES, &p);
        self.tx
            .add(&mut self.ram, &[(FRAMES, p.len() as u32, false)])
    }
}

fn frame(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(3) ^ seed).collect()
}

#[test]
fn a_transmitted_frame_reaches_the_wire_without_its_virtio_header() {
    let mut t = net_rig();
    let f = frame(60, 9);
    let head = t.tx(&f);
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(t.wire.sent, [f]);
    assert_eq!(t.tx.take_used(&t.ram), [(u32::from(head), 0)]);
    assert!(t.m.net.t.irq(), "transmit completion interrupts");
    assert_eq!(t.m.net.sent, 1);
}

#[test]
fn a_header_and_frame_in_separate_descriptors_are_joined() {
    let mut t = net_rig();
    let f = frame(100, 1);
    t.ram.write(FRAMES, &[0u8; 12]);
    t.ram.write(FRAMES + 0x1000, &f);
    t.tx.add(
        &mut t.ram,
        &[(FRAMES, 12, false), (FRAMES + 0x1000, 100, false)],
    );
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(t.wire.sent, [f]);
}

#[test]
fn transmit_buffers_the_device_may_not_read_or_that_are_too_short_are_dropped() {
    let mut t = net_rig();
    t.ram.write(FRAMES, &[0u8; 64]);
    t.tx.add(&mut t.ram, &[(FRAMES, 64, true)]); // device-writable: wrong direction
    t.tx.add(&mut t.ram, &[(FRAMES, 11, false)]); // shorter than the header
    t.tx.add(&mut t.ram, &[(FRAMES, 12 + MAX_FRAME as u32 + 1, false)]); // too large
    t.tx.add(&mut t.ram, &[(0x10_0000, 64, false)]); // not RAM
    assert_eq!(t.kick(1), (4, 0));
    assert!(t.wire.sent.is_empty());
    assert_eq!(t.m.net.tx_errors, 4);
    assert_eq!(t.tx.take_used(&t.ram).len(), 4, "every chain is returned");
    // a header-only frame is the shortest that is sent (zero payload)
    t.tx.add(&mut t.ram, &[(FRAMES, 12, false)]);
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(t.wire.sent, [Vec::<u8>::new()]);
    // the largest frame goes through whole
    let f = frame(MAX_FRAME, 5);
    t.tx(&f);
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(t.wire.sent[1], f);
}

#[test]
fn a_received_frame_lands_behind_a_header_with_one_buffer() {
    let mut t = net_rig();
    let f = frame(80, 4);
    t.wire.inbox.push_back(f.clone());
    let head = t.rx.add(&mut t.ram, &[(FRAMES, 2048, true)]);
    assert_eq!(t.kick(0), (0, 1));
    let mut got = vec![0u8; 12 + 80];
    assert!(t.ram.read(FRAMES, &mut got));
    assert_eq!(&got[..10], &[0u8; 10], "no offloads");
    assert_eq!(&got[10..12], &1u16.to_le_bytes(), "num_buffers = 1");
    assert_eq!(&got[12..], &f[..]);
    assert_eq!(t.rx.take_used(&t.ram), [(u32::from(head), 92)]);
    assert!(t.m.net.t.irq());
    assert_eq!(t.m.net.received, 1);
    assert_eq!(t.m.net.rx_truncated, 0);
}

#[test]
fn a_received_frame_spills_over_several_buffers() {
    let mut t = net_rig();
    let f = frame(300, 8);
    t.wire.inbox.push_back(f.clone());
    t.rx.add(
        &mut t.ram,
        &[
            (FRAMES, 100, true),
            (FRAMES + 0x1000, 100, true),
            (FRAMES + 0x2000, 200, true),
        ],
    );
    assert_eq!(t.kick(0), (0, 1));
    let mut all = Vec::new();
    for (a, n) in [
        (FRAMES, 100usize),
        (FRAMES + 0x1000, 100),
        (FRAMES + 0x2000, 112),
    ] {
        let mut b = vec![0u8; n];
        assert!(t.ram.read(a, &mut b));
        all.extend(b);
    }
    assert_eq!(&all[12..], &f[..]);
    assert_eq!(t.rx.take_used(&t.ram)[0].1, 312);
    assert_eq!(t.m.net.rx_truncated, 0);
}

#[test]
fn a_frame_that_does_not_fit_is_truncated_and_counted() {
    let mut t = net_rig();
    t.wire.inbox.push_back(frame(300, 8));
    t.rx.add(&mut t.ram, &[(FRAMES, 100, true)]);
    assert_eq!(t.kick(0), (0, 1));
    assert_eq!(t.rx.take_used(&t.ram)[0].1, 100);
    assert_eq!(t.m.net.rx_truncated, 1);
}

#[test]
fn frames_wait_in_the_network_until_the_guest_posts_buffers_and_buffers_wait_for_frames() {
    let mut t = net_rig();
    t.wire.inbox.push_back(frame(64, 1));
    t.wire.inbox.push_back(frame(65, 2));
    assert_eq!(
        t.kick(0),
        (0, 0),
        "no buffers: nothing is taken from the wire"
    );
    assert_eq!(t.wire.inbox.len(), 2);
    t.rx.add(&mut t.ram, &[(FRAMES, 2048, true)]);
    assert_eq!(t.kick(0), (0, 1), "one buffer, one frame");
    assert_eq!(t.wire.inbox.len(), 1);
    let mut empty = net_rig();
    empty.rx.add(&mut empty.ram, &[(FRAMES, 2048, true)]);
    assert_eq!(empty.kick(0), (0, 0), "a buffer with no frame stays posted");
    empty.wire.inbox.push_back(frame(70, 3));
    assert_eq!(
        empty.kick(0),
        (0, 1),
        "and takes the next frame that arrives"
    );
    assert_eq!(empty.rx.take_used(&empty.ram)[0].1, 82);
    let mut got = [0u8; 70];
    assert!(empty.ram.read(FRAMES + 12, &mut got));
    assert_eq!(&got[..], &frame(70, 3)[..]);
}

#[test]
fn a_receive_buffer_the_device_may_not_write_receives_nothing() {
    let mut t = net_rig();
    t.wire.inbox.push_back(frame(64, 1));
    t.ram.write(FRAMES, &[0x55; 128]);
    t.rx.add(&mut t.ram, &[(FRAMES, 128, false)]);
    assert_eq!(t.kick(0), (0, 1));
    let mut got = [0u8; 128];
    assert!(t.ram.read(FRAMES, &mut got));
    assert_eq!(got, [0x55; 128], "a read-only buffer is left alone");
    assert_eq!(t.rx.take_used(&t.ram)[0].1, 0);
    assert_eq!(t.m.net.rx_truncated, 1);
}

#[test]
fn net_service_is_idle_without_bus_mastering() {
    let mut t = net_rig();
    cfg_write(&mut t.m, 4, 0x04, 2, 0x0002);
    t.wire.inbox.push_back(frame(64, 1));
    t.rx.add(&mut t.ram, &[(FRAMES, 2048, true)]);
    t.tx(&frame(64, 2));
    assert_eq!(t.kick(1), (0, 0));
    assert!(t.wire.sent.is_empty());
    assert_eq!(t.wire.inbox.len(), 1);
}

#[test]
fn the_network_interrupt_uses_pin_16_and_line_10() {
    let mut t = net_rig();
    program_pic(&mut t.m);
    t.tx(&frame(64, 2));
    t.kick(1);
    assert_eq!(t.m.pending(0), Some(0x28 + 2), "IRQ 10");
    assert_eq!(t.m.acknowledge(0), Some(0x2A));
    assert_eq!(r(&mut t.m, NET_BAR, 0x1000, 1), 1);
    t.m.io_out(0xA0, 1, 0x20, 0);
    t.m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(t.m.pending(0), None);
}

#[test]
fn both_devices_share_the_bus_without_interfering() {
    let mut m = machine();
    let q = Q::new(0x1000, 8);
    let rx = Q::new(0x4000, 8);
    let tx = Q::new(0x8000, 8);
    bring_up(&mut m, 3, BLK_BAR, &[&q], BLK_FEATURES);
    bring_up(&mut m, 4, NET_BAR, &[&rx, &tx], NET_FEATURES);
    assert_eq!(r(&mut m, BLK_BAR, 0x14, 1), 0xF);
    assert_eq!(r(&mut m, NET_BAR, 0x14, 1), 0xF);
    // resetting the card leaves the disk alone
    w(&mut m, NET_BAR, 0x14, 1, 0);
    assert_eq!(r(&mut m, NET_BAR, 0x14, 1), 0);
    assert_eq!(r(&mut m, BLK_BAR, 0x14, 1), 0xF);
    assert_eq!(m.net.t.resets, 2);
    assert_eq!(m.blk.t.resets, 1);
}
