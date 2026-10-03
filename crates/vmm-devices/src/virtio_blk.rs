//! virtio-blk (device type 2): a block device behind a [`BlockBackend`].
//! One request queue; a request is a chain of a 16-byte header (type, sector),
//! data buffers and a final one-byte status the device writes. Supported
//! request types: read (0), write (1), flush (4) and get-id (8); anything else
//! gets UNSUPP. Features offered: VERSION_1 and FLUSH. The capacity is in the
//! device configuration, in 512-byte sectors.

use crate::virtio::{Chain, Desc, GuestMemory, VirtioPci};

pub const DEVICE_TYPE: u16 = 2;
/// Mass storage controller, other.
pub const CLASS: u32 = 0x01_8000;
pub const F_FLUSH: u64 = 1 << 9;
pub const SECTOR: usize = 512;
pub const STATUS_OK: u8 = 0;
pub const STATUS_IOERR: u8 = 1;
pub const STATUS_UNSUPPORTED: u8 = 2;
const REQ_IN: u32 = 0;
const REQ_OUT: u32 = 1;
const REQ_FLUSH: u32 = 4;
const REQ_GET_ID: u32 = 8;
const ID: &[u8; 20] = b"nanox-virtio-blk\0\0\0\0";

/// What the device reads and writes: sectors of 512 bytes.
pub trait BlockBackend {
    fn sectors(&self) -> u64;
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool;
    fn write(&mut self, sector: u64, data: &[u8; SECTOR]) -> bool;
    fn flush(&mut self) -> bool;
}

#[derive(Clone, Debug)]
pub struct VirtioBlk {
    pub t: VirtioPci,
    pub requests: u64,
    pub failed: u64,
}

impl VirtioBlk {
    pub fn new(sectors: u64, line: u8) -> Self {
        let mut t = VirtioPci::new(DEVICE_TYPE, CLASS, 1, F_FLUSH, line);
        t.set_device_config(0, &sectors.to_le_bytes());
        Self {
            t,
            requests: 0,
            failed: 0,
        }
    }

    /// Sets the capacity the guest reads from the device configuration.
    pub fn set_capacity(&mut self, sectors: u64) {
        self.t.set_device_config(0, &sectors.to_le_bytes());
    }

    /// Serves every request the driver made available; returns how many it completed.
    pub fn service(&mut self, mem: &mut dyn GuestMemory, be: &mut dyn BlockBackend) -> u32 {
        self.t.take_kicks();
        if !self.t.dma_allowed() {
            return 0;
        }
        let mut done = 0;
        while let Some(chain) = self.t.pop(mem, 0) {
            let (status, written) = self.request(mem, be, &chain);
            self.requests += 1;
            if status != STATUS_OK {
                self.failed += 1;
            }
            self.t.push_used(mem, 0, chain.head, written);
            done += 1;
        }
        done
    }

    /// Executes one request; returns the status byte it wrote and how many bytes it wrote to the guest.
    fn request(
        &mut self,
        mem: &mut dyn GuestMemory,
        be: &mut dyn BlockBackend,
        chain: &Chain,
    ) -> (u8, u32) {
        let d = chain.descs();
        // header, at least one more buffer (the status), the status last and writable
        let (Some(h), Some(st)) = (d.first(), d.last()) else {
            return (STATUS_IOERR, 0);
        };
        if d.len() < 2 || h.write || h.len < 16 || !st.write || st.len < 1 {
            // Without a usable status byte nothing can be reported: complete the chain empty.
            return (STATUS_IOERR, 0);
        }
        let mut hdr = [0u8; 16];
        if !mem.read(h.addr, &mut hdr) {
            return (STATUS_IOERR, 0);
        }
        let kind = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let sector = u64::from_le_bytes([
            hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
        ]);
        let data = &d[1..d.len() - 1];
        let (status, written) = match kind {
            REQ_IN => self.transfer(mem, be, sector, data, false),
            REQ_OUT => self.transfer(mem, be, sector, data, true),
            REQ_FLUSH => (if be.flush() { STATUS_OK } else { STATUS_IOERR }, 0),
            REQ_GET_ID => match data.first() {
                Some(b) if b.write && b.len >= 20 && mem.write(b.addr, ID) => (STATUS_OK, 20),
                _ => (STATUS_IOERR, 0),
            },
            _ => (STATUS_UNSUPPORTED, 0),
        };
        let ok = mem.write(st.addr, &[status]);
        (status, written + u32::from(ok))
    }

    /// Reads (`write == false`: into the guest) or writes (from the guest) the data buffers sector by sector.
    fn transfer(
        &mut self,
        mem: &mut dyn GuestMemory,
        be: &mut dyn BlockBackend,
        first: u64,
        data: &[Desc],
        write: bool,
    ) -> (u8, u32) {
        let total: u64 = data.iter().map(|b| u64::from(b.len)).sum();
        // Direction must match, whole sectors only, inside the device.
        if data.iter().any(|b| b.write == write)
            || data
                .iter()
                .any(|b| !(b.len as usize).is_multiple_of(SECTOR))
        {
            return (STATUS_IOERR, 0);
        }
        let sectors = total / SECTOR as u64;
        if first
            .checked_add(sectors)
            .is_none_or(|end| end > be.sectors())
        {
            return (STATUS_IOERR, 0);
        }
        let mut sector = first;
        let mut written = 0u32;
        let mut buf = [0u8; SECTOR];
        for b in data {
            for k in 0..(b.len as usize / SECTOR) {
                let at = b.addr + (k * SECTOR) as u64;
                if write {
                    if !mem.read(at, &mut buf) || !be.write(sector, &buf) {
                        return (STATUS_IOERR, written);
                    }
                } else {
                    if !be.read(sector, &mut buf) || !mem.write(at, &buf) {
                        return (STATUS_IOERR, written);
                    }
                    written += SECTOR as u32;
                }
                sector += 1;
            }
        }
        (STATUS_OK, written)
    }
}
