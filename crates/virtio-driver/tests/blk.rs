//! The driver against the VMM's own virtio-blk model (vmm-devices) on the
//! platform bus: the device runs when the driver notifies it.

use virtio_driver::*;
use vmm_devices::machine::{slot, Machine};
use vmm_devices::virtio::GuestMemory;
use vmm_devices::virtio_blk::{BlockBackend, SECTOR as DEV_SECTOR};

const BAR: u64 = 0xC000_0000;
const REGION: DmaRegion = DmaRegion {
    pa: 0x10_0000,
    len: DMA_BYTES,
};

struct Ram(Vec<u8>);

impl GuestMemory for Ram {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        let at = gpa as usize;
        match self.0.get(at..at + buf.len()) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        let at = gpa as usize;
        match self.0.get_mut(at..at + data.len()) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

/// A disk of `n` sectors whose bytes say where they are.
struct Disk {
    data: Vec<u8>,
    reads: u64,
}

impl Disk {
    fn new(sectors: usize) -> Self {
        let data = (0..sectors * 512)
            .map(|i| (i / 512 * 7 + i % 251) as u8)
            .collect();
        Self { data, reads: 0 }
    }
}

impl BlockBackend for Disk {
    fn sectors(&self) -> u64 {
        (self.data.len() / 512) as u64
    }
    fn read(&mut self, sector: u64, buf: &mut [u8; DEV_SECTOR]) -> bool {
        self.reads += 1;
        let at = sector as usize * 512;
        buf.copy_from_slice(&self.data[at..at + 512]);
        true
    }
    fn write(&mut self, sector: u64, data: &[u8; DEV_SECTOR]) -> bool {
        let at = sector as usize * 512;
        self.data[at..at + 512].copy_from_slice(data);
        true
    }
    fn flush(&mut self) -> bool {
        true
    }
}

/// The machine with the disk at slot `dev`, its BAR placed as firmware would.
struct Bench {
    m: Machine,
    ram: Ram,
    disk: Disk,
    dev: u8,
    /// Serve the device when notified (false: a device that never answers).
    serve: bool,
    notified: u32,
}

impl Bench {
    fn new(dev: u8, sectors: usize) -> Self {
        let mut b = Self {
            m: Machine::new(0, 100_000_000),
            ram: Ram(vec![0; 0x20_0000]),
            disk: Disk::new(sectors),
            dev,
            serve: true,
            notified: 0,
        };
        b.m.blk.set_capacity(b.disk.sectors());
        b.cfg_write32(0x10, BAR as u32);
        b
    }

    fn address(&self, off: u16) -> u32 {
        (1 << 31) | (u32::from(self.dev) << 11) | u32::from(off & !3)
    }
}

impl Bus for Bench {
    fn cfg_read32(&mut self, off: u16) -> u32 {
        let a = self.address(off);
        self.m.io_out(0xCF8, 4, a, 0);
        self.m.io_in(0xCFC, 4, 0)
    }
    fn cfg_write32(&mut self, off: u16, v: u32) {
        let a = self.address(off);
        self.m.io_out(0xCF8, 4, a, 0);
        self.m.io_out(0xCFC, 4, v, 0);
    }
    fn mmio_read(&mut self, addr: u64, size: u8) -> u32 {
        self.m.mmio_read(addr, size, 0) as u32
    }
    fn mmio_write(&mut self, addr: u64, size: u8, v: u32) {
        self.m.mmio_write(addr, size, u64::from(v), 0);
        if let Some((d, off)) = self.m.virtio_hit(addr) {
            if d == slot::BLK && off >= vmm_devices::virtio::NOTIFY {
                self.notified += 1;
                if self.serve {
                    self.m.service_blk(&mut self.ram, &mut self.disk, 0);
                }
            }
        }
    }
    fn mem_read(&mut self, pa: u64, buf: &mut [u8]) {
        assert!(self.ram.read(pa, buf), "DMA outside the region");
    }
    fn mem_write(&mut self, pa: u64, data: &[u8]) {
        assert!(self.ram.write(pa, data), "DMA outside the region");
    }
}

fn open(sectors: usize) -> (Bench, Blk) {
    let mut b = Bench::new(slot::BLK, sectors);
    let blk = Blk::open(&mut b, REGION).expect("open");
    (b, blk)
}

#[test]
fn open_negotiates_and_sets_up_queue_0() {
    let (mut b, blk) = open(100);
    assert_eq!(blk.capacity, 100);
    assert!(!blk.read_only);
    // ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK
    assert_eq!(b.mmio_read(blk.common_config() + 0x14, 1), 0xF);
    // the driver accepted VERSION_1 only
    b.mmio_write(blk.common_config() + 0x08, 4, 1);
    assert_eq!(b.mmio_read(blk.common_config() + 0x0C, 4), 1);
    b.mmio_write(blk.common_config() + 0x08, 4, 0);
    assert_eq!(b.mmio_read(blk.common_config() + 0x0C, 4), 0);
    // queue 0: the driver's size and rings, enabled
    b.mmio_write(blk.common_config() + 0x16, 2, 0);
    assert_eq!(b.mmio_read(blk.common_config() + 0x18, 2), u32::from(QUEUE));
    assert_eq!(b.mmio_read(blk.common_config() + 0x1C, 2), 1);
    assert_eq!(b.mmio_read(blk.common_config() + 0x20, 4) as u64, REGION.pa);
    // memory space and bus mastering on
    assert_eq!(b.cfg_read32(0x04) & 0x6, 0x6);
    assert_eq!(blk.device_config(), BAR + 0x2000);
    assert_eq!(b.notified, 0, "nothing asked yet");
}

#[test]
fn reads_return_the_disks_sectors() {
    let (mut b, mut blk) = open(400);
    let mut one = [0u8; 512];
    blk.read(&mut b, 7, &mut one).unwrap();
    assert_eq!(&one[..], &b.disk.data[7 * 512..8 * 512]);
    // longer than one request: split at MAX_IO
    let mut big = vec![0u8; MAX_IO * 2 + 3 * 512];
    blk.read(&mut b, 10, &mut big).unwrap();
    assert_eq!(&big[..], &b.disk.data[10 * 512..10 * 512 + big.len()]);
    assert_eq!(b.notified, 1 + 3);
    // the last sector, and nothing
    blk.read(&mut b, 399, &mut one).unwrap();
    assert_eq!(&one[..], &b.disk.data[399 * 512..]);
    blk.read(&mut b, 400, &mut []).unwrap();
    // many requests wrap the rings' indices around the queue
    for i in 0..40 {
        blk.read(&mut b, i, &mut one).unwrap();
        assert_eq!(one[1], b.disk.data[i as usize * 512 + 1]);
    }
}

#[test]
fn writes_reach_the_disk() {
    let (mut b, mut blk) = open(300);
    let data: Vec<u8> = (0..MAX_IO + 512).map(|i| (i * 3) as u8).collect();
    blk.write(&mut b, 20, &data).unwrap();
    assert_eq!(&b.disk.data[20 * 512..20 * 512 + data.len()], &data[..]);
    let mut back = vec![0u8; data.len()];
    blk.read(&mut b, 20, &mut back).unwrap();
    assert_eq!(back, data);
}

#[test]
fn bad_requests_are_refused_before_the_device_sees_them() {
    let (mut b, mut blk) = open(10);
    let mut buf = [0u8; 513];
    assert_eq!(
        blk.read(&mut b, 0, &mut buf),
        Err(Error::BadRequest),
        "not whole sectors"
    );
    let mut two = [0u8; 1024];
    assert_eq!(
        blk.read(&mut b, 9, &mut two),
        Err(Error::BadRequest),
        "past the end"
    );
    assert_eq!(
        blk.read(&mut b, u64::MAX, &mut two),
        Err(Error::BadRequest),
        "overflow"
    );
    assert_eq!(blk.write(&mut b, 10, &two[..512]), Err(Error::BadRequest));
    assert_eq!(b.notified, 0);
}

#[test]
fn a_device_error_is_reported() {
    let (mut b, mut blk) = open(10);
    // The driver thinks the disk is larger than the device does.
    blk.capacity = 20;
    let mut one = [0u8; 512];
    assert_eq!(blk.read(&mut b, 15, &mut one), Err(Error::Device(1)));
    // and the queue still works afterwards
    blk.read(&mut b, 3, &mut one).unwrap();
    assert_eq!(&one[..], &b.disk.data[3 * 512..4 * 512]);
}

#[test]
fn a_device_that_never_answers_times_out() {
    let (mut b, mut blk) = open(10);
    b.serve = false;
    blk.spins = 100;
    let mut one = [0u8; 512];
    assert_eq!(blk.read(&mut b, 0, &mut one), Err(Error::Timeout));
    assert_eq!(b.notified, 1);
    // The device answers late: its completion is not taken for the next
    // request's; the queue stays refused until the device is opened again.
    b.serve = true;
    b.m.service_blk(&mut b.ram, &mut b.disk, 0);
    assert_eq!(blk.read(&mut b, 1, &mut one), Err(Error::Timeout));
    assert_eq!(b.notified, 1, "not even sent");
    let mut blk = Blk::open(&mut b, REGION).unwrap();
    blk.read(&mut b, 1, &mut one).unwrap();
    assert_eq!(&one[..], &b.disk.data[512..1024]);
}

#[test]
fn opening_again_resets_what_another_driver_left() {
    let (mut b, mut first) = open(50);
    let mut one = [0u8; 512];
    first.read(&mut b, 1, &mut one).unwrap();
    // A second driver (the probe after the firmware's) on the same device.
    let mut blk = Blk::open(&mut b, REGION).expect("reopen");
    blk.read(&mut b, 2, &mut one).unwrap();
    assert_eq!(&one[..], &b.disk.data[2 * 512..3 * 512]);
}

#[test]
fn what_is_not_a_usable_virtio_blk_is_refused() {
    // the network card's slot, and an empty one
    let mut b = Bench::new(slot::NET, 1);
    assert_eq!(Blk::open(&mut b, REGION).err(), Some(Error::NotVirtioBlk));
    let mut b = Bench::new(20, 1);
    assert_eq!(Blk::open(&mut b, REGION).err(), Some(Error::NotVirtioBlk));
    // a region too small
    let mut b = Bench::new(slot::BLK, 1);
    let small = DmaRegion {
        len: DMA_BYTES - 1,
        ..REGION
    };
    assert_eq!(Blk::open(&mut b, small).err(), Some(Error::RegionTooSmall));
}
