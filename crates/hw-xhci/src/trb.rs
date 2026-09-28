//! Transfer Request Blocks (xHCI §6.4).
//!
//! A TRB is four little-endian dwords; dword 3 carries the cycle bit
//! (bit 0), flags and the TRB type (bits 15:10).

use crate::descriptor::SetupPacket;

/// TRB types (xHCI §6.4.6, Table 6-91).
pub mod ty {
    /// Normal (transfer).
    pub const NORMAL: u8 = 1;
    /// Setup Stage (transfer).
    pub const SETUP: u8 = 2;
    /// Data Stage (transfer).
    pub const DATA: u8 = 3;
    /// Status Stage (transfer).
    pub const STATUS: u8 = 4;
    /// Isoch (transfer).
    pub const ISOCH: u8 = 5;
    /// Link.
    pub const LINK: u8 = 6;
    /// Event Data (transfer).
    pub const EVENT_DATA: u8 = 7;
    /// No Op (transfer).
    pub const NOOP: u8 = 8;
    /// Enable Slot command.
    pub const ENABLE_SLOT: u8 = 9;
    /// Disable Slot command.
    pub const DISABLE_SLOT: u8 = 10;
    /// Address Device command.
    pub const ADDRESS_DEVICE: u8 = 11;
    /// Configure Endpoint command.
    pub const CONFIGURE_ENDPOINT: u8 = 12;
    /// Evaluate Context command.
    pub const EVALUATE_CONTEXT: u8 = 13;
    /// Reset Endpoint command.
    pub const RESET_ENDPOINT: u8 = 14;
    /// Stop Endpoint command.
    pub const STOP_ENDPOINT: u8 = 15;
    /// Set TR Dequeue Pointer command.
    pub const SET_TR_DEQUEUE: u8 = 16;
    /// Reset Device command.
    pub const RESET_DEVICE: u8 = 17;
    /// No Op command.
    pub const NOOP_COMMAND: u8 = 23;
    /// Transfer Event.
    pub const TRANSFER_EVENT: u8 = 32;
    /// Command Completion Event.
    pub const COMMAND_COMPLETION: u8 = 33;
    /// Port Status Change Event.
    pub const PORT_STATUS_CHANGE: u8 = 34;
    /// Bandwidth Request Event.
    pub const BANDWIDTH_REQUEST: u8 = 35;
    /// Host Controller Event.
    pub const HOST_CONTROLLER: u8 = 37;
    /// Device Notification Event.
    pub const DEVICE_NOTIFICATION: u8 = 38;
    /// MFINDEX Wrap Event.
    pub const MFINDEX_WRAP: u8 = 39;
}

/// Dword-3 flag bits (xHCI §6.4).
pub mod flag {
    /// Cycle bit.
    pub const CYCLE: u32 = 1 << 0;
    /// Toggle Cycle (Link TRB) / Evaluate Next TRB (transfer TRBs).
    pub const TC: u32 = 1 << 1;
    /// Interrupt on Short Packet.
    pub const ISP: u32 = 1 << 2;
    /// No Snoop.
    pub const NS: u32 = 1 << 3;
    /// Chain.
    pub const CH: u32 = 1 << 4;
    /// Interrupt On Completion.
    pub const IOC: u32 = 1 << 5;
    /// Immediate Data.
    pub const IDT: u32 = 1 << 6;
    /// Block Set Address Request (Address Device), Deconfigure (Configure
    /// Endpoint), Transfer State Preserve (Reset Endpoint).
    pub const BSR: u32 = 1 << 9;
    /// Direction IN (Data/Status Stage).
    pub const DIR_IN: u32 = 1 << 16;
    /// Suspend (Stop Endpoint).
    pub const SP: u32 = 1 << 23;
    /// Event Data (Transfer Event).
    pub const ED: u32 = 1 << 2;
}

/// One TRB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb(pub [u32; 4]);

const fn typed(t: u8) -> u32 {
    (t as u32) << 10
}

const fn lo(v: u64) -> u32 {
    v as u32
}

const fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

impl Trb {
    /// TRB type field.
    pub const fn trb_type(&self) -> u8 {
        ((self.0[3] >> 10) & 0x3F) as u8
    }
    /// Cycle bit.
    pub const fn cycle(&self) -> bool {
        self.0[3] & flag::CYCLE != 0
    }
    /// Chain bit.
    pub const fn chain(&self) -> bool {
        self.0[3] & flag::CH != 0
    }
    /// Same TRB with the cycle bit set to `c`.
    #[must_use]
    pub const fn with_cycle(self, c: bool) -> Self {
        let mut d = self.0;
        d[3] = (d[3] & !flag::CYCLE) | c as u32;
        Self(d)
    }
    /// Dwords 0-1 as a 64-bit pointer.
    pub const fn pointer(&self) -> u64 {
        self.0[0] as u64 | (self.0[1] as u64) << 32
    }
    /// Little-endian bytes.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        for (i, d) in self.0.iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&d.to_le_bytes());
        }
        b
    }
    /// From little-endian bytes.
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        let mut d = [0u32; 4];
        for (i, v) in d.iter_mut().enumerate() {
            *v = u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        Self(d)
    }

    /// Normal TRB (§6.4.1.1). `len` is the TRB Transfer Length (17 bits);
    /// `flags` from [`flag`] (ISP, CH, IOC, ...).
    pub const fn normal(buf: u64, len: u32, flags: u32) -> Self {
        Self([lo(buf), hi(buf), len & 0x1_FFFF, typed(ty::NORMAL) | flags])
    }

    /// Setup Stage TRB (§6.4.1.2.1) with the setup packet as immediate
    /// data.
    pub const fn setup_stage(setup: &SetupPacket, trt: Trt, flags: u32) -> Self {
        Self([
            setup.request_type as u32 | (setup.request as u32) << 8 | (setup.value as u32) << 16,
            setup.index as u32 | (setup.length as u32) << 16,
            8,
            typed(ty::SETUP) | flag::IDT | (trt as u32) << 16 | flags,
        ])
    }

    /// Data Stage TRB (§6.4.1.2.2).
    pub const fn data_stage(buf: u64, len: u32, dir_in: bool, flags: u32) -> Self {
        Self([
            lo(buf),
            hi(buf),
            len & 0x1_FFFF,
            typed(ty::DATA) | if dir_in { flag::DIR_IN } else { 0 } | flags,
        ])
    }

    /// Status Stage TRB (§6.4.1.2.3).
    pub const fn status_stage(dir_in: bool, flags: u32) -> Self {
        Self([
            0,
            0,
            0,
            typed(ty::STATUS) | if dir_in { flag::DIR_IN } else { 0 } | flags,
        ])
    }

    /// Link TRB (§6.4.4.1) pointing at `target`.
    pub const fn link(target: u64, toggle: bool, chain: bool) -> Self {
        Self([
            lo(target),
            hi(target),
            0,
            typed(ty::LINK) | if toggle { flag::TC } else { 0 } | if chain { flag::CH } else { 0 },
        ])
    }
}

/// Transfer Type of a Setup Stage TRB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trt {
    /// No data stage.
    NoData = 0,
    /// OUT data stage.
    Out = 2,
    /// IN data stage.
    In = 3,
}

/// Commands (xHCI §6.4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// No Op Command (§6.4.3.1).
    NoOp,
    /// Enable Slot (§6.4.3.2).
    EnableSlot {
        /// Protocol Slot Type from the Supported Protocol capability.
        slot_type: u8,
    },
    /// Disable Slot (§6.4.3.3).
    DisableSlot {
        /// Slot ID.
        slot: u8,
    },
    /// Address Device (§6.4.3.4).
    AddressDevice {
        /// Slot ID.
        slot: u8,
        /// Input context physical address (16-byte aligned).
        input_context: u64,
        /// Block Set Address Request.
        bsr: bool,
    },
    /// Configure Endpoint (§6.4.3.5).
    ConfigureEndpoint {
        /// Slot ID.
        slot: u8,
        /// Input context physical address.
        input_context: u64,
        /// Deconfigure.
        deconfigure: bool,
    },
    /// Evaluate Context (§6.4.3.6).
    EvaluateContext {
        /// Slot ID.
        slot: u8,
        /// Input context physical address.
        input_context: u64,
    },
    /// Reset Endpoint (§6.4.3.7).
    ResetEndpoint {
        /// Slot ID.
        slot: u8,
        /// Device Context Index.
        dci: u8,
        /// Transfer State Preserve.
        preserve: bool,
    },
    /// Stop Endpoint (§6.4.3.8).
    StopEndpoint {
        /// Slot ID.
        slot: u8,
        /// Device Context Index.
        dci: u8,
        /// Suspend.
        suspend: bool,
    },
    /// Set TR Dequeue Pointer (§6.4.3.9).
    SetTrDequeue {
        /// Slot ID.
        slot: u8,
        /// Device Context Index.
        dci: u8,
        /// New dequeue pointer (16-byte aligned).
        dequeue: u64,
        /// Dequeue Cycle State.
        cycle: bool,
    },
    /// Reset Device (§6.4.3.10).
    ResetDevice {
        /// Slot ID.
        slot: u8,
    },
}

const fn slot_field(slot: u8) -> u32 {
    (slot as u32) << 24
}

const fn ep_field(dci: u8) -> u32 {
    ((dci & 0x1F) as u32) << 16
}

impl Command {
    /// Encodes the command with cycle bit 0.
    pub const fn encode(&self) -> Trb {
        match *self {
            Self::NoOp => Trb([0, 0, 0, typed(ty::NOOP_COMMAND)]),
            Self::EnableSlot { slot_type } => Trb([
                0,
                0,
                0,
                typed(ty::ENABLE_SLOT) | ((slot_type & 0x1F) as u32) << 16,
            ]),
            Self::DisableSlot { slot } => {
                Trb([0, 0, 0, typed(ty::DISABLE_SLOT) | slot_field(slot)])
            }
            Self::AddressDevice {
                slot,
                input_context,
                bsr,
            } => Trb([
                lo(input_context),
                hi(input_context),
                0,
                typed(ty::ADDRESS_DEVICE) | if bsr { flag::BSR } else { 0 } | slot_field(slot),
            ]),
            Self::ConfigureEndpoint {
                slot,
                input_context,
                deconfigure,
            } => Trb([
                lo(input_context),
                hi(input_context),
                0,
                typed(ty::CONFIGURE_ENDPOINT)
                    | if deconfigure { flag::BSR } else { 0 }
                    | slot_field(slot),
            ]),
            Self::EvaluateContext {
                slot,
                input_context,
            } => Trb([
                lo(input_context),
                hi(input_context),
                0,
                typed(ty::EVALUATE_CONTEXT) | slot_field(slot),
            ]),
            Self::ResetEndpoint {
                slot,
                dci,
                preserve,
            } => Trb([
                0,
                0,
                0,
                typed(ty::RESET_ENDPOINT)
                    | if preserve { flag::BSR } else { 0 }
                    | ep_field(dci)
                    | slot_field(slot),
            ]),
            Self::StopEndpoint { slot, dci, suspend } => Trb([
                0,
                0,
                0,
                typed(ty::STOP_ENDPOINT)
                    | if suspend { flag::SP } else { 0 }
                    | ep_field(dci)
                    | slot_field(slot),
            ]),
            Self::SetTrDequeue {
                slot,
                dci,
                dequeue,
                cycle,
            } => {
                let p = (dequeue & !0xF) | cycle as u64;
                Trb([
                    lo(p),
                    hi(p),
                    0,
                    typed(ty::SET_TR_DEQUEUE) | ep_field(dci) | slot_field(slot),
                ])
            }
            Self::ResetDevice { slot } => {
                Trb([0, 0, 0, typed(ty::RESET_DEVICE) | slot_field(slot)])
            }
        }
    }
}

/// Completion code (xHCI §6.4.5, Table 6-90).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompletionCode(pub u8);

#[allow(missing_docs)]
impl CompletionCode {
    pub const INVALID: Self = Self(0);
    pub const SUCCESS: Self = Self(1);
    pub const DATA_BUFFER_ERROR: Self = Self(2);
    pub const BABBLE_DETECTED: Self = Self(3);
    pub const USB_TRANSACTION_ERROR: Self = Self(4);
    pub const TRB_ERROR: Self = Self(5);
    pub const STALL_ERROR: Self = Self(6);
    pub const RESOURCE_ERROR: Self = Self(7);
    pub const BANDWIDTH_ERROR: Self = Self(8);
    pub const NO_SLOTS_AVAILABLE: Self = Self(9);
    pub const INVALID_STREAM_TYPE: Self = Self(10);
    pub const SLOT_NOT_ENABLED: Self = Self(11);
    pub const ENDPOINT_NOT_ENABLED: Self = Self(12);
    pub const SHORT_PACKET: Self = Self(13);
    pub const RING_UNDERRUN: Self = Self(14);
    pub const RING_OVERRUN: Self = Self(15);
    pub const VF_EVENT_RING_FULL: Self = Self(16);
    pub const PARAMETER_ERROR: Self = Self(17);
    pub const BANDWIDTH_OVERRUN: Self = Self(18);
    pub const CONTEXT_STATE_ERROR: Self = Self(19);
    pub const NO_PING_RESPONSE: Self = Self(20);
    pub const EVENT_RING_FULL: Self = Self(21);
    pub const INCOMPATIBLE_DEVICE: Self = Self(22);
    pub const MISSED_SERVICE: Self = Self(23);
    pub const COMMAND_RING_STOPPED: Self = Self(24);
    pub const COMMAND_ABORTED: Self = Self(25);
    pub const STOPPED: Self = Self(26);
    pub const STOPPED_LENGTH_INVALID: Self = Self(27);
    pub const STOPPED_SHORT_PACKET: Self = Self(28);
    pub const MAX_EXIT_LATENCY_TOO_LARGE: Self = Self(29);
    pub const ISOCH_BUFFER_OVERRUN: Self = Self(31);
    pub const EVENT_LOST: Self = Self(32);
    pub const UNDEFINED: Self = Self(33);
    pub const INVALID_STREAM_ID: Self = Self(34);
    pub const SECONDARY_BANDWIDTH: Self = Self(35);
    pub const SPLIT_TRANSACTION: Self = Self(36);

    /// True for Success.
    pub const fn is_success(self) -> bool {
        self.0 == 1
    }

    /// True for the Stopped family (26-28).
    pub const fn is_stopped(self) -> bool {
        matches!(self.0, 26..=28)
    }
}

/// A decoded event TRB (xHCI §6.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Transfer Event (§6.4.2.1).
    Transfer {
        /// Pointer to the TRB that generated the event (or Event Data).
        trb: u64,
        /// TRB Transfer Length: residual bytes not transferred.
        residual: u32,
        /// Completion code.
        code: CompletionCode,
        /// Slot ID.
        slot: u8,
        /// Endpoint ID (DCI).
        dci: u8,
        /// Event Data flag: `trb` holds Event Data, not a pointer.
        event_data: bool,
    },
    /// Command Completion Event (§6.4.2.2).
    CommandCompletion {
        /// Pointer to the command TRB.
        trb: u64,
        /// Completion code.
        code: CompletionCode,
        /// Slot ID.
        slot: u8,
        /// Command Completion Parameter.
        param: u32,
    },
    /// Port Status Change Event (§6.4.2.3).
    PortStatusChange {
        /// Port ID (1-based).
        port: u8,
        /// Completion code.
        code: CompletionCode,
    },
    /// Host Controller Event (§6.4.2.6).
    HostController {
        /// Completion code (e.g. Event Ring Full).
        code: CompletionCode,
    },
    /// Any other event type.
    Other {
        /// TRB type.
        trb_type: u8,
    },
}

impl Event {
    /// Decodes an event TRB.
    pub const fn decode(t: Trb) -> Self {
        let d = t.0;
        let code = CompletionCode((d[2] >> 24) as u8);
        match t.trb_type() {
            ty::TRANSFER_EVENT => Self::Transfer {
                trb: t.pointer(),
                residual: d[2] & 0x00FF_FFFF,
                code,
                slot: (d[3] >> 24) as u8,
                dci: ((d[3] >> 16) & 0x1F) as u8,
                event_data: d[3] & flag::ED != 0,
            },
            ty::COMMAND_COMPLETION => Self::CommandCompletion {
                trb: t.pointer(),
                code,
                slot: (d[3] >> 24) as u8,
                param: d[2] & 0x00FF_FFFF,
            },
            ty::PORT_STATUS_CHANGE => Self::PortStatusChange {
                port: (d[0] >> 24) as u8,
                code,
            },
            ty::HOST_CONTROLLER => Self::HostController { code },
            other => Self::Other { trb_type: other },
        }
    }
}
