//! Completion status field (NVMe Base 2.0 §4.2.3 "Status Field Definition"
//! (?)): bits 14:0 of the field are SC (7:0), SCT (10:8), CRD (12:11),
//! More (13) and DNR (14); in completion dword 3 the field sits at bits
//! 31:17 above the phase tag.

/// Status field of a completion entry (15 bits, phase tag excluded).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(u16);

/// Status Code Type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusCodeType {
    /// 0h: generic command status.
    Generic,
    /// 1h: command specific status.
    CommandSpecific,
    /// 2h: media and data integrity errors.
    MediaDataIntegrity,
    /// 3h: path related status.
    PathRelated,
    /// 4h–6h: reserved.
    Reserved(u8),
    /// 7h: vendor specific.
    VendorSpecific,
}

/// Error classes a caller can act on differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusClass {
    /// Successful completion.
    Success,
    /// The command itself is invalid (opcode, field, namespace, PRP/SGL,
    /// sequence, CID conflict): retrying the same command cannot help.
    InvalidCommand,
    /// Aborted (abort requested, SQ deleted, power loss, preempt,
    /// interrupted, fused failure).
    Aborted,
    /// LBA out of range or capacity exceeded.
    OutOfRange,
    /// Namespace not ready, format or sanitize in progress, transient
    /// transport error: may succeed later.
    NotReady,
    /// Write protected, operation denied, reservation conflict.
    AccessDenied,
    /// Data transfer error.
    DataTransfer,
    /// Internal controller error.
    Internal,
    /// Command specific status (SCT 1h).
    CommandSpecific,
    /// Media or data integrity error (SCT 2h).
    Media,
    /// Path related (SCT 3h).
    Path,
    /// Vendor specific (SCT 7h).
    Vendor,
    /// Reserved SCT or unknown generic code.
    Other,
}

/// Generic status codes the driver names (SCT 0h).
pub mod generic {
    /// Successful Completion.
    pub const SUCCESS: u8 = 0x00;
    /// Invalid Command Opcode.
    pub const INVALID_OPCODE: u8 = 0x01;
    /// Invalid Field in Command.
    pub const INVALID_FIELD: u8 = 0x02;
    /// Command ID Conflict.
    pub const CID_CONFLICT: u8 = 0x03;
    /// Data Transfer Error.
    pub const DATA_TRANSFER_ERROR: u8 = 0x04;
    /// Commands Aborted due to Power Loss Notification.
    pub const ABORTED_POWER_LOSS: u8 = 0x05;
    /// Internal Error.
    pub const INTERNAL_ERROR: u8 = 0x06;
    /// Command Abort Requested.
    pub const ABORT_REQUESTED: u8 = 0x07;
    /// Command Aborted due to SQ Deletion.
    pub const ABORTED_SQ_DELETION: u8 = 0x08;
    /// Invalid Namespace or Format.
    pub const INVALID_NAMESPACE: u8 = 0x0B;
    /// Invalid PRP Offset.
    pub const INVALID_PRP_OFFSET: u8 = 0x13;
    /// LBA Out of Range.
    pub const LBA_OUT_OF_RANGE: u8 = 0x80;
    /// Namespace Not Ready.
    pub const NAMESPACE_NOT_READY: u8 = 0x82;
}

/// Command specific status codes of the admin commands the driver issues
/// (SCT 1h).
pub mod specific {
    /// Completion Queue Invalid.
    pub const INVALID_CQ: u8 = 0x00;
    /// Invalid Queue Identifier.
    pub const INVALID_QID: u8 = 0x01;
    /// Invalid Queue Size.
    pub const INVALID_QUEUE_SIZE: u8 = 0x02;
    /// Abort Command Limit Exceeded.
    pub const ABORT_LIMIT: u8 = 0x03;
    /// Invalid Queue Deletion.
    pub const INVALID_QUEUE_DELETION: u8 = 0x0C;
}

/// Media and data integrity status codes (SCT 2h).
pub mod media {
    /// Write Fault.
    pub const WRITE_FAULT: u8 = 0x80;
    /// Unrecovered Read Error.
    pub const UNRECOVERED_READ: u8 = 0x81;
}

impl Status {
    /// Successful completion.
    pub const SUCCESS: Status = Status(0);

    /// Status from completion dword 3.
    #[must_use]
    pub const fn from_dw3(dw3: u32) -> Self {
        Status(((dw3 >> 17) & 0x7FFF) as u16)
    }

    /// Status from its parts; CRD is left 0.
    #[must_use]
    pub const fn new(sct: u8, sc: u8, more: bool, dnr: bool) -> Self {
        Status(
            (sc as u16)
                | (((sct & 7) as u16) << 8)
                | if more { 1 << 13 } else { 0 }
                | if dnr { 1 << 14 } else { 0 },
        )
    }

    /// Status from the raw 15-bit field (bit 15 is ignored).
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        Status(raw & 0x7FFF)
    }

    /// Raw 15-bit field.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Status Code.
    #[must_use]
    pub const fn sc(self) -> u8 {
        self.0 as u8
    }

    /// Status Code Type (raw).
    #[must_use]
    pub const fn sct(self) -> u8 {
        ((self.0 >> 8) & 7) as u8
    }

    /// Status Code Type.
    #[must_use]
    pub const fn code_type(self) -> StatusCodeType {
        match self.sct() {
            0 => StatusCodeType::Generic,
            1 => StatusCodeType::CommandSpecific,
            2 => StatusCodeType::MediaDataIntegrity,
            3 => StatusCodeType::PathRelated,
            7 => StatusCodeType::VendorSpecific,
            n => StatusCodeType::Reserved(n),
        }
    }

    /// Command Retry Delay selector (0 = none).
    #[must_use]
    pub const fn crd(self) -> u8 {
        ((self.0 >> 11) & 3) as u8
    }

    /// More: additional information in the Error Information log page.
    #[must_use]
    pub const fn more(self) -> bool {
        self.0 & (1 << 13) != 0
    }

    /// Do Not Retry: the same command is expected to fail again.
    #[must_use]
    pub const fn dnr(self) -> bool {
        self.0 & (1 << 14) != 0
    }

    /// SCT = 0 and SC = 0.
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.sct() == 0 && self.sc() == 0
    }

    /// Generic "Command Abort Requested".
    #[must_use]
    pub const fn is_abort_requested(self) -> bool {
        self.sct() == 0 && self.sc() == generic::ABORT_REQUESTED
    }

    /// Error class.
    #[must_use]
    pub const fn class(self) -> StatusClass {
        match self.code_type() {
            StatusCodeType::Generic => match self.sc() {
                0x00 => StatusClass::Success,
                0x01..=0x03 | 0x0B..=0x14 | 0x16 | 0x18 | 0x1A | 0x1E | 0x1F => {
                    StatusClass::InvalidCommand
                }
                0x04 => StatusClass::DataTransfer,
                0x05 | 0x07..=0x0A | 0x1B | 0x21 => StatusClass::Aborted,
                0x06 => StatusClass::Internal,
                0x15 | 0x20 | 0x83 => StatusClass::AccessDenied,
                0x1D | 0x22 | 0x82 | 0x84 => StatusClass::NotReady,
                0x80 | 0x81 => StatusClass::OutOfRange,
                _ => StatusClass::Other,
            },
            StatusCodeType::CommandSpecific => StatusClass::CommandSpecific,
            StatusCodeType::MediaDataIntegrity => StatusClass::Media,
            StatusCodeType::PathRelated => StatusClass::Path,
            StatusCodeType::VendorSpecific => StatusClass::Vendor,
            StatusCodeType::Reserved(_) => StatusClass::Other,
        }
    }
}

impl core::fmt::Debug for Status {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Status")
            .field("sct", &self.sct())
            .field("sc", &self.sc())
            .field("crd", &self.crd())
            .field("more", &self.more())
            .field("dnr", &self.dnr())
            .finish()
    }
}
