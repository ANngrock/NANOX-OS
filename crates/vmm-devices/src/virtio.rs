//! The virtio 1.x PCI transport ("modern", no legacy I/O interface) and the
//! split virtqueue, as a device sees them: the PCI function (vendor 0x1AF4,
//! device 0x1040 + type, capabilities for the common, notification, ISR and
//! device-specific configuration structures, all in memory BAR 0), the
//! handshake through the device status register, feature negotiation,
//! queue set-up, notifications, the ISR status and the INTx line.
//!
//! The queue engine reads and writes the guest's memory through
//! [`GuestMemory`]; it validates every address, bounds chains by the queue
//! size, and refuses what it does not implement (indirect descriptors, packed
//! rings) by setting DEVICE_NEEDS_RESET, which is what the specification asks
//! of a device that has been given something broken. Devices built on it are
//! `virtio_blk` and `virtio_net`.
//!
//! BAR 0 (16 KiB): common configuration at 0x0000, ISR at 0x1000, device
//! configuration at 0x2000, notifications at 0x3000 (queue `n` at
//! `0x3000 + 4n`, multiplier 4).

use crate::pci::{BarKind, Config};

pub const VENDOR: u16 = 0x1AF4;
pub const BAR_SIZE: u32 = 0x4000;
const COMMON_LEN: u64 = 0x38;
const ISR: u64 = 0x1000;
const DEVICE: u64 = 0x2000;
pub const DEVICE_LEN: usize = 64;
const NOTIFY: u64 = 0x3000;
const NOTIFY_MULTIPLIER: u32 = 4;

pub const F_VERSION_1: u64 = 1 << 32;
pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_NEEDS_RESET: u8 = 0x40;
pub const STATUS_FAILED: u8 = 0x80;

pub const MAX_QUEUES: usize = 2;
/// The largest queue the device accepts.
pub const MAX_QUEUE_SIZE: u16 = 128;
/// Descriptors followed in one chain at most.
pub const MAX_CHAIN: usize = 16;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const DESC_INDIRECT: u16 = 4;
const AVAIL_NO_INTERRUPT: u16 = 1;

/// The guest's physical memory, as a DMA-capable device sees it.
pub trait GuestMemory {
    /// Copies `buf.len()` bytes from guest address `gpa`; false if any byte is not guest RAM.
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool;
    /// Copies `data` to guest address `gpa`; false if any byte is not guest RAM.
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool;
}

fn rd16(m: &dyn GuestMemory, a: u64) -> Option<u16> {
    let mut b = [0u8; 2];
    m.read(a, &mut b).then(|| u16::from_le_bytes(b))
}

fn rd32(m: &dyn GuestMemory, a: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    m.read(a, &mut b).then(|| u32::from_le_bytes(b))
}

fn rd64(m: &dyn GuestMemory, a: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    m.read(a, &mut b).then(|| u64::from_le_bytes(b))
}

/// One buffer of a descriptor chain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    /// The device writes it (otherwise it reads it).
    pub write: bool,
}

/// A descriptor chain the driver made available.
#[derive(Clone, Copy, Debug)]
pub struct Chain {
    pub head: u16,
    pub descs: [Desc; MAX_CHAIN],
    pub len: usize,
}

impl Chain {
    pub fn descs(&self) -> &[Desc] {
        &self.descs[..self.len]
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Queue {
    size: u16,
    enabled: bool,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
    used_idx: u16,
}

#[derive(Clone, Debug)]
pub struct VirtioPci {
    pub cfg: Config,
    device_type: u16,
    queues_n: usize,
    offered: u64,
    accepted: u64,
    device_sel: u32,
    driver_sel: u32,
    status: u8,
    queue_sel: u16,
    queue: [Queue; MAX_QUEUES],
    isr: u8,
    dev_cfg: [u8; DEVICE_LEN],
    /// Queues the driver notified since the device last looked (bit per queue).
    kicks: u32,
    pub resets: u32,
}

fn cap(cfg_type: u8, offset: u32, length: u32, extra: Option<u32>) -> ([u8; 20], usize) {
    let mut b = [0u8; 20];
    b[0] = 0x09; // vendor-specific
    b[2] = if extra.is_some() { 20 } else { 16 };
    b[3] = cfg_type;
    b[4] = 0; // BAR 0
    b[8..12].copy_from_slice(&offset.to_le_bytes());
    b[12..16].copy_from_slice(&length.to_le_bytes());
    if let Some(m) = extra {
        b[16..20].copy_from_slice(&m.to_le_bytes());
    }
    (b, if extra.is_some() { 20 } else { 16 })
}

impl VirtioPci {
    /// A modern virtio PCI function of `device_type` (1 net, 2 block) with `queues` queues and
    /// `features` (without VERSION_1, which is always offered), class code `class`, INTA on `line`.
    pub fn new(device_type: u16, class: u32, queues: usize, features: u64, line: u8) -> Self {
        assert!(queues <= MAX_QUEUES);
        let mut cfg = Config::new(
            VENDOR,
            0x1040 + device_type,
            class,
            1,
            (VENDOR, device_type),
            1,
        );
        cfg.define_bar(0, BarKind::Memory(BAR_SIZE));
        cfg.write(crate::pci::INTERRUPT_LINE, 1, u32::from(line));
        for (t, off, len, extra) in [
            (1u8, 0, COMMON_LEN, None),
            (2, NOTIFY, 4 * queues as u64, Some(NOTIFY_MULTIPLIER)),
            (3, ISR, 4, None),
            (4, DEVICE, DEVICE_LEN as u64, None),
        ] {
            let (b, n) = cap(t, off as u32, len as u32, extra);
            cfg.add_capability(&b[..n]);
        }
        Self {
            cfg,
            device_type,
            queues_n: queues,
            offered: features | F_VERSION_1,
            accepted: 0,
            device_sel: 0,
            driver_sel: 0,
            status: 0,
            queue_sel: 0,
            queue: [Queue {
                size: MAX_QUEUE_SIZE,
                ..Queue::default()
            }; MAX_QUEUES],
            isr: 0,
            dev_cfg: [0; DEVICE_LEN],
            kicks: 0,
            resets: 0,
        }
    }

    pub fn device_type(&self) -> u16 {
        self.device_type
    }

    /// Fills the device-specific configuration (byte offsets from 0).
    pub fn set_device_config(&mut self, at: usize, bytes: &[u8]) {
        self.dev_cfg[at..at + bytes.len()].copy_from_slice(bytes);
    }

    pub fn status(&self) -> u8 {
        self.status
    }

    pub fn driver_ok(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0
            && self.status & (STATUS_NEEDS_RESET | STATUS_FAILED) == 0
    }

    /// The features the driver selected.
    pub fn accepted_features(&self) -> u64 {
        self.accepted
    }

    /// May the device master the bus (DMA)? The PCI command register says.
    pub fn dma_allowed(&self) -> bool {
        self.cfg.bus_master()
    }

    /// The INTx output: asserted while the ISR is non-zero and the command register allows it.
    pub fn irq(&self) -> bool {
        self.isr != 0 && self.cfg.intx_enabled()
    }

    fn refresh(&mut self) {
        let on = self.isr != 0;
        self.cfg.set_interrupt_status(on);
    }

    /// Queue bits the driver notified since the last call (cleared).
    pub fn take_kicks(&mut self) -> u32 {
        core::mem::take(&mut self.kicks)
    }

    /// Is queue `q` ready to be served (enabled, driver running)?
    pub fn queue_ready(&self, q: usize) -> bool {
        q < self.queues_n && self.queue[q].enabled && self.driver_ok()
    }

    /// Raises the queue interrupt (ISR bit 0).
    pub fn interrupt(&mut self) {
        self.isr |= 1;
        self.refresh();
    }

    fn reset(&mut self) {
        self.accepted = 0;
        self.status = 0;
        self.queue_sel = 0;
        self.isr = 0;
        self.kicks = 0;
        self.queue = [Queue {
            size: MAX_QUEUE_SIZE,
            ..Queue::default()
        }; MAX_QUEUES];
        self.resets += 1;
        self.refresh();
    }

    /// The device found something it cannot work with: DEVICE_NEEDS_RESET.
    pub fn needs_reset(&mut self) {
        self.status |= STATUS_NEEDS_RESET;
    }

    fn selected(&self) -> Option<usize> {
        let q = usize::from(self.queue_sel);
        (q < self.queues_n).then_some(q)
    }

    // ----------------------------------------------------------- BAR 0 MMIO

    /// A read of `size` (1, 2 or 4) bytes at `offset` of BAR 0.
    pub fn mmio_read(&mut self, offset: u64, size: u8) -> u32 {
        match offset {
            o if o < COMMON_LEN => self.common_read(o, size),
            ISR => {
                let v = u32::from(self.isr);
                self.isr = 0; // reading clears it
                self.refresh();
                v
            }
            o if (DEVICE..DEVICE + DEVICE_LEN as u64).contains(&o) => {
                let at = (o - DEVICE) as usize;
                let mut w = [0u8; 4];
                for (i, b) in w.iter_mut().enumerate().take(usize::from(size)) {
                    *b = self.dev_cfg.get(at + i).copied().unwrap_or(0);
                }
                u32::from_le_bytes(w)
            }
            _ => 0,
        }
    }

    /// The common configuration structure as the driver would read it right now.
    fn common_image(&self) -> [u8; COMMON_LEN as usize] {
        let mut b = [0u8; COMMON_LEN as usize];
        let put = |b: &mut [u8], at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
        put(&mut b, 0x00, &self.device_sel.to_le_bytes());
        let features = if self.device_sel < 2 {
            (self.offered >> (32 * self.device_sel)) as u32
        } else {
            0
        };
        put(&mut b, 0x04, &features.to_le_bytes());
        put(&mut b, 0x08, &self.driver_sel.to_le_bytes());
        let driver = if self.driver_sel < 2 {
            (self.accepted >> (32 * self.driver_sel)) as u32
        } else {
            0
        };
        put(&mut b, 0x0C, &driver.to_le_bytes());
        put(&mut b, 0x10, &0xFFFFu16.to_le_bytes()); // msix_config: NO_VECTOR
        put(&mut b, 0x12, &(self.queues_n as u16).to_le_bytes());
        b[0x14] = self.status;
        put(&mut b, 0x16, &self.queue_sel.to_le_bytes());
        put(&mut b, 0x1A, &0xFFFFu16.to_le_bytes()); // queue_msix_vector: NO_VECTOR
        if let Some(q) = self.selected() {
            let s = &self.queue[q];
            put(&mut b, 0x18, &s.size.to_le_bytes());
            put(&mut b, 0x1C, &u16::from(s.enabled).to_le_bytes());
            put(&mut b, 0x1E, &(q as u16).to_le_bytes());
            put(&mut b, 0x20, &s.desc.to_le_bytes());
            put(&mut b, 0x28, &s.avail.to_le_bytes());
            put(&mut b, 0x30, &s.used.to_le_bytes());
        }
        b
    }

    fn common_read(&mut self, o: u64, size: u8) -> u32 {
        let img = self.common_image();
        let o = o as usize;
        let mut w = [0u8; 4];
        for (i, byte) in w.iter_mut().enumerate().take(usize::from(size)) {
            *byte = img.get(o + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(w)
    }

    /// A write of `size` (1, 2 or 4) bytes at `offset` of BAR 0.
    pub fn mmio_write(&mut self, offset: u64, size: u8, value: u32) {
        if (NOTIFY..NOTIFY + 4 * self.queues_n as u64).contains(&offset) {
            let q = ((offset - NOTIFY) / 4) as u32;
            self.kicks |= 1 << q;
            return;
        }
        if offset < COMMON_LEN {
            self.common_write(offset, size, value);
        }
    }

    fn common_write(&mut self, o: u64, size: u8, v: u32) {
        match (o, size) {
            (0x00, 4) => self.device_sel = v,
            (0x08, 4) => self.driver_sel = v,
            (0x0C, 4) => {
                let part = u64::from(v) << (32 * (self.driver_sel & 1));
                let keep = !(0xFFFF_FFFFu64 << (32 * (self.driver_sel & 1)));
                if self.driver_sel < 2 {
                    self.accepted = (self.accepted & keep) | part;
                }
            }
            (0x14, 1) => self.write_status(v as u8),
            (0x16, 2) => self.queue_sel = v as u16,
            (0x18, 2) => {
                if let Some(q) = self.selected() {
                    let n = v as u16;
                    if !self.queue[q].enabled && n <= MAX_QUEUE_SIZE && n.is_power_of_two() {
                        self.queue[q].size = n;
                    }
                }
            }
            (0x1C, 2) => {
                if let Some(q) = self.selected() {
                    let ready = {
                        let s = &self.queue[q];
                        s.desc != 0 && s.avail != 0 && s.used != 0
                    };
                    if v == 1 && ready {
                        self.queue[q].enabled = true;
                    }
                }
            }
            (0x20 | 0x28 | 0x30, 4) | (0x24 | 0x2C | 0x34, 4) => {
                if let Some(q) = self.selected() {
                    if !self.queue[q].enabled {
                        let high = o & 4 != 0;
                        let slot = match o & !4 {
                            0x20 => &mut self.queue[q].desc,
                            0x28 => &mut self.queue[q].avail,
                            _ => &mut self.queue[q].used,
                        };
                        *slot = if high {
                            (*slot & 0xFFFF_FFFF) | u64::from(v) << 32
                        } else {
                            (*slot & !0xFFFF_FFFF) | u64::from(v)
                        };
                    }
                }
            }
            _ => {}
        }
    }

    fn write_status(&mut self, v: u8) {
        if v == 0 {
            self.reset();
            return;
        }
        let mut s = v;
        // The device accepts FEATURES_OK only for features it offered and VERSION_1 included.
        if self.accepted & !self.offered != 0 || self.accepted & F_VERSION_1 == 0 {
            s &= !STATUS_FEATURES_OK;
        }
        // Status bits only ever get set by the driver; the device's own bits are kept.
        self.status = s | (self.status & (STATUS_NEEDS_RESET));
    }

    // ----------------------------------------------------------- the queues

    /// The next available chain of queue `q`, if the driver put one there.
    /// A chain the device cannot follow sets DEVICE_NEEDS_RESET and returns `None`.
    pub fn pop(&mut self, mem: &dyn GuestMemory, q: usize) -> Option<Chain> {
        if !self.queue_ready(q) {
            return None;
        }
        let s = self.queue[q];
        let avail_idx = rd16(mem, s.avail + 2)?;
        if avail_idx == s.last_avail {
            return None;
        }
        // The driver may not make more than `size` chains available.
        if avail_idx.wrapping_sub(s.last_avail) > s.size {
            self.needs_reset();
            return None;
        }
        let head = rd16(mem, s.avail + 4 + 2 * u64::from(s.last_avail % s.size))?;
        let mut chain = Chain {
            head,
            descs: [Desc::default(); MAX_CHAIN],
            len: 0,
        };
        let mut next = head;
        loop {
            if next >= s.size || chain.len == MAX_CHAIN {
                self.needs_reset();
                return None;
            }
            let at = s.desc + 16 * u64::from(next);
            let (addr, len, flags, link) = (
                rd64(mem, at)?,
                rd32(mem, at + 8)?,
                rd16(mem, at + 12)?,
                rd16(mem, at + 14)?,
            );
            if flags & DESC_INDIRECT != 0 {
                self.needs_reset();
                return None;
            }
            chain.descs[chain.len] = Desc {
                addr,
                len,
                write: flags & DESC_WRITE != 0,
            };
            chain.len += 1;
            if flags & DESC_NEXT == 0 {
                break;
            }
            next = link;
        }
        self.queue[q].last_avail = s.last_avail.wrapping_add(1);
        Some(chain)
    }

    /// Gives the last chain popped from queue `q` back (the device could not use it yet).
    pub fn unpop(&mut self, q: usize) {
        self.queue[q].last_avail = self.queue[q].last_avail.wrapping_sub(1);
    }

    /// Returns a chain to the driver: `written` bytes were written into its device-writable buffers.
    pub fn push_used(
        &mut self,
        mem: &mut dyn GuestMemory,
        q: usize,
        head: u16,
        written: u32,
    ) -> bool {
        let s = self.queue[q];
        let at = s.used + 4 + 8 * u64::from(s.used_idx % s.size);
        let mut entry = [0u8; 8];
        entry[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        entry[4..].copy_from_slice(&written.to_le_bytes());
        let idx = s.used_idx.wrapping_add(1);
        if !mem.write(at, &entry) || !mem.write(s.used + 2, &idx.to_le_bytes()) {
            self.needs_reset();
            return false;
        }
        self.queue[q].used_idx = idx;
        // The driver may ask not to be interrupted.
        let flags = rd16(mem, s.avail).unwrap_or(0);
        if flags & AVAIL_NO_INTERRUPT == 0 {
            self.interrupt();
        }
        true
    }
}
