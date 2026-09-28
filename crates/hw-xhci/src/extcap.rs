//! Extended capabilities (xHCI §7).
//!
//! The list starts at HCCPARAMS1.xECP (in dwords from the MMIO base); each
//! header holds the capability ID (bits 7:0) and the offset of the next
//! capability in dwords relative to the current one (bits 15:8, 0 = end).
//! Because the offset is a forward delta a list cannot loop, but a
//! malformed one can run past the MMIO window or be arbitrarily long; both
//! are bounded here.

use crate::{Error, Mmio};

/// USB Legacy Support capability ID (xHCI §7.1).
pub const ID_LEGACY: u8 = 1;
/// Supported Protocol capability ID (xHCI §7.2).
pub const ID_PROTOCOL: u8 = 2;
/// Upper bound on the number of extended capabilities walked.
pub const MAX_EXT_CAPS: usize = 64;
/// Upper bound on Supported Protocol capabilities kept.
pub const MAX_PROTOCOLS: usize = 16;

/// USB Legacy Support registers (xHCI §7.1.1, §7.1.2).
pub mod legacy {
    /// USBLEGSUP: HC BIOS Owned Semaphore.
    pub const BIOS_OWNED: u32 = 1 << 16;
    /// USBLEGSUP: HC OS Owned Semaphore.
    pub const OS_OWNED: u32 = 1 << 24;
    /// Offset of USBLEGCTLSTS from the capability.
    pub const CTLSTS: u32 = 4;
    /// USBLEGCTLSTS SMI enable bits: USB SMI (0), SMI on Host System
    /// Error (4), SMI on OS Ownership (13), SMI on PCI Command (14),
    /// SMI on BAR (15).
    pub const SMI_ENABLES: u32 = 1 | 1 << 4 | 1 << 13 | 1 << 14 | 1 << 15;
    /// USBLEGCTLSTS RW1C SMI event bits 29..31.
    pub const SMI_EVENTS: u32 = 0b111 << 29;
}

/// Name string of a USB Supported Protocol capability: "USB ".
pub const PROTOCOL_NAME_USB: u32 = 0x2042_5355;

/// One extended capability header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtCap {
    /// Byte offset of the capability in the MMIO window.
    pub offset: u32,
    /// Capability ID.
    pub id: u8,
    /// Raw first dword.
    pub header: u32,
}

/// The extended capability list.
#[derive(Clone, Debug)]
pub struct ExtCaps {
    caps: [ExtCap; MAX_EXT_CAPS],
    len: usize,
}

impl ExtCaps {
    /// Walks the list starting at byte offset `xecp` (0 = no list).
    pub fn read<M: Mmio + ?Sized>(mmio: &mut M, xecp: u32, mmio_len: u32) -> Result<Self, Error> {
        let mut out = Self {
            caps: [ExtCap::default(); MAX_EXT_CAPS],
            len: 0,
        };
        if xecp == 0 {
            return Ok(out);
        }
        let mut off = u64::from(xecp);
        loop {
            if off % 4 != 0 || off + 4 > u64::from(mmio_len) {
                return Err(Error::BadExtCap("capability outside MMIO window"));
            }
            if out.len == MAX_EXT_CAPS {
                return Err(Error::BadExtCap("too many extended capabilities"));
            }
            let header = mmio.read32(off as u32);
            if header == u32::MAX {
                return Err(Error::ControllerGone);
            }
            let id = header as u8;
            if id == 0 {
                return Err(Error::BadExtCap("reserved capability ID 0"));
            }
            out.caps[out.len] = ExtCap {
                offset: off as u32,
                id,
                header,
            };
            out.len += 1;
            let next = (header >> 8) & 0xFF;
            if next == 0 {
                return Ok(out);
            }
            off += u64::from(next) * 4;
        }
    }

    /// All capabilities in list order.
    pub fn iter(&self) -> impl Iterator<Item = &ExtCap> {
        self.caps[..self.len].iter()
    }

    /// First capability with `id`.
    pub fn find(&self, id: u8) -> Option<ExtCap> {
        self.iter().find(|c| c.id == id).copied()
    }

    /// Number of capabilities.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A USB Supported Protocol capability (xHCI §7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupportedProtocol {
    /// Byte offset of the capability.
    pub offset: u32,
    /// Major revision (BCD): 0x02 = USB2, 0x03 = USB3.
    pub major: u8,
    /// Minor revision (BCD).
    pub minor: u8,
    /// First root hub port (1-based).
    pub first_port: u8,
    /// Number of consecutive ports.
    pub port_count: u8,
    /// Protocol Speed ID Count.
    pub psic: u8,
    /// Protocol Defined field (bits 27:16 of dword 2).
    pub protocol_defined: u16,
    /// Protocol Slot Type for Enable Slot.
    pub slot_type: u8,
}

impl SupportedProtocol {
    /// True when `port` belongs to this protocol.
    pub fn contains(&self, port: u8) -> bool {
        port >= self.first_port
            && u16::from(port) < u16::from(self.first_port) + u16::from(self.port_count)
    }

    /// True for USB 3.x ports.
    pub fn is_usb3(&self) -> bool {
        self.major == 3
    }
}

/// All Supported Protocol capabilities.
#[derive(Clone, Debug)]
pub struct Protocols {
    entries: [Option<SupportedProtocol>; MAX_PROTOCOLS],
}

impl Protocols {
    /// No protocol information.
    pub const fn empty() -> Self {
        Self {
            entries: [None; MAX_PROTOCOLS],
        }
    }

    /// Reads every Supported Protocol capability in `caps`, checking the
    /// name string, the port ranges against `max_ports` and that ranges do
    /// not overlap.
    pub fn read<M: Mmio + ?Sized>(
        mmio: &mut M,
        caps: &ExtCaps,
        max_ports: u8,
        mmio_len: u32,
    ) -> Result<Self, Error> {
        let mut out = Self::empty();
        for (n, cap) in caps.iter().filter(|c| c.id == ID_PROTOCOL).enumerate() {
            let off = cap.offset;
            if u64::from(off) + 16 > u64::from(mmio_len) {
                return Err(Error::BadExtCap("protocol capability outside window"));
            }
            let d0 = cap.header;
            let d1 = mmio.read32(off + 4);
            let d2 = mmio.read32(off + 8);
            let d3 = mmio.read32(off + 12);
            if d1 == u32::MAX && d2 == u32::MAX {
                return Err(Error::ControllerGone);
            }
            if d1 != PROTOCOL_NAME_USB {
                return Err(Error::BadExtCap("protocol name string is not \"USB \""));
            }
            let p = SupportedProtocol {
                offset: off,
                major: (d0 >> 24) as u8,
                minor: (d0 >> 16) as u8,
                first_port: d2 as u8,
                port_count: (d2 >> 8) as u8,
                psic: (d2 >> 28) as u8,
                protocol_defined: ((d2 >> 16) & 0xFFF) as u16,
                slot_type: (d3 & 0x1F) as u8,
            };
            if p.first_port == 0
                || p.port_count == 0
                || u16::from(p.first_port) + u16::from(p.port_count) - 1 > u16::from(max_ports)
            {
                return Err(Error::BadExtCap("protocol port range"));
            }
            if u64::from(off) + 16 + 4 * u64::from(p.psic) > u64::from(mmio_len) {
                return Err(Error::BadExtCap("protocol speed IDs outside window"));
            }
            if out.iter().any(|q| {
                let (a0, a1) = (
                    u16::from(q.first_port),
                    u16::from(q.first_port) + u16::from(q.port_count),
                );
                let (b0, b1) = (
                    u16::from(p.first_port),
                    u16::from(p.first_port) + u16::from(p.port_count),
                );
                a0 < b1 && b0 < a1
            }) {
                return Err(Error::BadExtCap("overlapping protocol port ranges"));
            }
            if n == MAX_PROTOCOLS {
                return Err(Error::BadExtCap("too many protocol capabilities"));
            }
            out.entries[n] = Some(p);
        }
        Ok(out)
    }

    /// Protocol of `port`, if any capability covers it.
    pub fn for_port(&self, port: u8) -> Option<SupportedProtocol> {
        self.iter().find(|p| p.contains(port))
    }

    /// All protocols.
    pub fn iter(&self) -> impl Iterator<Item = SupportedProtocol> + '_ {
        self.entries.iter().filter_map(|e| *e)
    }
}
