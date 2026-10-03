//! virtio-console (device type 3), one port, as the agent channel between a
//! guest service and the host: a byte stream in each direction. Queue 0
//! receives (host to guest), queue 1 transmits (guest to host). No features
//! beyond VERSION_1 are offered: no console size, no multiport (so no control
//! queues), no emergency write; the device configuration space is empty.
//!
//! The stream has no message boundaries: whatever the guest put in a transmit
//! chain goes to [`ConsoleBackend::write`] in order, and receive buffers are
//! filled from [`ConsoleBackend::read`] as far as the host has data.

use crate::virtio::{GuestMemory, VirtioPci};

pub const DEVICE_TYPE: u16 = 3;
/// Communication controller, other.
pub const CLASS: u32 = 0x07_8000;
const RX: usize = 0;
const TX: usize = 1;
/// Bytes moved between guest memory and the backend per step.
const CHUNK: usize = 256;

/// The host's end of the channel.
pub trait ConsoleBackend {
    /// Bytes the guest sent, in order.
    fn write(&mut self, data: &[u8]);
    /// Copies up to `buf.len()` bytes the host has for the guest into `buf`; returns how many (0: none).
    fn read(&mut self, buf: &mut [u8]) -> usize;
}

#[derive(Clone, Debug)]
pub struct VirtioConsole {
    pub t: VirtioPci,
    pub bytes_out: u64,
    pub bytes_in: u64,
    /// Transmit chains with a device-writable buffer or memory that could not be read.
    pub tx_errors: u64,
    /// Receive chains with a buffer the device may not write, or no room at all.
    pub rx_bad: u64,
    /// Bytes taken from the backend that could not be stored in guest memory.
    pub rx_lost: u64,
}

impl VirtioConsole {
    pub fn new(line: u8) -> Self {
        Self {
            t: VirtioPci::new(DEVICE_TYPE, CLASS, 2, 0, line),
            bytes_out: 0,
            bytes_in: 0,
            tx_errors: 0,
            rx_bad: 0,
            rx_lost: 0,
        }
    }

    /// Passes on what the guest transmitted and delivers what the host has; returns (chains sent, chains filled).
    pub fn service(
        &mut self,
        mem: &mut dyn GuestMemory,
        be: &mut dyn ConsoleBackend,
    ) -> (u32, u32) {
        self.t.take_kicks();
        if !self.t.dma_allowed() {
            return (0, 0);
        }
        (self.transmit(mem, be), self.receive(mem, be))
    }

    fn transmit(&mut self, mem: &mut dyn GuestMemory, be: &mut dyn ConsoleBackend) -> u32 {
        let mut n = 0;
        let mut buf = [0u8; CHUNK];
        while let Some(chain) = self.t.pop(mem, TX) {
            'chain: for d in chain.descs() {
                if d.write {
                    self.tx_errors += 1;
                    break;
                }
                let mut off = 0u64;
                while off < u64::from(d.len) {
                    let take = (u64::from(d.len) - off).min(CHUNK as u64) as usize;
                    let Some(at) = d.addr.checked_add(off) else {
                        self.tx_errors += 1;
                        break 'chain;
                    };
                    if !mem.read(at, &mut buf[..take]) {
                        self.tx_errors += 1;
                        break 'chain;
                    }
                    be.write(&buf[..take]);
                    self.bytes_out += take as u64;
                    off += take as u64;
                }
            }
            self.t.push_used(mem, TX, chain.head, 0);
            n += 1;
        }
        n
    }

    fn receive(&mut self, mem: &mut dyn GuestMemory, be: &mut dyn ConsoleBackend) -> u32 {
        let mut n = 0;
        let mut buf = [0u8; CHUNK];
        // A buffer is only taken when there may be something to put in it.
        while let Some(chain) = self.t.pop(mem, RX) {
            if chain.descs().iter().any(|d| !d.write) || chain.descs().iter().all(|d| d.len == 0) {
                self.rx_bad += 1;
                self.t.push_used(mem, RX, chain.head, 0);
                n += 1;
                continue;
            }
            let mut written = 0u32;
            let mut stored = true;
            'fill: for d in chain.descs() {
                let mut off = 0u32;
                while off < d.len {
                    let want = ((d.len - off) as usize).min(CHUNK);
                    let got = be.read(&mut buf[..want]).min(want);
                    if got == 0 {
                        break 'fill;
                    }
                    let ok = d
                        .addr
                        .checked_add(u64::from(off))
                        .is_some_and(|at| mem.write(at, &buf[..got]));
                    if !ok {
                        self.rx_lost += got as u64;
                        stored = false;
                        break 'fill;
                    }
                    off += got as u32;
                    written += got as u32;
                }
            }
            if written == 0 && stored {
                self.t.unpop(RX);
                break;
            }
            self.bytes_in += u64::from(written);
            self.t.push_used(mem, RX, chain.head, written);
            n += 1;
        }
        n
    }
}
