//! Root hub ports: PORTSC (xHCI §5.4.8) and speeds.
//!
//! PORTSC mixes read-only, RW, RW1S, RWS and RW1C bits. Writing back a
//! value that was read would clear every pending change bit (RW1C) and,
//! worse, disable the port (PED is RW1CS: writing 1 disables). Every write
//! therefore starts from [`neutral`], which keeps only the RWS bits that
//! must be preserved, and then sets exactly the bits the caller means.

/// PORTSC bits.
pub mod portsc {
    /// Current Connect Status (RO).
    pub const CCS: u32 = 1 << 0;
    /// Port Enabled/Disabled (RW1CS: writing 1 disables the port).
    pub const PED: u32 = 1 << 1;
    /// Over-current Active (RO).
    pub const OCA: u32 = 1 << 3;
    /// Port Reset (RW1S).
    pub const PR: u32 = 1 << 4;
    /// Port Link State shift.
    pub const PLS_SHIFT: u32 = 5;
    /// Port Link State mask (RWS, written only with LWS).
    pub const PLS_MASK: u32 = 0xF << PLS_SHIFT;
    /// Port Power (RWS).
    pub const PP: u32 = 1 << 9;
    /// Port Speed shift.
    pub const SPEED_SHIFT: u32 = 10;
    /// Port Speed mask (RO).
    pub const SPEED_MASK: u32 = 0xF << SPEED_SHIFT;
    /// Port Indicator Control (RWS).
    pub const PIC_MASK: u32 = 0x3 << 14;
    /// Port Link State Write Strobe (RW).
    pub const LWS: u32 = 1 << 16;
    /// Connect Status Change (RW1CS).
    pub const CSC: u32 = 1 << 17;
    /// Port Enabled/Disabled Change (RW1CS).
    pub const PEC: u32 = 1 << 18;
    /// Warm Port Reset Change (RW1CS, USB3 only).
    pub const WRC: u32 = 1 << 19;
    /// Over-current Change (RW1CS).
    pub const OCC: u32 = 1 << 20;
    /// Port Reset Change (RW1CS).
    pub const PRC: u32 = 1 << 21;
    /// Port Link State Change (RW1CS).
    pub const PLC: u32 = 1 << 22;
    /// Port Config Error Change (RW1CS, USB3 only).
    pub const CEC: u32 = 1 << 23;
    /// Cold Attach Status (RO).
    pub const CAS: u32 = 1 << 24;
    /// Wake on Connect Enable (RWS).
    pub const WCE: u32 = 1 << 25;
    /// Wake on Disconnect Enable (RWS).
    pub const WDE: u32 = 1 << 26;
    /// Wake on Over-current Enable (RWS).
    pub const WOE: u32 = 1 << 27;
    /// Device Removable (RO).
    pub const DR: u32 = 1 << 30;
    /// Warm Port Reset (RW1S, USB3 only).
    pub const WPR: u32 = 1 << 31;
    /// All change bits.
    pub const CHANGE_BITS: u32 = CSC | PEC | WRC | OCC | PRC | PLC | CEC;
    /// Bits a write must carry over from the read value.
    pub const PRESERVE: u32 = PP | PIC_MASK | WCE | WDE | WOE;
}

/// Value to write to PORTSC that changes nothing: RWS bits kept, RW1C and
/// RW1S bits and PED written as 0.
pub const fn neutral(raw: u32) -> u32 {
    raw & portsc::PRESERVE
}

/// Value that clears exactly the change bits `changes` (masked to change
/// bits) and nothing else.
pub const fn clear_changes(raw: u32, changes: u32) -> u32 {
    neutral(raw) | (changes & portsc::CHANGE_BITS)
}

/// Port link state (PORTSC.PLS, xHCI §5.4.8 Table 5-27).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// U0 (enabled, active).
    U0,
    /// U1.
    U1,
    /// U2.
    U2,
    /// U3 (suspended).
    U3,
    /// Disabled.
    Disabled,
    /// RxDetect.
    RxDetect,
    /// SS.Inactive (needs warm reset).
    Inactive,
    /// Polling.
    Polling,
    /// Recovery.
    Recovery,
    /// Hot Reset.
    HotReset,
    /// Compliance Mode (needs warm reset).
    Compliance,
    /// Test Mode.
    Test,
    /// Resume.
    Resume,
    /// Reserved value.
    Reserved(u8),
}

impl LinkState {
    /// Decodes a PLS value.
    pub const fn from_raw(v: u8) -> Self {
        match v {
            0 => Self::U0,
            1 => Self::U1,
            2 => Self::U2,
            3 => Self::U3,
            4 => Self::Disabled,
            5 => Self::RxDetect,
            6 => Self::Inactive,
            7 => Self::Polling,
            8 => Self::Recovery,
            9 => Self::HotReset,
            10 => Self::Compliance,
            11 => Self::Test,
            15 => Self::Resume,
            v => Self::Reserved(v),
        }
    }
}

/// Device speed as reported by PORTSC.Port Speed with the default
/// Protocol Speed ID mapping (xHCI §7.2.1, Table 7-13 (unverified)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// Full speed, 12 Mb/s.
    Full,
    /// Low speed, 1.5 Mb/s.
    Low,
    /// High speed, 480 Mb/s.
    High,
    /// SuperSpeed Gen1 x1, 5 Gb/s.
    Super,
    /// SuperSpeedPlus, 10 Gb/s or more.
    SuperPlus,
}

impl Speed {
    /// Default PSI mapping; `None` for 0 or a custom PSI value.
    pub const fn from_psi(id: u8) -> Option<Self> {
        match id {
            1 => Some(Self::Full),
            2 => Some(Self::Low),
            3 => Some(Self::High),
            4 => Some(Self::Super),
            5 => Some(Self::SuperPlus),
            _ => None,
        }
    }

    /// Default Protocol Speed ID.
    pub const fn psi(self) -> u8 {
        match self {
            Self::Full => 1,
            Self::Low => 2,
            Self::High => 3,
            Self::Super => 4,
            Self::SuperPlus => 5,
        }
    }

    /// True for SuperSpeed and SuperSpeedPlus.
    pub const fn is_superspeed(self) -> bool {
        matches!(self, Self::Super | Self::SuperPlus)
    }

    /// Initial EP0 max packet size before the device descriptor is read
    /// (USB 2.0 §5.5.3, USB 3.2 §9.6.1).
    pub const fn default_ep0_mps(self) -> u16 {
        match self {
            Self::Low | Self::Full => 8,
            Self::High => 64,
            Self::Super | Self::SuperPlus => 512,
        }
    }
}

/// A PORTSC value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    /// Raw register value.
    pub raw: u32,
}

impl PortStatus {
    /// Wraps a raw value.
    pub const fn new(raw: u32) -> Self {
        Self { raw }
    }
    /// CCS.
    pub const fn connected(self) -> bool {
        self.raw & portsc::CCS != 0
    }
    /// PED.
    pub const fn enabled(self) -> bool {
        self.raw & portsc::PED != 0
    }
    /// PR.
    pub const fn in_reset(self) -> bool {
        self.raw & portsc::PR != 0
    }
    /// PP.
    pub const fn powered(self) -> bool {
        self.raw & portsc::PP != 0
    }
    /// OCA.
    pub const fn over_current(self) -> bool {
        self.raw & portsc::OCA != 0
    }
    /// PLS.
    pub const fn link_state(self) -> LinkState {
        LinkState::from_raw(((self.raw & portsc::PLS_MASK) >> portsc::PLS_SHIFT) as u8)
    }
    /// Raw Port Speed field.
    pub const fn speed_id(self) -> u8 {
        ((self.raw & portsc::SPEED_MASK) >> portsc::SPEED_SHIFT) as u8
    }
    /// Speed with the default PSI mapping.
    pub const fn speed(self) -> Option<Speed> {
        Speed::from_psi(self.speed_id())
    }
    /// Pending change bits.
    pub const fn changes(self) -> u32 {
        self.raw & portsc::CHANGE_BITS
    }
}
