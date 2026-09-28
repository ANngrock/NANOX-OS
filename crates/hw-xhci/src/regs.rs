//! Register layout (xHCI §5).
//!
//! The MMIO window starts with the capability registers (§5.3); the
//! operational registers follow at CAPLENGTH (§5.4), the runtime registers
//! are at RTSOFF (§5.5) and the doorbell array at DBOFF (§5.6).

use crate::{Error, Mmio};

/// Capability register offsets (xHCI §5.3, Table 5-9).
pub mod cap {
    /// CAPLENGTH (byte 0) and HCIVERSION (bytes 2..4).
    pub const CAPLENGTH: u32 = 0x00;
    /// Structural parameters 1.
    pub const HCSPARAMS1: u32 = 0x04;
    /// Structural parameters 2.
    pub const HCSPARAMS2: u32 = 0x08;
    /// Structural parameters 3.
    pub const HCSPARAMS3: u32 = 0x0C;
    /// Capability parameters 1.
    pub const HCCPARAMS1: u32 = 0x10;
    /// Doorbell offset.
    pub const DBOFF: u32 = 0x14;
    /// Runtime register space offset.
    pub const RTSOFF: u32 = 0x18;
    /// Capability parameters 2.
    pub const HCCPARAMS2: u32 = 0x1C;
}

/// Operational register offsets relative to CAPLENGTH (xHCI §5.4,
/// Table 5-18).
pub mod op {
    /// USB command.
    pub const USBCMD: u32 = 0x00;
    /// USB status.
    pub const USBSTS: u32 = 0x04;
    /// Supported page sizes (bit n = 2^(n+12) bytes).
    pub const PAGESIZE: u32 = 0x08;
    /// Device notification control.
    pub const DNCTRL: u32 = 0x14;
    /// Command ring control (64-bit).
    pub const CRCR: u32 = 0x18;
    /// Device context base address array pointer (64-bit).
    pub const DCBAAP: u32 = 0x30;
    /// Configure.
    pub const CONFIG: u32 = 0x38;
    /// First port register set (PORTSC of port 1).
    pub const PORT_BASE: u32 = 0x400;
    /// Size of one port register set.
    pub const PORT_STRIDE: u32 = 0x10;
}

/// USBCMD bits (xHCI §5.4.1).
pub mod usbcmd {
    /// Run/Stop.
    pub const RS: u32 = 1 << 0;
    /// Host Controller Reset.
    pub const HCRST: u32 = 1 << 1;
    /// Interrupter Enable.
    pub const INTE: u32 = 1 << 2;
    /// Host System Error Enable.
    pub const HSEE: u32 = 1 << 3;
}

/// USBSTS bits (xHCI §5.4.2).
pub mod usbsts {
    /// HC Halted (RO).
    pub const HCH: u32 = 1 << 0;
    /// Host System Error (RW1C).
    pub const HSE: u32 = 1 << 2;
    /// Event Interrupt (RW1C).
    pub const EINT: u32 = 1 << 3;
    /// Port Change Detect (RW1C).
    pub const PCD: u32 = 1 << 4;
    /// Save State Status (RO).
    pub const SSS: u32 = 1 << 8;
    /// Restore State Status (RO).
    pub const RSS: u32 = 1 << 9;
    /// Save/Restore Error (RW1C).
    pub const SRE: u32 = 1 << 10;
    /// Controller Not Ready (RO).
    pub const CNR: u32 = 1 << 11;
    /// Host Controller Error (RO).
    pub const HCE: u32 = 1 << 12;
    /// All RW1C bits.
    pub const RW1C: u32 = HSE | EINT | PCD | SRE;
}

/// CRCR bits (xHCI §5.4.5).
pub mod crcr {
    /// Ring Cycle State.
    pub const RCS: u64 = 1 << 0;
    /// Command Stop.
    pub const CS: u64 = 1 << 1;
    /// Command Abort.
    pub const CA: u64 = 1 << 2;
    /// Command Ring Running (RO).
    pub const CRR: u64 = 1 << 3;
    /// Command ring dequeue pointer bits (64-byte aligned).
    pub const PTR_MASK: u64 = !0x3F;
}

/// CONFIG bits (xHCI §5.4.7).
pub mod config {
    /// Max Device Slots Enabled.
    pub const MAX_SLOTS_EN_MASK: u32 = 0xFF;
}

/// Runtime register offsets relative to RTSOFF (xHCI §5.5).
pub mod rt {
    /// Microframe index.
    pub const MFINDEX: u32 = 0x00;
    /// Interrupter register set 0.
    pub const IR0: u32 = 0x20;
    /// Size of one interrupter register set.
    pub const IR_STRIDE: u32 = 0x20;
    /// Interrupter management.
    pub const IMAN: u32 = 0x00;
    /// Interrupter moderation.
    pub const IMOD: u32 = 0x04;
    /// Event ring segment table size.
    pub const ERSTSZ: u32 = 0x08;
    /// Event ring segment table base address (64-bit).
    pub const ERSTBA: u32 = 0x10;
    /// Event ring dequeue pointer (64-bit).
    pub const ERDP: u32 = 0x18;
}

/// IMAN bits (xHCI §5.5.2.1).
pub mod iman {
    /// Interrupt Pending (RW1C).
    pub const IP: u32 = 1 << 0;
    /// Interrupt Enable.
    pub const IE: u32 = 1 << 1;
}

/// ERDP bits (xHCI §5.5.2.3.3).
pub mod erdp {
    /// Dequeue ERST Segment Index.
    pub const DESI_MASK: u64 = 0x7;
    /// Event Handler Busy (RW1C).
    pub const EHB: u64 = 1 << 3;
    /// Dequeue pointer bits (16-byte aligned).
    pub const PTR_MASK: u64 = !0xF;
}

/// Decoded capability registers (xHCI §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Offset of the operational registers.
    pub cap_length: u8,
    /// Interface version (BCD, e.g. 0x0120).
    pub hci_version: u16,
    /// HCSPARAMS1.MaxSlots.
    pub max_slots: u8,
    /// HCSPARAMS1.MaxIntrs.
    pub max_intrs: u16,
    /// HCSPARAMS1.MaxPorts.
    pub max_ports: u8,
    /// HCSPARAMS2.IST.
    pub ist: u8,
    /// HCSPARAMS2.ERST Max (log2 of the ERST entry limit).
    pub erst_max_log2: u8,
    /// Max Scratchpad Buffers (Hi << 5 | Lo).
    pub max_scratchpad: u16,
    /// HCSPARAMS2.SPR (scratchpad restore).
    pub spr: bool,
    /// HCSPARAMS3.U1 Device Exit Latency.
    pub u1_latency: u8,
    /// HCSPARAMS3.U2 Device Exit Latency.
    pub u2_latency: u16,
    /// Raw HCCPARAMS1.
    pub hccparams1: u32,
    /// HCCPARAMS1.AC64: 64-bit addressing.
    pub ac64: bool,
    /// HCCPARAMS1.CSZ: 64-byte contexts.
    pub csz: bool,
    /// HCCPARAMS1.PPC: port power control.
    pub ppc: bool,
    /// Byte offset of the first extended capability (0 = none).
    pub xecp: u32,
    /// Byte offset of the doorbell array.
    pub dboff: u32,
    /// Byte offset of the runtime registers.
    pub rtsoff: u32,
    /// Raw HCCPARAMS2.
    pub hccparams2: u32,
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

impl Capabilities {
    /// Reads and validates the capability registers. `mmio_len` is the
    /// size of the MMIO window; every register block must lie inside it.
    pub fn read<M: Mmio + ?Sized>(mmio: &mut M, mmio_len: u32) -> Result<Self, Error> {
        let mut raw = [0u32; 8];
        for (i, r) in raw.iter_mut().enumerate() {
            *r = mmio.read32(i as u32 * 4);
        }
        Self::decode(raw, mmio_len)
    }

    /// Decodes the eight capability dwords.
    pub fn decode(raw: [u32; 8], mmio_len: u32) -> Result<Self, Error> {
        if raw.iter().all(|&r| r == u32::MAX) || raw[0] == u32::MAX {
            return Err(Error::ControllerGone);
        }
        let cap_length = raw[0] as u8;
        let hci_version = (raw[0] >> 16) as u16;
        if cap_length < 0x20 || !cap_length.is_multiple_of(4) {
            return Err(Error::BadCapability("CAPLENGTH"));
        }
        if hci_version == 0 {
            return Err(Error::BadCapability("HCIVERSION"));
        }
        let hcs1 = raw[1];
        let max_slots = hcs1 as u8;
        let max_intrs = ((hcs1 >> 8) & 0x7FF) as u16;
        let max_ports = (hcs1 >> 24) as u8;
        if max_slots == 0 {
            return Err(Error::BadCapability("MaxSlots is 0"));
        }
        if max_intrs == 0 {
            return Err(Error::BadCapability("MaxIntrs is 0"));
        }
        if max_ports == 0 {
            return Err(Error::BadCapability("MaxPorts is 0"));
        }
        let hcs2 = raw[2];
        let sp_hi = (hcs2 >> 21) & 0x1F;
        let sp_lo = (hcs2 >> 27) & 0x1F;
        let hcc1 = raw[4];
        let caps = Self {
            cap_length,
            hci_version,
            max_slots,
            max_intrs,
            max_ports,
            ist: (hcs2 & 0xF) as u8,
            erst_max_log2: ((hcs2 >> 4) & 0xF) as u8,
            max_scratchpad: ((sp_hi << 5) | sp_lo) as u16,
            spr: hcs2 & (1 << 26) != 0,
            u1_latency: raw[3] as u8,
            u2_latency: (raw[3] >> 16) as u16,
            hccparams1: hcc1,
            ac64: hcc1 & 1 != 0,
            csz: hcc1 & (1 << 2) != 0,
            ppc: hcc1 & (1 << 3) != 0,
            xecp: ((hcc1 >> 16) & 0xFFFF) * 4,
            dboff: raw[5] & !0x3,
            rtsoff: raw[6] & !0x1F,
            hccparams2: raw[7],
        };
        let len = u64::from(mmio_len);
        let op = (
            0u64,
            u64::from(cap_length) + 0x400 + 0x10 * u64::from(max_ports),
        );
        // MFINDEX plus interrupter 0 (the only one this driver uses).
        let rt = (
            u64::from(caps.rtsoff),
            u64::from(caps.rtsoff) + u64::from(rt::IR0 + rt::IR_STRIDE),
        );
        // Doorbell 0 (host controller) plus one per slot.
        let db = (
            u64::from(caps.dboff),
            u64::from(caps.dboff) + 4 * (u64::from(max_slots) + 1),
        );
        if op.1 > len {
            return Err(Error::BadCapability("operational registers exceed window"));
        }
        if caps.rtsoff == 0 || rt.1 > len || overlaps(rt, op) {
            return Err(Error::BadCapability("RTSOFF"));
        }
        if caps.dboff == 0 || db.1 > len || overlaps(db, op) || overlaps(db, rt) {
            return Err(Error::BadCapability("DBOFF"));
        }
        if caps.xecp != 0
            && (u64::from(caps.xecp) < u64::from(cap_length) || u64::from(caps.xecp) + 4 > len)
        {
            return Err(Error::BadCapability("xECP"));
        }
        Ok(caps)
    }

    /// Offset of an operational register.
    pub fn op(&self, reg: u32) -> u32 {
        u32::from(self.cap_length) + reg
    }

    /// Offset of PORTSC for `port` (1-based).
    pub fn portsc(&self, port: u8) -> u32 {
        self.op(op::PORT_BASE + op::PORT_STRIDE * (u32::from(port) - 1))
    }

    /// Offset of an interrupter-0 register.
    pub fn ir0(&self, reg: u32) -> u32 {
        self.rtsoff + rt::IR0 + reg
    }

    /// Offset of the doorbell register for `slot` (0 = host controller).
    pub fn doorbell(&self, slot: u8) -> u32 {
        self.dboff + 4 * u32::from(slot)
    }

    /// Size of one context data structure in bytes (32 or 64).
    pub fn context_size(&self) -> usize {
        if self.csz {
            64
        } else {
            32
        }
    }
}
