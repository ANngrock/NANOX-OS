//! The probe machine's own disk for the Linux guest: a `virtio-blk-pci` of
//! QEMU (run.py attaches large images that way rather than through fw_cfg,
//! which would hold them in memory twice), driven by `virtio-driver` and
//! served to the guest's virtio-blk as [`HostDisk`]. The guest's device asks
//! for one sector at a time; a few [`BLOCK`]-sized blocks are kept, so a
//! sequential read becomes one request per block.

use crate::hw;
use virtio_driver::{Blk, Bus, DmaRegion, Error, DMA_BYTES};
use vmm_devices::virtio_blk::{BlockBackend, SECTOR};

/// The unit the host disk is read in.
pub const BLOCK: usize = 64 << 10;
/// Blocks kept (oldest replaced first).
const CACHED: usize = 8;

/// PCI configuration (ports 0xCF8/0xCFC) of one function on bus 0, its
/// memory-mapped registers and the DMA region, all identity-mapped.
struct ProbeBus {
    device: u8,
}

impl ProbeBus {
    fn select(&self, off: u16) {
        hw::outl(
            0xCF8,
            (1 << 31) | u32::from(self.device) << 11 | u32::from(off & !3),
        );
    }
}

impl Bus for ProbeBus {
    fn cfg_read32(&mut self, off: u16) -> u32 {
        self.select(off);
        hw::inl(0xCFC)
    }

    fn cfg_write32(&mut self, off: u16, v: u32) {
        self.select(off);
        hw::outl(0xCFC, v);
    }

    fn mmio_read(&mut self, addr: u64, size: u8) -> u32 {
        // SAFETY: `addr` is a register of the device's BAR as the firmware
        // placed it (read from its configuration space), identity-mapped
        // uncached by the firmware; volatile, naturally aligned accesses.
        unsafe {
            match size {
                1 => u32::from((addr as *const u8).read_volatile()),
                2 => u32::from((addr as *const u16).read_volatile()),
                _ => (addr as *const u32).read_volatile(),
            }
        }
    }

    fn mmio_write(&mut self, addr: u64, size: u8, v: u32) {
        // SAFETY: as in `mmio_read`.
        unsafe {
            match size {
                1 => (addr as *mut u8).write_volatile(v as u8),
                2 => (addr as *mut u16).write_volatile(v as u16),
                _ => (addr as *mut u32).write_volatile(v),
            }
        }
    }

    fn mem_read(&mut self, pa: u64, buf: &mut [u8]) {
        // Volatile: the device writes this memory behind the compiler's back
        // (the driver polls the used ring through here).
        for (i, b) in buf.iter_mut().enumerate() {
            // SAFETY: inside the DMA region the probe allocated for the
            // driver (identity-mapped firmware memory); a byte has no alignment.
            *b = unsafe { ((pa + i as u64) as *const u8).read_volatile() };
        }
    }

    fn mem_write(&mut self, pa: u64, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            // SAFETY: as in `mem_read`.
            unsafe { ((pa + i as u64) as *mut u8).write_volatile(b) };
        }
    }

    fn fence(&mut self) {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
}

/// The disk, with the last blocks read.
pub struct HostDisk {
    bus: ProbeBus,
    blk: Blk,
    blocks: &'static mut [[u8; BLOCK]; CACHED],
    /// Which block each cache slot holds.
    tags: [Option<u64>; CACHED],
    next: usize,
    pub requests: u64,
    pub errors: u64,
    pub last_error: Option<Error>,
}

impl HostDisk {
    /// The first virtio-blk on bus 0, opened with DMA memory and a cache
    /// from the firmware (`allocate`); None if there is none or it fails.
    pub fn open(allocate: impl Fn(usize) -> Option<&'static mut [u8]>) -> Option<Self> {
        let device = (0..32u8).find(|&d| {
            let mut bus = ProbeBus { device: d };
            let id = bus.cfg_read32(0);
            id & 0xFFFF == u32::from(virtio_driver::VENDOR)
                && matches!(
                    (id >> 16) as u16,
                    virtio_driver::DEVICE_MODERN | virtio_driver::DEVICE_TRANSITIONAL
                )
        })?;
        let region = allocate(DMA_BYTES as usize)?;
        let region = DmaRegion {
            pa: region.as_mut_ptr() as u64,
            len: region.len() as u64,
        };
        let mut bus = ProbeBus { device };
        let blk = Blk::open(&mut bus, region).ok()?;
        let cache = allocate(BLOCK * CACHED)?;
        // SAFETY: fresh page-aligned firmware memory of `BLOCK * CACHED`
        // bytes, owned by the probe; the byte slice is consumed here.
        let blocks = unsafe { &mut *(cache.as_mut_ptr() as *mut [[u8; BLOCK]; CACHED]) };
        Some(Self {
            bus,
            blk,
            blocks,
            tags: [None; CACHED],
            next: 0,
            requests: 0,
            errors: 0,
            last_error: None,
        })
    }

    pub fn read_only(&self) -> bool {
        self.blk.read_only
    }

    /// The cache slot holding block `b`, reading it first if needed.
    fn block(&mut self, b: u64) -> Option<usize> {
        if let Some(i) = self.tags.iter().position(|&t| t == Some(b)) {
            return Some(i);
        }
        let per = (BLOCK / SECTOR) as u64;
        let first = b * per;
        let sectors = per.min(self.blk.capacity.checked_sub(first)?);
        let i = self.next;
        self.next = (self.next + 1) % CACHED;
        self.tags[i] = None;
        self.requests += 1;
        let len = sectors as usize * SECTOR;
        if let Err(e) = self
            .blk
            .read(&mut self.bus, first, &mut self.blocks[i][..len])
        {
            self.errors += 1;
            self.last_error = Some(e);
            return None;
        }
        self.tags[i] = Some(b);
        Some(i)
    }
}

impl BlockBackend for HostDisk {
    fn sectors(&self) -> u64 {
        self.blk.capacity
    }

    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        let per = (BLOCK / SECTOR) as u64;
        let Some(i) = self.block(sector / per) else {
            return false;
        };
        let at = (sector % per) as usize * SECTOR;
        buf.copy_from_slice(&self.blocks[i][at..at + SECTOR]);
        true
    }

    fn write(&mut self, sector: u64, data: &[u8; SECTOR]) -> bool {
        let r = match self.blk.read_only {
            true => Err(Error::Device(1)),
            false => self.blk.write(&mut self.bus, sector, data),
        };
        if let Err(e) = r {
            self.errors += 1;
            self.last_error = Some(e);
            return false;
        }
        // Keep a cached copy of the block right.
        let per = (BLOCK / SECTOR) as u64;
        if let Some(i) = self.tags.iter().position(|&t| t == Some(sector / per)) {
            let at = (sector % per) as usize * SECTOR;
            self.blocks[i][at..at + SECTOR].copy_from_slice(data);
        }
        true
    }

    fn flush(&mut self) -> bool {
        true
    }
}
