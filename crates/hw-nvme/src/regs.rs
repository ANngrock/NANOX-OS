//! Controller registers.
//!
//! Layout: NVMe Base 2.0 §3.1.3 "Controller Properties" (?) (CAP, VS, CC,
//! CSTS, AQA, ASQ, ACQ) and the doorbell layout of NVMe over PCIe
//! Transport Specification 1.0 §3.1.2 (?): SQ `y` tail doorbell at
//! `0x1000 + (2y) * (4 << CAP.DSTRD)`, CQ `y` head doorbell at
//! `0x1000 + (2y + 1) * (4 << CAP.DSTRD)`.

/// Controller Capabilities (64-bit).
pub const CAP: u32 = 0x00;
/// Version.
pub const VS: u32 = 0x08;
/// Interrupt Mask Set.
pub const INTMS: u32 = 0x0C;
/// Interrupt Mask Clear.
pub const INTMC: u32 = 0x10;
/// Controller Configuration.
pub const CC: u32 = 0x14;
/// Controller Status.
pub const CSTS: u32 = 0x1C;
/// Admin Queue Attributes.
pub const AQA: u32 = 0x24;
/// Admin Submission Queue base (64-bit).
pub const ASQ: u32 = 0x28;
/// Admin Completion Queue base (64-bit).
pub const ACQ: u32 = 0x30;
/// Offset of the first doorbell.
pub const DOORBELL_BASE: u64 = 0x1000;

/// CC bits (NVMe Base 2.0 §3.1.3.5 (?)).
pub mod cc {
    /// Enable.
    pub const EN: u32 = 1;
    /// I/O Command Set Selected, bits 6:4 (000b = NVM command set).
    pub const CSS_SHIFT: u32 = 4;
    /// Memory Page Size, bits 10:7 (2^(12 + MPS)).
    pub const MPS_SHIFT: u32 = 7;
    /// Arbitration Mechanism Selected, bits 13:11 (000b = round robin).
    pub const AMS_SHIFT: u32 = 11;
    /// Shutdown Notification, bits 15:14.
    pub const SHN_SHIFT: u32 = 14;
    /// SHN field mask.
    pub const SHN_MASK: u32 = 3 << SHN_SHIFT;
    /// SHN = 01b, normal shutdown.
    pub const SHN_NORMAL: u32 = 1 << SHN_SHIFT;
    /// SHN = 10b, abrupt shutdown.
    pub const SHN_ABRUPT: u32 = 2 << SHN_SHIFT;
    /// I/O Submission Queue Entry Size, bits 19:16 (2^n bytes).
    pub const IOSQES_SHIFT: u32 = 16;
    /// I/O Completion Queue Entry Size, bits 23:20 (2^n bytes).
    pub const IOCQES_SHIFT: u32 = 20;
    /// log2 of the submission queue entry size (64 bytes).
    pub const SQES_LOG2: u32 = 6;
    /// log2 of the completion queue entry size (16 bytes).
    pub const CQES_LOG2: u32 = 4;

    /// CC value the driver programs (EN clear): NVM command set, 4 KiB
    /// pages, round robin, no shutdown, 64/16-byte entries.
    pub const DRIVER_CONFIG: u32 = (SQES_LOG2 << IOSQES_SHIFT) | (CQES_LOG2 << IOCQES_SHIFT);
}

/// CSTS bits (NVMe Base 2.0 §3.1.3.6 (?)).
pub mod csts {
    /// Ready.
    pub const RDY: u32 = 1;
    /// Controller Fatal Status.
    pub const CFS: u32 = 1 << 1;
    /// Shutdown Status, bits 3:2.
    pub const SHST_SHIFT: u32 = 2;
    /// SHST field mask.
    pub const SHST_MASK: u32 = 3 << SHST_SHIFT;
    /// NVM Subsystem Reset Occurred.
    pub const NSSRO: u32 = 1 << 4;
    /// Processing Paused.
    pub const PP: u32 = 1 << 5;
}

/// Why a CAP value was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapError {
    /// All ones: the register window does not respond.
    AllOnes,
    /// MQES = 0: queues of fewer than two entries are not allowed.
    QueueEntries,
    /// MPSMAX < MPSMIN.
    PageSizeRange,
    /// CSS = 0: no command set at all.
    NoCommandSet,
}

/// Decoded, validated Controller Capabilities (NVMe Base 2.0 §3.1.3.1 (?)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    raw: u64,
}

impl Capabilities {
    /// Validates a raw CAP value.
    pub fn parse(raw: u64) -> Result<Self, CapError> {
        if raw == u64::MAX {
            return Err(CapError::AllOnes);
        }
        let cap = Self { raw };
        if cap.mqes() == 0 {
            return Err(CapError::QueueEntries);
        }
        if cap.mpsmax() < cap.mpsmin() {
            return Err(CapError::PageSizeRange);
        }
        if cap.css() == 0 {
            return Err(CapError::NoCommandSet);
        }
        Ok(cap)
    }

    /// Raw register value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.raw
    }

    /// Maximum Queue Entries Supported, 0's based (bits 15:0).
    #[must_use]
    pub const fn mqes(self) -> u16 {
        self.raw as u16
    }

    /// Largest I/O queue the controller accepts, in entries.
    #[must_use]
    pub const fn max_queue_entries(self) -> u32 {
        self.mqes() as u32 + 1
    }

    /// Contiguous Queues Required (bit 16).
    #[must_use]
    pub const fn contiguous_required(self) -> bool {
        (self.raw >> 16) & 1 != 0
    }

    /// Timeout (bits 31:24) in 500 ms units.
    #[must_use]
    pub const fn timeout_units(self) -> u8 {
        (self.raw >> 24) as u8
    }

    /// Bound for a CSTS.RDY transition. TO = 0 is treated as one unit
    /// (500 ms) so a zero field never turns into an immediate timeout.
    #[must_use]
    pub const fn ready_timeout_ns(self) -> u64 {
        let units = if self.timeout_units() == 0 {
            1
        } else {
            self.timeout_units() as u64
        };
        units * 500_000_000
    }

    /// Doorbell Stride exponent (bits 35:32).
    #[must_use]
    pub const fn dstrd(self) -> u8 {
        ((self.raw >> 32) & 0xF) as u8
    }

    /// Distance between doorbells in bytes (`4 << DSTRD`).
    #[must_use]
    pub const fn doorbell_stride(self) -> u64 {
        4 << self.dstrd()
    }

    /// NVM Subsystem Reset Supported (bit 36).
    #[must_use]
    pub const fn nssr_supported(self) -> bool {
        (self.raw >> 36) & 1 != 0
    }

    /// Command Sets Supported (bits 44:37).
    #[must_use]
    pub const fn css(self) -> u8 {
        (self.raw >> 37) as u8
    }

    /// CSS bit 0: NVM command set.
    #[must_use]
    pub const fn nvm_command_set(self) -> bool {
        self.css() & 1 != 0
    }

    /// Memory Page Size Minimum exponent (bits 51:48), page = 2^(12 + n).
    #[must_use]
    pub const fn mpsmin(self) -> u8 {
        ((self.raw >> 48) & 0xF) as u8
    }

    /// Memory Page Size Maximum exponent (bits 55:52).
    #[must_use]
    pub const fn mpsmax(self) -> u8 {
        ((self.raw >> 52) & 0xF) as u8
    }

    /// Offset of the submission queue `qid` tail doorbell.
    #[must_use]
    pub const fn sq_tail_doorbell(self, qid: u16) -> u64 {
        DOORBELL_BASE + (2 * qid as u64) * self.doorbell_stride()
    }

    /// Offset of the completion queue `qid` head doorbell.
    #[must_use]
    pub const fn cq_head_doorbell(self, qid: u16) -> u64 {
        DOORBELL_BASE + (2 * qid as u64 + 1) * self.doorbell_stride()
    }
}

/// Version register (NVMe Base 2.0 §3.1.3.2 (?)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    raw: u32,
}

impl Version {
    /// Wraps a raw VS value without validation.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Raw register value.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.raw
    }

    /// Major version (bits 31:16).
    #[must_use]
    pub const fn major(self) -> u16 {
        (self.raw >> 16) as u16
    }

    /// Minor version (bits 15:8).
    #[must_use]
    pub const fn minor(self) -> u8 {
        (self.raw >> 8) as u8
    }

    /// Tertiary version (bits 7:0).
    #[must_use]
    pub const fn tertiary(self) -> u8 {
        self.raw as u8
    }
}

/// Shutdown Status (CSTS.SHST).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownStatus {
    /// 00b: normal operation.
    Normal,
    /// 01b: shutdown processing occurring.
    Occurring,
    /// 10b: shutdown processing complete.
    Complete,
    /// 11b: reserved.
    Reserved,
}

/// Snapshot of CSTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerStatus(pub u32);

impl ControllerStatus {
    /// All ones: the register window does not respond.
    #[must_use]
    pub const fn is_all_ones(self) -> bool {
        self.0 == u32::MAX
    }

    /// CSTS.RDY.
    #[must_use]
    pub const fn ready(self) -> bool {
        self.0 & csts::RDY != 0
    }

    /// CSTS.CFS.
    #[must_use]
    pub const fn fatal(self) -> bool {
        self.0 & csts::CFS != 0
    }

    /// CSTS.SHST.
    #[must_use]
    pub const fn shutdown(self) -> ShutdownStatus {
        match (self.0 & csts::SHST_MASK) >> csts::SHST_SHIFT {
            0 => ShutdownStatus::Normal,
            1 => ShutdownStatus::Occurring,
            2 => ShutdownStatus::Complete,
            _ => ShutdownStatus::Reserved,
        }
    }
}
