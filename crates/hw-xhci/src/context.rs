//! Device, input and endpoint contexts (xHCI §6.2).
//!
//! Every context entry is 32 bytes, or 64 bytes when HCCPARAMS1.CSZ = 1
//! (the upper 32 bytes are reserved). A device (output) context is the slot
//! context followed by 31 endpoint contexts indexed by Device Context Index
//! (DCI); an input context prepends the input control context.

use crate::port::Speed;
use crate::{DescError, DmaMemory};

/// Endpoint types (xHCI §6.2.3, Table 6-9).
pub mod ep_type {
    /// Isoch OUT.
    pub const ISOCH_OUT: u8 = 1;
    /// Bulk OUT.
    pub const BULK_OUT: u8 = 2;
    /// Interrupt OUT.
    pub const INTERRUPT_OUT: u8 = 3;
    /// Control (bidirectional).
    pub const CONTROL: u8 = 4;
    /// Isoch IN.
    pub const ISOCH_IN: u8 = 5;
    /// Bulk IN.
    pub const BULK_IN: u8 = 6;
    /// Interrupt IN.
    pub const INTERRUPT_IN: u8 = 7;
}

/// Endpoint states (xHCI §6.2.3, Table 6-8).
pub mod ep_state {
    /// Disabled.
    pub const DISABLED: u8 = 0;
    /// Running.
    pub const RUNNING: u8 = 1;
    /// Halted.
    pub const HALTED: u8 = 2;
    /// Stopped.
    pub const STOPPED: u8 = 3;
    /// Error.
    pub const ERROR: u8 = 4;
}

/// Slot states (xHCI §6.2.2, Table 6-7).
pub mod slot_state {
    /// Disabled/Enabled.
    pub const DISABLED_ENABLED: u8 = 0;
    /// Default.
    pub const DEFAULT: u8 = 1;
    /// Addressed.
    pub const ADDRESSED: u8 = 2;
    /// Configured.
    pub const CONFIGURED: u8 = 3;
}

/// Context geometry for one controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    entry: usize,
}

impl Layout {
    /// Layout for 64-byte (`csz = true`) or 32-byte contexts.
    pub const fn new(csz: bool) -> Self {
        Self {
            entry: if csz { 64 } else { 32 },
        }
    }
    /// Size of one context entry.
    pub const fn entry(&self) -> usize {
        self.entry
    }
    /// Size of a device (output) context.
    pub const fn output_len(&self) -> usize {
        32 * self.entry
    }
    /// Size of an input context.
    pub const fn input_len(&self) -> usize {
        33 * self.entry
    }
    /// Offset of the slot context in an input context.
    pub const fn input_slot(&self) -> u64 {
        self.entry as u64
    }
    /// Offset of endpoint context `dci` in an input context.
    pub const fn input_ep(&self, dci: u8) -> u64 {
        (self.entry * (1 + dci as usize)) as u64
    }
    /// Offset of endpoint context `dci` in an output context.
    pub const fn output_ep(&self, dci: u8) -> u64 {
        (self.entry * dci as usize) as u64
    }
}

/// Device Context Index of endpoint `number` (0 = default control).
pub const fn dci(number: u8, dir_in: bool) -> u8 {
    if number == 0 {
        1
    } else {
        number * 2 + dir_in as u8
    }
}

/// Input control context (xHCI §6.2.5.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputControl {
    /// Drop Context flags (bits 2..=31 valid).
    pub drop: u32,
    /// Add Context flags.
    pub add: u32,
    /// Configuration Value.
    pub config_value: u8,
    /// Interface Number.
    pub interface: u8,
    /// Alternate Setting.
    pub alternate: u8,
}

impl InputControl {
    /// Encodes the 8 defined dwords.
    pub const fn encode(&self) -> [u32; 8] {
        [
            self.drop & !0b11,
            self.add,
            0,
            0,
            0,
            0,
            0,
            self.config_value as u32 | (self.interface as u32) << 8 | (self.alternate as u32) << 16,
        ]
    }
}

/// Slot context (xHCI §6.2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotContext {
    /// Route String (0 for root-port devices).
    pub route_string: u32,
    /// Speed (deprecated in 1.2 but still honoured by many controllers).
    pub speed: u8,
    /// Multi-TT.
    pub mtt: bool,
    /// Hub.
    pub hub: bool,
    /// Context Entries (index of the last valid endpoint context).
    pub context_entries: u8,
    /// Max Exit Latency.
    pub max_exit_latency: u16,
    /// Root Hub Port Number.
    pub root_port: u8,
    /// Number of Ports (hubs).
    pub num_ports: u8,
    /// Parent Hub Slot ID.
    pub tt_hub_slot: u8,
    /// Parent Port Number.
    pub tt_port: u8,
    /// TT Think Time.
    pub ttt: u8,
    /// Interrupter Target.
    pub interrupter: u16,
    /// USB Device Address (output only).
    pub usb_address: u8,
    /// Slot State (output only).
    pub slot_state: u8,
}

impl SlotContext {
    /// Encodes the four defined dwords.
    pub const fn encode(&self) -> [u32; 4] {
        [
            (self.route_string & 0xF_FFFF)
                | ((self.speed & 0xF) as u32) << 20
                | (self.mtt as u32) << 25
                | (self.hub as u32) << 26
                | ((self.context_entries & 0x1F) as u32) << 27,
            self.max_exit_latency as u32
                | (self.root_port as u32) << 16
                | (self.num_ports as u32) << 24,
            self.tt_hub_slot as u32
                | (self.tt_port as u32) << 8
                | ((self.ttt & 3) as u32) << 16
                | ((self.interrupter & 0x3FF) as u32) << 22,
            self.usb_address as u32 | ((self.slot_state & 0x1F) as u32) << 27,
        ]
    }

    /// Decodes the four defined dwords.
    pub const fn decode(d: [u32; 4]) -> Self {
        Self {
            route_string: d[0] & 0xF_FFFF,
            speed: ((d[0] >> 20) & 0xF) as u8,
            mtt: d[0] & (1 << 25) != 0,
            hub: d[0] & (1 << 26) != 0,
            context_entries: (d[0] >> 27) as u8,
            max_exit_latency: d[1] as u16,
            root_port: (d[1] >> 16) as u8,
            num_ports: (d[1] >> 24) as u8,
            tt_hub_slot: d[2] as u8,
            tt_port: (d[2] >> 8) as u8,
            ttt: ((d[2] >> 16) & 3) as u8,
            interrupter: ((d[2] >> 22) & 0x3FF) as u16,
            usb_address: d[3] as u8,
            slot_state: (d[3] >> 27) as u8,
        }
    }
}

/// Endpoint context (xHCI §6.2.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndpointContext {
    /// Endpoint State (output only).
    pub state: u8,
    /// Mult.
    pub mult: u8,
    /// MaxPStreams.
    pub max_pstreams: u8,
    /// Linear Stream Array.
    pub lsa: bool,
    /// Interval (2^Interval × 125 µs).
    pub interval: u8,
    /// Max Endpoint Service Time Interval Payload (24 bits, Hi:Lo).
    pub max_esit_payload: u32,
    /// Error Count.
    pub cerr: u8,
    /// Endpoint Type.
    pub ep_type: u8,
    /// Host Initiate Disable.
    pub hid: bool,
    /// Max Burst Size.
    pub max_burst: u8,
    /// Max Packet Size.
    pub max_packet: u16,
    /// TR Dequeue Pointer (16-byte aligned).
    pub dequeue: u64,
    /// Dequeue Cycle State.
    pub dcs: bool,
    /// Average TRB Length.
    pub avg_trb_len: u16,
}

impl EndpointContext {
    /// Encodes the five defined dwords.
    pub const fn encode(&self) -> [u32; 5] {
        let deq = (self.dequeue & !0xF) | self.dcs as u64;
        [
            (self.state & 7) as u32
                | ((self.mult & 3) as u32) << 8
                | ((self.max_pstreams & 0x1F) as u32) << 10
                | (self.lsa as u32) << 15
                | (self.interval as u32) << 16
                | ((self.max_esit_payload >> 16) & 0xFF) << 24,
            ((self.cerr & 3) as u32) << 1
                | ((self.ep_type & 7) as u32) << 3
                | (self.hid as u32) << 7
                | (self.max_burst as u32) << 8
                | (self.max_packet as u32) << 16,
            deq as u32,
            (deq >> 32) as u32,
            self.avg_trb_len as u32 | (self.max_esit_payload & 0xFFFF) << 16,
        ]
    }

    /// Decodes the five defined dwords.
    pub const fn decode(d: [u32; 5]) -> Self {
        let deq = d[2] as u64 | (d[3] as u64) << 32;
        Self {
            state: (d[0] & 7) as u8,
            mult: ((d[0] >> 8) & 3) as u8,
            max_pstreams: ((d[0] >> 10) & 0x1F) as u8,
            lsa: d[0] & (1 << 15) != 0,
            interval: (d[0] >> 16) as u8,
            max_esit_payload: (d[0] >> 24) << 16 | d[4] >> 16,
            cerr: ((d[1] >> 1) & 3) as u8,
            ep_type: ((d[1] >> 3) & 7) as u8,
            hid: d[1] & (1 << 7) != 0,
            max_burst: (d[1] >> 8) as u8,
            max_packet: (d[1] >> 16) as u16,
            dequeue: deq & !0xF,
            dcs: deq & 1 != 0,
            avg_trb_len: d[4] as u16,
        }
    }
}

/// Endpoint Context Interval for a periodic endpoint from bInterval
/// (xHCI §6.2.3.6): FS/LS interrupt bInterval is in 1 ms frames
/// (1..=255) and is rounded down to a power of two of 125 µs units within
/// 3..=10; HS/SS bInterval is an exponent (1..=16) and Interval =
/// bInterval - 1.
pub fn interrupt_interval(speed: Speed, b_interval: u8) -> Result<u8, DescError> {
    match speed {
        Speed::Low | Speed::Full => {
            if b_interval == 0 {
                return Err(DescError::BadInterval);
            }
            let micro = u32::from(b_interval) * 8;
            let log2 = 31 - micro.leading_zeros();
            Ok(log2.clamp(3, 10) as u8)
        }
        Speed::High | Speed::Super | Speed::SuperPlus => {
            if !(1..=16).contains(&b_interval) {
                return Err(DescError::BadInterval);
            }
            Ok(b_interval - 1)
        }
    }
}

/// Writes `dwords` at `pa`.
pub fn write_dwords<D: DmaMemory + ?Sized>(mem: &mut D, pa: u64, dwords: &[u32]) {
    for (i, d) in dwords.iter().enumerate() {
        mem.write_u32(pa + 4 * i as u64, *d);
    }
}

/// Reads `N` dwords at `pa`.
pub fn read_dwords<D: DmaMemory + ?Sized, const N: usize>(mem: &mut D, pa: u64) -> [u32; N] {
    let mut out = [0u32; N];
    for (i, d) in out.iter_mut().enumerate() {
        *d = mem.read_u32(pa + 4 * i as u64);
    }
    out
}
