//! virtio-net (device type 1): a network card behind a [`NetBackend`]. Queue 0
//! receives, queue 1 transmits; there is no control queue, no offload and one
//! queue pair. Features: VERSION_1, MAC and STATUS (link up). Every frame
//! travels behind a 12-byte `virtio_net_hdr_v1` that is all zeros here (no
//! checksum or segmentation offload was negotiated) with `num_buffers` = 1 on
//! the receive side.

use crate::virtio::{GuestMemory, VirtioPci};

pub const DEVICE_TYPE: u16 = 1;
/// Ethernet controller.
pub const CLASS: u32 = 0x02_0000;
pub const F_MAC: u64 = 1 << 5;
pub const F_STATUS: u64 = 1 << 16;
pub const HEADER: usize = 12;
/// The largest frame handled (headers and a 1514-byte Ethernet frame fit; jumbo frames do not).
pub const MAX_FRAME: usize = 2048;
const RX: usize = 0;
const TX: usize = 1;
const LINK_UP: u16 = 1;

/// The network on the other side of the card.
pub trait NetBackend {
    /// A frame the guest sent.
    fn send(&mut self, frame: &[u8]);
    /// The next frame for the guest, copied into `buf`; its length, or None if there is none.
    fn recv(&mut self, buf: &mut [u8; MAX_FRAME]) -> Option<usize>;
}

#[derive(Clone, Debug)]
pub struct VirtioNet {
    pub t: VirtioPci,
    pub mac: [u8; 6],
    pub sent: u64,
    pub received: u64,
    /// Frames the guest sent that did not fit the buffer, or whose buffers could not be read.
    pub tx_errors: u64,
    /// Frames that did not fit the guest's receive buffer.
    pub rx_truncated: u64,
}

impl VirtioNet {
    pub fn new(mac: [u8; 6], line: u8) -> Self {
        let mut t = VirtioPci::new(DEVICE_TYPE, CLASS, 2, F_MAC | F_STATUS, line);
        t.set_device_config(0, &mac);
        t.set_device_config(6, &LINK_UP.to_le_bytes());
        Self {
            t,
            mac,
            sent: 0,
            received: 0,
            tx_errors: 0,
            rx_truncated: 0,
        }
    }

    /// Takes what the guest transmitted and delivers what arrived; returns (frames sent, frames received).
    pub fn service(&mut self, mem: &mut dyn GuestMemory, be: &mut dyn NetBackend) -> (u32, u32) {
        self.t.take_kicks();
        if !self.t.dma_allowed() {
            return (0, 0);
        }
        (self.transmit(mem, be), self.receive(mem, be))
    }

    fn transmit(&mut self, mem: &mut dyn GuestMemory, be: &mut dyn NetBackend) -> u32 {
        let mut n = 0;
        let mut frame = [0u8; MAX_FRAME + HEADER];
        while let Some(chain) = self.t.pop(mem, TX) {
            let mut len = 0usize;
            let mut ok = true;
            for d in chain.descs() {
                let l = d.len as usize;
                if d.write || len + l > frame.len() || !mem.read(d.addr, &mut frame[len..len + l]) {
                    ok = false;
                    break;
                }
                len += l;
            }
            if ok && len >= HEADER {
                be.send(&frame[HEADER..len]);
                self.sent += 1;
            } else {
                self.tx_errors += 1;
            }
            self.t.push_used(mem, TX, chain.head, 0);
            n += 1;
        }
        n
    }

    fn receive(&mut self, mem: &mut dyn GuestMemory, be: &mut dyn NetBackend) -> u32 {
        let mut n = 0;
        let mut frame = [0u8; MAX_FRAME];
        // Only take a frame from the network when there is a buffer to put it in.
        while let Some(chain) = self.t.pop(mem, RX) {
            let Some(len) = be.recv(&mut frame) else {
                self.t.unpop(RX);
                break;
            };
            let mut packet = [0u8; HEADER + MAX_FRAME];
            packet[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers
            packet[HEADER..HEADER + len].copy_from_slice(&frame[..len]);
            let total = HEADER + len;
            let mut at = 0usize;
            for d in chain.descs() {
                if !d.write || at >= total {
                    break;
                }
                let take = (d.len as usize).min(total - at);
                if !mem.write(d.addr, &packet[at..at + take]) {
                    break;
                }
                at += take;
            }
            if at < total {
                self.rx_truncated += 1;
            }
            self.received += 1;
            self.t.push_used(mem, RX, chain.head, at as u32);
            n += 1;
        }
        n
    }
}
