//! A virtio 1.x block-device driver over the modern PCI transport: what a
//! VMM uses to read a disk of the machine it runs on (the probe's
//! `virtio-blk-pci` under QEMU, holding a multi-GiB guest image), and what
//! NANOX's own storage service can start from (ROADMAP M4).
//!
//! The driver finds the common, notification and device configuration
//! structures through the function's vendor capabilities, resets the device,
//! accepts VERSION_1 (and RO, when offered) and nothing else, sets up queue 0
//! with [`QUEUE`] entries and runs one request at a time: a header, the data
//! and a status byte as a chain of three descriptors, then a notification,
//! then the used ring is polled (interrupts stay suppressed). Requests longer
//! than [`MAX_IO`] are split.
//!
//! Everything the driver touches goes through [`Bus`]: the function's
//! configuration space, memory-mapped registers and the physical memory of
//! its [`DmaRegion`]. `no_std`, no allocation, safe Rust: the caller's `Bus`
//! holds whatever `unsafe` the platform needs.

#![no_std]
#![forbid(unsafe_code)]

/// What the driver reaches of the machine.
pub trait Bus {
    /// A dword of the function's PCI configuration space (`off` dword-aligned).
    fn cfg_read32(&mut self, off: u16) -> u32;
    fn cfg_write32(&mut self, off: u16, v: u32);
    /// A memory-mapped register of `size` bytes (1, 2 or 4).
    fn mmio_read(&mut self, addr: u64, size: u8) -> u32;
    fn mmio_write(&mut self, addr: u64, size: u8, v: u32);
    /// Physical memory of the DMA region. A read must see what the device
    /// wrote since the last one — on hardware, volatile accesses: the driver
    /// polls the used ring through it, and a plain load could be kept from
    /// the first iteration of the loop.
    fn mem_read(&mut self, pa: u64, buf: &mut [u8]);
    fn mem_write(&mut self, pa: u64, data: &[u8]);
    /// Orders the memory writes before it against the device (a store fence
    /// on hardware; nothing for a device that runs only when called).
    fn fence(&mut self) {}
}

pub const SECTOR: usize = 512;
/// The longest single request.
pub const MAX_IO: usize = 64 << 10;
/// Entries in the driver's queue: a request takes three.
pub const QUEUE: u16 = 8;
/// DMA memory a [`Blk`] needs: rings, header and status, then the data.
pub const DMA_BYTES: u64 = DATA + MAX_IO as u64;
/// How often the used ring is read before a request is given up.
pub const DEFAULT_SPINS: u32 = 1 << 26;

pub const VENDOR: u16 = 0x1AF4;
/// Transitional and modern virtio-blk.
pub const DEVICE_TRANSITIONAL: u16 = 0x1001;
pub const DEVICE_MODERN: u16 = 0x1042;
pub const F_RO: u64 = 1 << 5;
pub const F_VERSION_1: u64 = 1 << 32;

// Offsets in the DMA region.
const DESC: u64 = 0;
const AVAIL: u64 = 0x100;
const USED: u64 = 0x200;
const HEADER: u64 = 0x400;
const STATUS: u64 = 0x410;
const DATA: u64 = 0x1000;

// Common configuration registers.
const DEVICE_FEATURE_SELECT: u64 = 0x00;
const DEVICE_FEATURE: u64 = 0x04;
const DRIVER_FEATURE_SELECT: u64 = 0x08;
const DRIVER_FEATURE: u64 = 0x0C;
const DEVICE_STATUS: u64 = 0x14;
const CONFIG_GENERATION: u64 = 0x15;
const QUEUE_SELECT: u64 = 0x16;
const QUEUE_SIZE: u64 = 0x18;
const QUEUE_ENABLE: u64 = 0x1C;
const QUEUE_NOTIFY_OFF: u64 = 0x1E;
const QUEUE_DESC: u64 = 0x20;
const QUEUE_DRIVER: u64 = 0x28;
const QUEUE_DEVICE: u64 = 0x30;

const ACKNOWLEDGE: u8 = 1;
const DRIVER: u8 = 2;
const DRIVER_OK: u8 = 4;
const FEATURES_OK: u8 = 8;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
const T_IN: u32 = 0;
const T_OUT: u32 = 1;

const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_DEVICE: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The function is not a virtio block device.
    NotVirtioBlk,
    /// A configuration structure the driver needs has no capability, or an I/O BAR.
    NoCapability,
    /// The device did not take the features (it lacks VERSION_1, or refused).
    FeaturesRefused,
    /// Queue 0 cannot be used.
    QueueUnavailable,
    /// The DMA region is shorter than [`DMA_BYTES`].
    RegionTooSmall,
    /// A buffer that is not whole sectors, or a request past the disk's end.
    BadRequest,
    /// The device answered with this status (1: I/O error, 2: unsupported).
    Device(u8),
    /// The device did not answer within the spins.
    Timeout,
}

/// DMA-capable memory for the driver: `len` bytes at physical address `pa`,
/// 4 KiB-aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaRegion {
    pub pa: u64,
    pub len: u64,
}

/// An open virtio block device.
#[derive(Clone, Debug)]
pub struct Blk {
    common: u64,
    notify: u64,
    device: u64,
    region: DmaRegion,
    size: u16,
    avail_idx: u16,
    used_idx: u16,
    /// Sectors on the disk.
    pub capacity: u64,
    pub read_only: bool,
    /// See [`DEFAULT_SPINS`].
    pub spins: u32,
    /// A request timed out: the device may still complete it later, so the
    /// queue's state is unknown and every request fails until [`Blk::open`]
    /// resets the device.
    stuck: bool,
}

/// The address `cfg` structure of type `kind` starts at: its BAR's memory
/// address plus its offset.
fn structure(bus: &mut impl Bus, kind: u8) -> Result<(u64, u32), Error> {
    let status = bus.cfg_read32(0x04) >> 16;
    if status & 0x10 == 0 {
        return Err(Error::NoCapability);
    }
    let mut ptr = (bus.cfg_read32(0x34) & 0xFC) as u16;
    // Bounded: a configuration space has room for 48 capabilities.
    for _ in 0..48 {
        if ptr < 0x40 {
            break;
        }
        let head = bus.cfg_read32(ptr);
        let (id, next, cfg_type) = (head as u8, (head >> 8) as u8, (head >> 24) as u8);
        if id == CAP_VENDOR && cfg_type == kind {
            let bar = (bus.cfg_read32(ptr + 4) & 0xFF) as u16;
            let offset = bus.cfg_read32(ptr + 8);
            let extra = if kind == CFG_NOTIFY {
                bus.cfg_read32(ptr + 16)
            } else {
                0
            };
            if bar > 5 {
                return Err(Error::NoCapability);
            }
            let low = bus.cfg_read32(0x10 + 4 * bar);
            if low & 1 != 0 {
                return Err(Error::NoCapability); // an I/O BAR
            }
            let mut base = u64::from(low & !0xF);
            if (low >> 1) & 3 == 2 && bar < 5 {
                base |= u64::from(bus.cfg_read32(0x14 + 4 * bar)) << 32;
            }
            return Ok((base + u64::from(offset), extra));
        }
        ptr = u16::from(next & 0xFC);
    }
    Err(Error::NoCapability)
}

impl Blk {
    /// Opens the device behind `bus` with `region` for its rings and buffers:
    /// memory space and bus mastering on, reset, the handshake, queue 0.
    pub fn open(bus: &mut impl Bus, region: DmaRegion) -> Result<Self, Error> {
        let id = bus.cfg_read32(0);
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor != VENDOR || !matches!(device, DEVICE_TRANSITIONAL | DEVICE_MODERN) {
            return Err(Error::NotVirtioBlk);
        }
        if region.len < DMA_BYTES {
            return Err(Error::RegionTooSmall);
        }
        let command = bus.cfg_read32(0x04) & 0xFFFF;
        bus.cfg_write32(0x04, command | 0x6);
        let (common, _) = structure(bus, CFG_COMMON)?;
        let (notify_base, multiplier) = structure(bus, CFG_NOTIFY)?;
        let (device_cfg, _) = structure(bus, CFG_DEVICE)?;
        let w8 = |bus: &mut _, off, v| Bus::mmio_write(bus, common + off, 1, v);
        // Reset, then acknowledge.
        w8(bus, DEVICE_STATUS, 0);
        for _ in 0..1000 {
            if bus.mmio_read(common + DEVICE_STATUS, 1) == 0 {
                break;
            }
        }
        let mut status = u32::from(ACKNOWLEDGE | DRIVER);
        w8(bus, DEVICE_STATUS, status);
        let mut offered = 0u64;
        for half in 0..2 {
            bus.mmio_write(common + DEVICE_FEATURE_SELECT, 4, half);
            offered |= u64::from(bus.mmio_read(common + DEVICE_FEATURE, 4)) << (32 * half);
        }
        if offered & F_VERSION_1 == 0 {
            w8(bus, DEVICE_STATUS, 0x80);
            return Err(Error::FeaturesRefused);
        }
        let accepted = offered & (F_VERSION_1 | F_RO);
        for half in 0..2 {
            bus.mmio_write(common + DRIVER_FEATURE_SELECT, 4, half);
            bus.mmio_write(common + DRIVER_FEATURE, 4, (accepted >> (32 * half)) as u32);
        }
        status |= u32::from(FEATURES_OK);
        w8(bus, DEVICE_STATUS, status);
        if bus.mmio_read(common + DEVICE_STATUS, 1) & u32::from(FEATURES_OK) == 0 {
            return Err(Error::FeaturesRefused);
        }
        // Queue 0.
        bus.mmio_write(common + QUEUE_SELECT, 2, 0);
        let max = bus.mmio_read(common + QUEUE_SIZE, 2) as u16;
        if max == 0 {
            return Err(Error::QueueUnavailable);
        }
        let size = max.min(QUEUE);
        bus.mmio_write(common + QUEUE_SIZE, 2, u32::from(size));
        let zero = [0u8; 0x400];
        bus.mem_write(region.pa, &zero);
        bus.mem_write(region.pa + AVAIL, &AVAIL_NO_INTERRUPT.to_le_bytes());
        for (off, at) in [
            (QUEUE_DESC, DESC),
            (QUEUE_DRIVER, AVAIL),
            (QUEUE_DEVICE, USED),
        ] {
            let pa = region.pa + at;
            bus.mmio_write(common + off, 4, pa as u32);
            bus.mmio_write(common + off + 4, 4, (pa >> 32) as u32);
        }
        let notify_off = bus.mmio_read(common + QUEUE_NOTIFY_OFF, 2);
        bus.mmio_write(common + QUEUE_ENABLE, 2, 1);
        status |= u32::from(DRIVER_OK);
        w8(bus, DEVICE_STATUS, status);
        // The capacity, read again while the configuration changes under it.
        let mut capacity = 0;
        for _ in 0..16 {
            let generation = bus.mmio_read(common + CONFIG_GENERATION, 1);
            let low = bus.mmio_read(device_cfg, 4);
            let high = bus.mmio_read(device_cfg + 4, 4);
            capacity = u64::from(high) << 32 | u64::from(low);
            if bus.mmio_read(common + CONFIG_GENERATION, 1) == generation {
                break;
            }
        }
        Ok(Self {
            common,
            notify: notify_base + u64::from(notify_off) * u64::from(multiplier),
            device: device_cfg,
            region,
            size,
            avail_idx: 0,
            used_idx: 0,
            capacity,
            read_only: accepted & F_RO != 0,
            spins: DEFAULT_SPINS,
            stuck: false,
        })
    }

    /// Reads whole sectors from `sector` into `buf`.
    pub fn read(&mut self, bus: &mut impl Bus, sector: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.check(sector, buf.len())?;
        for (i, chunk) in buf.chunks_mut(MAX_IO).enumerate() {
            let at = sector + (i * MAX_IO / SECTOR) as u64;
            self.request(bus, T_IN, at, chunk.len())?;
            bus.mem_read(self.region.pa + DATA, chunk);
        }
        Ok(())
    }

    /// Writes whole sectors from `data` at `sector`.
    pub fn write(&mut self, bus: &mut impl Bus, sector: u64, data: &[u8]) -> Result<(), Error> {
        self.check(sector, data.len())?;
        for (i, chunk) in data.chunks(MAX_IO).enumerate() {
            let at = sector + (i * MAX_IO / SECTOR) as u64;
            bus.mem_write(self.region.pa + DATA, chunk);
            self.request(bus, T_OUT, at, chunk.len())?;
        }
        Ok(())
    }

    fn check(&self, sector: u64, len: usize) -> Result<(), Error> {
        let sectors = (len / SECTOR) as u64;
        let fits = sector
            .checked_add(sectors)
            .is_some_and(|e| e <= self.capacity);
        if !len.is_multiple_of(SECTOR) || !fits {
            return Err(Error::BadRequest);
        }
        Ok(())
    }

    /// One request of `len` bytes from the data buffer: the chain, the
    /// notification, the wait for the used ring, the status.
    fn request(
        &mut self,
        bus: &mut impl Bus,
        kind: u32,
        sector: u64,
        len: usize,
    ) -> Result<(), Error> {
        if self.stuck {
            return Err(Error::Timeout);
        }
        let pa = self.region.pa;
        let mut header = [0u8; 16];
        header[..4].copy_from_slice(&kind.to_le_bytes());
        header[8..].copy_from_slice(&sector.to_le_bytes());
        bus.mem_write(pa + HEADER, &header);
        bus.mem_write(pa + STATUS, &[0xFF]);
        let data_flags = if kind == T_IN { DESC_WRITE } else { 0 };
        for (i, (addr, n, flags)) in [
            (pa + HEADER, 16, DESC_NEXT),
            (pa + DATA, len as u32, data_flags | DESC_NEXT),
            (pa + STATUS, 1, DESC_WRITE),
        ]
        .into_iter()
        .enumerate()
        {
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&addr.to_le_bytes());
            d[8..12].copy_from_slice(&n.to_le_bytes());
            d[12..14].copy_from_slice(&flags.to_le_bytes());
            d[14..].copy_from_slice(&(i as u16 + 1).to_le_bytes());
            bus.mem_write(pa + DESC + 16 * i as u64, &d);
        }
        let slot = u64::from(self.avail_idx % self.size);
        bus.mem_write(pa + AVAIL + 4 + 2 * slot, &0u16.to_le_bytes());
        bus.fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        bus.mem_write(pa + AVAIL + 2, &self.avail_idx.to_le_bytes());
        bus.fence();
        bus.mmio_write(self.notify, 2, 0);
        let mut idx = [0u8; 2];
        let mut spins = self.spins;
        loop {
            bus.mem_read(pa + USED + 2, &mut idx);
            if u16::from_le_bytes(idx) != self.used_idx {
                break;
            }
            if spins == 0 {
                self.stuck = true;
                return Err(Error::Timeout);
            }
            spins -= 1;
        }
        self.used_idx = self.used_idx.wrapping_add(1);
        let mut status = [0u8];
        bus.mem_read(pa + STATUS, &mut status);
        match status[0] {
            0 => Ok(()),
            s => Err(Error::Device(s)),
        }
    }

    /// The device configuration's address (for a caller that wants more of it).
    pub fn device_config(&self) -> u64 {
        self.device
    }

    /// The common configuration's address.
    pub fn common_config(&self) -> u64 {
        self.common
    }
}
