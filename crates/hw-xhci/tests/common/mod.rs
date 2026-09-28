//! Behavioural xHCI controller model shared by the integration tests.
//!
//! The model decodes registers, TRBs and contexts itself from the xHCI 1.2
//! layouts (it does not use the crate's encoders), executes commands and
//! control/interrupt transfers against model USB devices and posts events
//! with the producer cycle bit. It is strict: things a real controller
//! would misinterpret are recorded in `violations` (PED written as 1,
//! run before the data structures are set up, HCRST while running, CRCR
//! pointer writes while the ring runs, bad input contexts, doorbells while
//! halted, DMA outside allocated memory, bad or double frees, ...). Tests
//! assert the list is empty.
//!
//! Faults: BIOS that never releases ownership, HCH/HCRST/CNR that never
//! settle, commands that hang (optionally with an abort that never
//! finishes) or fail, host system error, stalls, disconnects and malformed
//! descriptors.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};

use hw_xhci::controller::Config;
use hw_xhci::{Clock, Controller, DmaAlloc, DmaMemory, Mmio};

pub const MMIO_LEN: u32 = 0x1_0000;
const CAPLEN: u32 = 0x20;
const OP: u32 = CAPLEN;
const PORTS: u32 = OP + 0x400;
const RTS: u32 = 0x1000;
const IR0: u32 = RTS + 0x20;
const DB: u32 = 0x2000;
const XECP_LEGACY: u32 = 0x3000;
const XECP_USB2: u32 = 0x3010;
const XECP_USB3: u32 = 0x3030;

pub const MEM_BASE: u64 = 0x1000_0000;
pub const MEM_SIZE: usize = 8 << 20;
/// Test-owned buffers (interrupt IN data) live above this address; the
/// driver's allocator never hands it out.
pub const USER_BASE: u64 = MEM_BASE + (6 << 20);

// USBCMD / USBSTS
const RS: u32 = 1;
const HCRST: u32 = 2;
const INTE: u32 = 4;
const HCH: u32 = 1;
const HSE: u32 = 1 << 2;
const EINT: u32 = 1 << 3;
const PCD: u32 = 1 << 4;
const CNR: u32 = 1 << 11;
const HCE: u32 = 1 << 12;
// PORTSC
const CCS: u32 = 1;
const PED: u32 = 1 << 1;
const PR: u32 = 1 << 4;
const PP: u32 = 1 << 9;
const CSC: u32 = 1 << 17;
const PEC: u32 = 1 << 18;
const WRC: u32 = 1 << 19;
const PRC: u32 = 1 << 21;
const PLC: u32 = 1 << 22;
const WPR: u32 = 1 << 31;
const CHANGES: u32 = CSC | PEC | WRC | (1 << 20) | PRC | PLC | (1 << 23);
// Legacy support
const BIOS_OWNED: u32 = 1 << 16;
const OS_OWNED: u32 = 1 << 24;
// TRB types
pub mod ty {
    pub const NORMAL: u32 = 1;
    pub const SETUP: u32 = 2;
    pub const DATA: u32 = 3;
    pub const STATUS: u32 = 4;
    pub const LINK: u32 = 6;
    pub const ENABLE_SLOT: u32 = 9;
    pub const DISABLE_SLOT: u32 = 10;
    pub const ADDRESS_DEVICE: u32 = 11;
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    pub const EVALUATE_CONTEXT: u32 = 13;
    pub const RESET_ENDPOINT: u32 = 14;
    pub const STOP_ENDPOINT: u32 = 15;
    pub const SET_TR_DEQUEUE: u32 = 16;
    pub const NOOP_COMMAND: u32 = 23;
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETION: u32 = 33;
    pub const PORT_STATUS_CHANGE: u32 = 34;
}
// Completion codes
pub mod cc {
    pub const SUCCESS: u32 = 1;
    pub const STALL: u32 = 6;
    pub const NO_SLOTS: u32 = 9;
    pub const SLOT_NOT_ENABLED: u32 = 11;
    pub const SHORT_PACKET: u32 = 13;
    pub const PARAMETER: u32 = 17;
    pub const CONTEXT_STATE: u32 = 19;
    pub const TRB_ERROR: u32 = 5;
    pub const RING_STOPPED: u32 = 24;
    pub const ABORTED: u32 = 25;
    pub const STOPPED: u32 = 26;
}
// Endpoint states
const EP_DISABLED: u8 = 0;
const EP_RUNNING: u8 = 1;
const EP_HALTED: u8 = 2;
const EP_STOPPED: u8 = 3;

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub max_slots: u8,
    pub csz: bool,
    pub ac64: bool,
    pub scratchpads: u16,
    pub legacy: bool,
    pub bios_owned: bool,
    /// BIOS releases ownership this long after the OS request; `None` =
    /// never.
    pub bios_release_us: Option<u64>,
    pub initially_running: bool,
    pub halt_us: Option<u64>,
    pub reset_us: Option<u64>,
    pub cnr_us: Option<u64>,
    pub run_us: u64,
    pub tick_us: u64,
    pub cmd_latency_us: u64,
    pub port_reset_us: u64,
    pub usb3_train_us: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            max_slots: 16,
            csz: false,
            ac64: true,
            scratchpads: 2,
            legacy: true,
            bios_owned: true,
            bios_release_us: Some(2_000),
            initially_running: false,
            halt_us: Some(100),
            reset_us: Some(500),
            cnr_us: Some(300),
            run_us: 50,
            tick_us: 5,
            cmd_latency_us: 20,
            port_reset_us: 10_000,
            usb3_train_us: 2_000,
        }
    }
}

/// What the model does with a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdAction {
    Normal,
    /// Never completes on its own; an abort completes it as aborted.
    Hang,
    /// Hangs and a Command Abort never finishes either.
    HangForever,
    /// Completes with this completion code and no effect.
    Fail(u32),
}

pub type CmdHook = Box<dyn FnMut(u32) -> CmdAction>;

/// A model USB device.
#[derive(Clone, Debug)]
pub struct UsbDevice {
    /// Default PSI: 1 full, 2 low, 3 high, 4 super.
    pub psi: u32,
    pub device_desc: Vec<u8>,
    pub config_desc: Vec<u8>,
    /// Reports returned by the interrupt IN endpoint, in order.
    pub reports: VecDeque<Vec<u8>>,
    /// Stall the next interrupt IN transfer.
    pub stall_interrupt: bool,
    /// (bRequest, wValue) pairs answered with STALL on EP0.
    pub stall_requests: Vec<(u8, u16)>,
    pub address: u8,
    pub configuration: u8,
    pub halt_cleared: u32,
    pub setups: Vec<[u8; 8]>,
}

impl UsbDevice {
    /// Full-speed boot keyboard: EP0 8 bytes, interrupt IN 0x81, 8 bytes,
    /// bInterval 10.
    pub fn keyboard() -> Self {
        let device = vec![
            18, 1, 0x00, 0x02, 0, 0, 0, 8, 0x6D, 0x04, 0x1C, 0xC3, 0x10, 0x01, 1, 2, 0, 1,
        ];
        let mut config = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50];
        config.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        config.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
        config.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        let total = config.len() as u16;
        config[2..4].copy_from_slice(&total.to_le_bytes());
        Self::new(1, device, config)
    }

    /// Full-speed device whose bMaxPacketSize0 is 64, so the driver must
    /// Evaluate Context after reading the first 8 bytes.
    pub fn full_speed_mps64() -> Self {
        let mut d = Self::keyboard();
        d.device_desc[7] = 64;
        d
    }

    /// SuperSpeed boot keyboard-like device: EP0 512 bytes (exponent 9),
    /// interrupt IN with a SuperSpeed endpoint companion.
    pub fn superspeed() -> Self {
        let device = vec![
            18, 1, 0x20, 0x03, 0, 0, 0, 9, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01, 0, 0, 0, 1,
        ];
        let mut config = vec![9, 2, 0, 0, 1, 1, 0, 0x80, 25];
        config.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        config.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
        config.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 4]);
        config.extend_from_slice(&[6, 0x30, 0, 0, 8, 0]);
        let total = config.len() as u16;
        config[2..4].copy_from_slice(&total.to_le_bytes());
        Self::new(4, device, config)
    }

    fn new(psi: u32, device_desc: Vec<u8>, config_desc: Vec<u8>) -> Self {
        Self {
            psi,
            device_desc,
            config_desc,
            reports: VecDeque::new(),
            stall_interrupt: false,
            stall_requests: Vec::new(),
            address: 0,
            configuration: 0,
            halt_cleared: 0,
            setups: Vec::new(),
        }
    }

    /// Answers a control request: `Ok(data)` for IN, `Ok(empty)` for
    /// no-data/OUT, `Err(())` for STALL.
    fn control(&mut self, setup: [u8; 8], out: &[u8]) -> Result<Vec<u8>, ()> {
        self.setups.push(setup);
        let (req, value) = (setup[1], u16::from_le_bytes([setup[2], setup[3]]));
        let len = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        if self.stall_requests.contains(&(req, value)) {
            return Err(());
        }
        let _ = out;
        match (setup[0], req) {
            (0x80, 6) => {
                let data = match value >> 8 {
                    1 => self.device_desc.clone(),
                    2 => self.config_desc.clone(),
                    _ => return Err(()),
                };
                Ok(data[..data.len().min(len)].to_vec())
            }
            (0x00, 9) => {
                self.configuration = value as u8;
                Ok(Vec::new())
            }
            (0x02, 1) if value == 0 => {
                self.halt_cleared += 1;
                Ok(Vec::new())
            }
            (0x21, _) => Ok(Vec::new()),
            _ => Err(()),
        }
    }
}

struct Port {
    usb3: bool,
    ccs: bool,
    ped: bool,
    pr: bool,
    pls: u32,
    changes: u32,
    reset_done: Option<u64>,
    warm: bool,
    train_done: Option<u64>,
    device: Option<UsbDevice>,
    psi: u32,
}

impl Port {
    fn portsc(&self) -> u32 {
        let mut v = PP | (self.pls << 5) | self.changes;
        if self.ccs {
            v |= CCS | (self.psi << 10);
        }
        if self.ped {
            v |= PED;
        }
        if self.pr {
            v |= if self.warm { WPR } else { PR };
        }
        v
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Ep {
    state: u8,
    ep_type: u32,
    mps: u32,
    deq: u64,
    cycle: bool,
}

#[derive(Clone, Debug, Default)]
struct Slot {
    enabled: bool,
    port: u8,
    state: u8, // 0 enabled, 1 default, 2 addressed, 3 configured
    eps: [Ep; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stuck {
    None,
    Hang(u64),
    Forever(u64),
}

#[derive(Default)]
pub struct Stats {
    pub commands: Vec<u32>,
    pub doorbells: u64,
    pub events: u64,
    pub aborts: u64,
    pub port_resets: u64,
    pub warm_resets: u64,
    pub hc_resets: u64,
    pub max_event_backlog: usize,
}

pub struct Hw {
    pub cfg: ModelConfig,
    pub now: u64,
    mem: Vec<u8>,
    allocs: BTreeMap<u64, usize>,
    pub violations: Vec<String>,
    pub stats: Stats,
    pub cmd_hook: Option<CmdHook>,
    // registers
    usbcmd: u32,
    usbsts: u32,
    halt_at: Option<u64>,
    run_at: Option<u64>,
    reset_at: Option<u64>,
    cnr_at: Option<u64>,
    crcr_ptr: u64,
    ccs: bool,
    crr: bool,
    stuck: Stuck,
    dcbaap: u64,
    config: u32,
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    seg: u64,
    seg_size: u32,
    evt_enq: u32,
    pcs: bool,
    backlog: VecDeque<[u32; 4]>,
    legsup: u32,
    legctl: u32,
    bios_release_at: Option<u64>,
    cmd_kick_at: Option<u64>,
    ports: Vec<Port>,
    slots: Vec<Slot>,
    configured: bool,
    // Low halves of 64-bit registers written as two dwords (low first).
    crcr_lo: Option<u32>,
    erstba_lo: Option<u32>,
    erdp_lo: Option<u32>,
}

impl Hw {
    pub fn new(cfg: ModelConfig) -> Self {
        let mut ports = Vec::new();
        for i in 0..4 {
            ports.push(Port {
                usb3: i >= 2,
                ccs: false,
                ped: false,
                pr: false,
                pls: if i >= 2 { 5 } else { 7 },
                changes: 0,
                reset_done: None,
                warm: false,
                train_done: None,
                device: None,
                psi: 0,
            });
        }
        let mut legsup = 1 | ((XECP_USB2 - XECP_LEGACY) / 4) << 8;
        if cfg.bios_owned {
            legsup |= BIOS_OWNED;
        }
        let running = cfg.initially_running;
        Self {
            now: 0,
            mem: vec![0xA5; MEM_SIZE],
            allocs: BTreeMap::new(),
            violations: Vec::new(),
            stats: Stats::default(),
            cmd_hook: None,
            usbcmd: if running { RS } else { 0 },
            usbsts: if running { 0 } else { HCH },
            halt_at: None,
            run_at: None,
            reset_at: None,
            cnr_at: None,
            crcr_ptr: 0,
            ccs: false,
            crr: false,
            stuck: Stuck::None,
            dcbaap: 0,
            config: 0,
            iman: 0,
            imod: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            seg: 0,
            seg_size: 0,
            evt_enq: 0,
            pcs: true,
            backlog: VecDeque::new(),
            legsup,
            legctl: 1 | 1 << 4 | 1 << 29,
            bios_release_at: None,
            cmd_kick_at: None,
            ports,
            slots: vec![Slot::default(); 256],
            configured: false,
            crcr_lo: None,
            erstba_lo: None,
            erdp_lo: None,
            cfg,
        }
    }

    pub fn assert_clean(&self) {
        assert!(
            self.violations.is_empty(),
            "model violations: {:#?}",
            self.violations
        );
    }

    fn violation(&mut self, msg: String) {
        self.violations.push(msg);
    }

    /// Bytes of DMA memory the driver has not freed.
    pub fn allocated(&self) -> usize {
        self.allocs.values().sum()
    }

    pub fn running(&self) -> bool {
        self.usbcmd & RS != 0 && self.usbsts & HCH == 0
    }

    pub fn legacy_ctl(&self) -> u32 {
        self.legctl
    }

    pub fn legacy_sup(&self) -> u32 {
        self.legsup
    }

    pub fn portsc(&self, port: u8) -> u32 {
        self.ports[usize::from(port) - 1].portsc()
    }

    pub fn device(&self, port: u8) -> Option<&UsbDevice> {
        self.ports[usize::from(port) - 1].device.as_ref()
    }

    pub fn device_mut(&mut self, port: u8) -> Option<&mut UsbDevice> {
        self.ports[usize::from(port) - 1].device.as_mut()
    }

    pub fn slot_enabled(&self, slot: u8) -> bool {
        self.slots[usize::from(slot)].enabled
    }

    pub fn ep_state(&self, slot: u8, dci: u8) -> u8 {
        self.slots[usize::from(slot)].eps[usize::from(dci)].state
    }

    // ---- faults and stimuli ----------------------------------------------

    /// Connects `dev` to `port`; a USB3 port trains its link on its own.
    pub fn connect(&mut self, port: u8, dev: UsbDevice) {
        let now = self.now;
        let train = self.cfg.usb3_train_us;
        let p = &mut self.ports[usize::from(port) - 1];
        p.psi = dev.psi;
        p.device = Some(dev);
        p.ccs = true;
        p.ped = false;
        p.changes |= CSC;
        if p.usb3 {
            p.pls = 7; // Polling
            p.train_done = Some(now + train);
        } else {
            p.pls = 7;
        }
        self.port_event(port);
    }

    pub fn disconnect(&mut self, port: u8) {
        let p = &mut self.ports[usize::from(port) - 1];
        p.device = None;
        p.ccs = false;
        if p.ped {
            p.changes |= PEC;
        }
        p.ped = false;
        p.pr = false;
        p.reset_done = None;
        p.train_done = None;
        p.pls = if p.usb3 { 5 } else { 7 };
        p.changes |= CSC;
        self.port_event(port);
    }

    /// Puts a USB3 port's link into SS.Inactive (needs a warm reset).
    pub fn usb3_inactive(&mut self, port: u8) {
        let p = &mut self.ports[usize::from(port) - 1];
        p.ped = false;
        p.pls = 6;
        p.train_done = None;
    }

    pub fn push_report(&mut self, port: u8, report: &[u8]) {
        if let Some(d) = self.device_mut(port) {
            d.reports.push_back(report.to_vec());
        }
        self.kick_all_interrupt();
    }

    pub fn trigger_hse(&mut self) {
        self.usbsts |= HSE | HCH;
        self.usbcmd &= !RS;
    }

    pub fn trigger_hce(&mut self) {
        self.usbsts |= HCE | HCH;
        self.usbcmd &= !RS;
    }

    // ---- memory ------------------------------------------------------------

    fn range(&mut self, pa: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let off = pa.checked_sub(MEM_BASE)? as usize;
        let end = off.checked_add(len)?;
        if end > MEM_SIZE {
            return None;
        }
        Some(off..end)
    }

    pub fn mem_read(&mut self, pa: u64, buf: &mut [u8]) {
        match self.range(pa, buf.len()) {
            Some(r) => buf.copy_from_slice(&self.mem[r]),
            None => {
                self.violation(format!("DMA read outside memory at {pa:#x}"));
                buf.fill(0);
            }
        }
    }

    pub fn mem_write(&mut self, pa: u64, data: &[u8]) {
        match self.range(pa, data.len()) {
            Some(r) => self.mem[r].copy_from_slice(data),
            None => self.violation(format!("DMA write outside memory at {pa:#x}")),
        }
    }

    fn r32(&mut self, pa: u64) -> u32 {
        let mut b = [0; 4];
        self.mem_read(pa, &mut b);
        u32::from_le_bytes(b)
    }

    fn r64(&mut self, pa: u64) -> u64 {
        u64::from(self.r32(pa)) | u64::from(self.r32(pa + 4)) << 32
    }

    fn w32(&mut self, pa: u64, v: u32) {
        self.mem_write(pa, &v.to_le_bytes());
    }

    fn read_trb(&mut self, pa: u64) -> [u32; 4] {
        [
            self.r32(pa),
            self.r32(pa + 4),
            self.r32(pa + 8),
            self.r32(pa + 12),
        ]
    }

    fn ctx_size(&self) -> u64 {
        if self.cfg.csz {
            64
        } else {
            32
        }
    }

    // ---- time ---------------------------------------------------------------

    fn process(&mut self) {
        let now = self.now;
        if self.bios_release_at.is_some_and(|t| now >= t) {
            self.legsup &= !BIOS_OWNED;
            self.bios_release_at = None;
        }
        if self.halt_at.is_some_and(|t| now >= t) {
            self.usbsts |= HCH;
            self.halt_at = None;
            self.crr = false;
        }
        if self.reset_at.is_some_and(|t| now >= t) {
            self.usbcmd &= !HCRST;
            self.reset_at = None;
        }
        if self.cnr_at.is_some_and(|t| now >= t) {
            self.usbsts &= !CNR;
            self.cnr_at = None;
        }
        if self.run_at.is_some_and(|t| now >= t) {
            self.usbsts &= !HCH;
            self.run_at = None;
        }
        for i in 0..self.ports.len() {
            let port = i as u8 + 1;
            let p = &mut self.ports[i];
            if p.reset_done.is_some_and(|t| now >= t) {
                p.reset_done = None;
                p.pr = false;
                if p.ccs {
                    p.ped = true;
                    p.pls = 0;
                }
                p.changes |= if p.warm { WRC | PRC } else { PRC };
                p.warm = false;
                self.port_event(port);
            }
            let p = &mut self.ports[i];
            if p.train_done.is_some_and(|t| now >= t) {
                p.train_done = None;
                if p.ccs {
                    p.ped = true;
                    p.pls = 0;
                    p.changes |= PLC;
                }
                self.port_event(port);
            }
        }
        if self.cmd_kick_at.is_some_and(|t| now >= t) {
            self.cmd_kick_at = None;
            self.run_command_ring();
        }
    }

    // ---- events -------------------------------------------------------------

    fn post(&mut self, ev: [u32; 4]) {
        if !self.running() || self.seg == 0 {
            return;
        }
        self.backlog.push_back(ev);
        self.stats.max_event_backlog = self.stats.max_event_backlog.max(self.backlog.len());
        self.flush_events();
    }

    fn flush_events(&mut self) {
        while let Some(ev) = self.backlog.front().copied() {
            let deq = ((self.erdp & !0xF) - self.seg) / 16;
            let next = (self.evt_enq + 1) % self.seg_size;
            if u64::from(next) == deq {
                return; // full: wait for the driver to advance ERDP
            }
            self.backlog.pop_front();
            let pa = self.seg + u64::from(self.evt_enq) * 16;
            let mut ev = ev;
            ev[3] = (ev[3] & !1) | u32::from(self.pcs);
            for (i, d) in ev.iter().enumerate() {
                self.w32(pa + 4 * i as u64, *d);
            }
            self.evt_enq = next;
            if next == 0 {
                self.pcs = !self.pcs;
            }
            self.stats.events += 1;
            self.iman |= 1;
            self.usbsts |= EINT;
        }
    }

    fn port_event(&mut self, port: u8) {
        self.usbsts |= PCD;
        self.post([
            u32::from(port) << 24,
            0,
            cc::SUCCESS << 24,
            ty::PORT_STATUS_CHANGE << 10,
        ]);
    }

    fn command_event(&mut self, trb: u64, code: u32, slot: u8) {
        self.post([
            trb as u32,
            (trb >> 32) as u32,
            code << 24,
            ty::COMMAND_COMPLETION << 10 | u32::from(slot) << 24,
        ]);
    }

    fn transfer_event(&mut self, trb: u64, code: u32, residual: u32, slot: u8, dci: u8) {
        self.post([
            trb as u32,
            (trb >> 32) as u32,
            code << 24 | (residual & 0xFF_FFFF),
            ty::TRANSFER_EVENT << 10 | u32::from(dci) << 16 | u32::from(slot) << 24,
        ]);
    }

    // ---- command ring -------------------------------------------------------

    fn run_command_ring(&mut self) {
        if !self.running() || self.stuck != Stuck::None {
            return;
        }
        self.crr = true;
        for _ in 0..10_000 {
            let pa = self.crcr_ptr;
            let t = self.read_trb(pa);
            if (t[3] & 1 != 0) != self.ccs {
                return;
            }
            let kind = (t[3] >> 10) & 0x3F;
            if kind == ty::LINK {
                self.crcr_ptr = (u64::from(t[0]) | u64::from(t[1]) << 32) & !0xF;
                if t[3] & 2 != 0 {
                    self.ccs = !self.ccs;
                }
                continue;
            }
            self.stats.commands.push(kind);
            let action = match self.cmd_hook.as_mut() {
                Some(h) => h(kind),
                None => CmdAction::Normal,
            };
            match action {
                CmdAction::Hang => {
                    self.stuck = Stuck::Hang(pa);
                    return;
                }
                CmdAction::HangForever => {
                    self.stuck = Stuck::Forever(pa);
                    return;
                }
                CmdAction::Fail(code) => {
                    self.crcr_ptr = pa + 16;
                    self.command_event(pa, code, 0);
                }
                CmdAction::Normal => {
                    self.crcr_ptr = pa + 16;
                    let (code, slot) = self.execute(kind, t);
                    self.command_event(pa, code, slot);
                }
            }
        }
        self.violation("command ring never ends".into());
    }

    fn abort_commands(&mut self) {
        self.stats.aborts += 1;
        match self.stuck {
            Stuck::Forever(_) => {}
            Stuck::Hang(pa) => {
                self.stuck = Stuck::None;
                self.crcr_ptr = pa + 16;
                self.command_event(pa, cc::ABORTED, 0);
                self.command_event(self.crcr_ptr, cc::RING_STOPPED, 0);
                self.crr = false;
            }
            Stuck::None => {
                self.command_event(self.crcr_ptr, cc::RING_STOPPED, 0);
                self.crr = false;
            }
        }
    }

    fn input_ctx(&mut self, t: [u32; 4]) -> u64 {
        let pa = (u64::from(t[0]) | u64::from(t[1]) << 32) & !0xF;
        if !pa.is_multiple_of(16) || pa == 0 {
            self.violation(format!("bad input context pointer {pa:#x}"));
        }
        pa
    }

    fn output_ctx(&mut self, slot: u8) -> u64 {
        let pa = self.r64(self.dcbaap + 8 * u64::from(slot));
        if pa == 0 || !pa.is_multiple_of(64) {
            self.violation(format!("DCBAA[{slot}] = {pa:#x}"));
        }
        pa
    }

    fn ep_ctx_ok(&mut self, dw: [u32; 5], what: &str) -> bool {
        let ep_type = (dw[1] >> 3) & 7;
        let mps = dw[1] >> 16;
        let deq = (u64::from(dw[2]) | u64::from(dw[3]) << 32) & !0xF;
        if ep_type == 0 || mps == 0 || deq == 0 || (dw[1] >> 1) & 3 == 0 {
            self.violation(format!("{what}: bad endpoint context {dw:x?}"));
            return false;
        }
        true
    }

    fn read_dwords<const N: usize>(&mut self, pa: u64) -> [u32; N] {
        let mut out = [0u32; N];
        for (i, d) in out.iter_mut().enumerate() {
            *d = self.r32(pa + 4 * i as u64);
        }
        out
    }

    fn execute(&mut self, kind: u32, t: [u32; 4]) -> (u32, u8) {
        let slot = (t[3] >> 24) as u8;
        let dci = ((t[3] >> 16) & 0x1F) as u8;
        let max = (self.config & 0xFF) as u8;
        let csz = self.ctx_size();
        match kind {
            ty::NOOP_COMMAND => (cc::SUCCESS, 0),
            ty::ENABLE_SLOT => match (1..=max).find(|&s| !self.slots[usize::from(s)].enabled) {
                Some(s) => {
                    self.slots[usize::from(s)] = Slot {
                        enabled: true,
                        ..Slot::default()
                    };
                    (cc::SUCCESS, s)
                }
                None => (cc::NO_SLOTS, 0),
            },
            _ if slot == 0 || slot > max || !self.slots[usize::from(slot)].enabled => {
                (cc::SLOT_NOT_ENABLED, slot)
            }
            ty::DISABLE_SLOT => {
                self.slots[usize::from(slot)] = Slot::default();
                (cc::SUCCESS, slot)
            }
            ty::ADDRESS_DEVICE => {
                let bsr = t[3] & (1 << 9) != 0;
                if self.slots[usize::from(slot)].state > 1 {
                    return (cc::CONTEXT_STATE, slot);
                }
                let ic = self.input_ctx(t);
                let icc: [u32; 2] = self.read_dwords(ic);
                if icc[0] != 0 || icc[1] != 0b11 {
                    self.violation(format!("Address Device: input control {icc:x?}"));
                    return (cc::PARAMETER, slot);
                }
                let sc: [u32; 4] = self.read_dwords(ic + csz);
                let port = (sc[1] >> 16) as u8;
                let speed = (sc[0] >> 20) & 0xF;
                let ok_port = port >= 1 && usize::from(port) <= self.ports.len();
                if !ok_port || self.ports[usize::from(port) - 1].psi != speed || sc[0] >> 27 != 1 {
                    self.violation(format!("Address Device: slot context {sc:x?}"));
                    return (cc::PARAMETER, slot);
                }
                let ep: [u32; 5] = self.read_dwords(ic + 2 * csz);
                if !self.ep_ctx_ok(ep, "Address Device EP0") || (ep[1] >> 3) & 7 != 4 {
                    return (cc::PARAMETER, slot);
                }
                let out = self.output_ctx(slot);
                let s = &mut self.slots[usize::from(slot)];
                s.port = port;
                s.state = if bsr { 1 } else { 2 };
                s.eps[1] = Ep {
                    state: EP_RUNNING,
                    ep_type: 4,
                    mps: ep[1] >> 16,
                    deq: (u64::from(ep[2]) | u64::from(ep[3]) << 32) & !0xF,
                    cycle: ep[2] & 1 != 0,
                };
                if !bsr {
                    if let Some(d) = self.ports[usize::from(port) - 1].device.as_mut() {
                        d.address = slot;
                    }
                }
                let state = u32::from(self.slots[usize::from(slot)].state);
                let addr = if bsr { 0 } else { u32::from(slot) };
                for (i, d) in sc.iter().enumerate() {
                    let v = if i == 3 { addr | state << 27 } else { *d };
                    self.w32(out + 4 * i as u64, v);
                }
                self.write_out_ep(slot, 1);
                (cc::SUCCESS, slot)
            }
            ty::EVALUATE_CONTEXT => {
                let ic = self.input_ctx(t);
                let icc: [u32; 2] = self.read_dwords(ic);
                if icc[1] & !0b11 != 0 || icc[1] & 0b10 == 0 {
                    self.violation(format!("Evaluate Context: input control {icc:x?}"));
                    return (cc::PARAMETER, slot);
                }
                let ep: [u32; 5] = self.read_dwords(ic + 2 * csz);
                self.slots[usize::from(slot)].eps[1].mps = ep[1] >> 16;
                self.write_out_ep(slot, 1);
                (cc::SUCCESS, slot)
            }
            ty::CONFIGURE_ENDPOINT => {
                if self.slots[usize::from(slot)].state < 2 {
                    return (cc::CONTEXT_STATE, slot);
                }
                let ic = self.input_ctx(t);
                let icc: [u32; 8] = self.read_dwords(ic);
                if icc[0] & 0b11 != 0 || icc[1] & 0b10 != 0 || icc[1] & 1 == 0 {
                    self.violation(format!("Configure Endpoint: input control {icc:x?}"));
                    return (cc::PARAMETER, slot);
                }
                for d in 2..32u8 {
                    if icc[0] & (1 << d) != 0 {
                        self.slots[usize::from(slot)].eps[usize::from(d)] = Ep::default();
                        self.write_out_ep(slot, d);
                    }
                }
                for d in 2..32u8 {
                    if icc[1] & (1 << d) == 0 {
                        continue;
                    }
                    let ep: [u32; 5] = self.read_dwords(ic + (u64::from(d) + 1) * csz);
                    if !self.ep_ctx_ok(ep, "Configure Endpoint") {
                        return (cc::PARAMETER, slot);
                    }
                    let ep_type = (ep[1] >> 3) & 7;
                    let dir_in = d % 2 == 1;
                    if (ep_type >= 5) != dir_in || ep[0] >> 16 & 0xFF == 0 && ep_type == 7 {
                        self.violation(format!("Configure Endpoint: dci {d} context {ep:x?}"));
                    }
                    self.slots[usize::from(slot)].eps[usize::from(d)] = Ep {
                        state: EP_RUNNING,
                        ep_type,
                        mps: ep[1] >> 16,
                        deq: (u64::from(ep[2]) | u64::from(ep[3]) << 32) & !0xF,
                        cycle: ep[2] & 1 != 0,
                    };
                    self.write_out_ep(slot, d);
                }
                let s = &mut self.slots[usize::from(slot)];
                s.state = if s.eps[2..].iter().any(|e| e.state != EP_DISABLED) {
                    3
                } else {
                    2
                };
                (cc::SUCCESS, slot)
            }
            ty::RESET_ENDPOINT => {
                let e = &mut self.slots[usize::from(slot)].eps[usize::from(dci)];
                if e.state != EP_HALTED {
                    return (cc::CONTEXT_STATE, slot);
                }
                e.state = EP_STOPPED;
                self.write_out_ep(slot, dci);
                (cc::SUCCESS, slot)
            }
            ty::STOP_ENDPOINT => {
                let e = &mut self.slots[usize::from(slot)].eps[usize::from(dci)];
                if e.state != EP_RUNNING {
                    return (cc::CONTEXT_STATE, slot);
                }
                e.state = EP_STOPPED;
                let deq = e.deq;
                self.write_out_ep(slot, dci);
                self.transfer_event(deq, cc::STOPPED, 0, slot, dci);
                (cc::SUCCESS, slot)
            }
            ty::SET_TR_DEQUEUE => {
                let p = u64::from(t[0]) | u64::from(t[1]) << 32;
                let e = &mut self.slots[usize::from(slot)].eps[usize::from(dci)];
                if e.state != EP_STOPPED {
                    return (cc::CONTEXT_STATE, slot);
                }
                e.deq = p & !0xF;
                e.cycle = p & 1 != 0;
                self.write_out_ep(slot, dci);
                (cc::SUCCESS, slot)
            }
            _ => (cc::TRB_ERROR, slot),
        }
    }

    fn write_out_ep(&mut self, slot: u8, dci: u8) {
        let out = self.output_ctx(slot);
        let csz = self.ctx_size();
        let e = self.slots[usize::from(slot)].eps[usize::from(dci)];
        let pa = out + u64::from(dci) * csz;
        self.w32(pa, u32::from(e.state));
        self.w32(pa + 4, e.ep_type << 3 | e.mps << 16 | 3 << 1);
        self.w32(pa + 8, e.deq as u32 | u32::from(e.cycle));
        self.w32(pa + 12, (e.deq >> 32) as u32);
    }

    // ---- transfer rings -----------------------------------------------------

    fn kick_all_interrupt(&mut self) {
        for s in 1..=255u8 {
            if !self.slots[usize::from(s)].enabled {
                continue;
            }
            for d in 2..32u8 {
                if self.slots[usize::from(s)].eps[usize::from(d)].ep_type == 7 {
                    self.run_endpoint(s, d);
                }
            }
        }
    }

    /// Next TRB of an endpoint ring after following Link TRBs; `None` if
    /// the producer has not published it.
    fn next_trb(&mut self, slot: u8, dci: u8) -> Option<(u64, [u32; 4])> {
        for _ in 0..8 {
            let e = self.slots[usize::from(slot)].eps[usize::from(dci)];
            let t = self.read_trb(e.deq);
            if (t[3] & 1 != 0) != e.cycle {
                return None;
            }
            if (t[3] >> 10) & 0x3F == ty::LINK {
                let e = &mut self.slots[usize::from(slot)].eps[usize::from(dci)];
                e.deq = (u64::from(t[0]) | u64::from(t[1]) << 32) & !0xF;
                if t[3] & 2 != 0 {
                    e.cycle = !e.cycle;
                }
                continue;
            }
            return Some((e.deq, t));
        }
        self.violation("link TRB loop".into());
        None
    }

    fn advance(&mut self, slot: u8, dci: u8) {
        self.slots[usize::from(slot)].eps[usize::from(dci)].deq += 16;
    }

    fn run_endpoint(&mut self, slot: u8, dci: u8) {
        if !self.running() {
            return;
        }
        let port = self.slots[usize::from(slot)].port;
        for _ in 0..64 {
            if self.slots[usize::from(slot)].eps[usize::from(dci)].state != EP_RUNNING {
                return;
            }
            if port == 0 || self.ports[usize::from(port) - 1].device.is_none() {
                return; // disconnected: nothing answers
            }
            let done = if dci == 1 {
                self.control_td(slot, dci, port)
            } else {
                self.interrupt_td(slot, dci, port)
            };
            if !done {
                return;
            }
        }
    }

    fn halt(&mut self, slot: u8, dci: u8) {
        self.slots[usize::from(slot)].eps[usize::from(dci)].state = EP_HALTED;
        self.write_out_ep(slot, dci);
    }

    /// Runs one control TD; false when no complete TD is published.
    fn control_td(&mut self, slot: u8, dci: u8, port: u8) -> bool {
        let Some((_, setup)) = self.next_trb(slot, dci) else {
            return false;
        };
        let kind = (setup[3] >> 10) & 0x3F;
        if kind != ty::SETUP || setup[3] & (1 << 6) == 0 || setup[2] & 0x1_FFFF != 8 {
            self.violation(format!(
                "control TD does not start with an IDT Setup TRB: {setup:x?}"
            ));
            self.halt(slot, dci);
            return false;
        }
        let saved = self.slots[usize::from(slot)].eps[usize::from(dci)];
        self.advance(slot, dci);
        let Some((mut pa, mut t)) = self.next_trb(slot, dci) else {
            self.slots[usize::from(slot)].eps[usize::from(dci)] = saved;
            return false;
        };
        let mut sp = [0u8; 8];
        sp[..4].copy_from_slice(&setup[0].to_le_bytes());
        sp[4..].copy_from_slice(&setup[1].to_le_bytes());
        let wlen = u32::from(u16::from_le_bytes([sp[6], sp[7]]));
        let trt = (setup[3] >> 16) & 3;
        let dir_in = sp[0] & 0x80 != 0;
        let mut data = None;
        if (t[3] >> 10) & 0x3F == ty::DATA {
            let buf = u64::from(t[0]) | u64::from(t[1]) << 32;
            let len = t[2] & 0x1_FFFF;
            let tdir = t[3] & (1 << 16) != 0;
            if len != wlen || tdir != dir_in || trt != if dir_in { 3 } else { 2 } {
                self.violation(format!(
                    "control data stage mismatch: setup {sp:x?}, data {t:x?}"
                ));
            }
            data = Some((pa, buf, len, t));
            self.advance(slot, dci);
            match self.next_trb(slot, dci) {
                Some(x) => (pa, t) = x,
                None => {
                    self.slots[usize::from(slot)].eps[usize::from(dci)] = saved;
                    return false;
                }
            }
        } else if wlen != 0 || trt != 0 {
            self.violation(format!(
                "control transfer without data stage: {sp:x?}, TRT {trt}"
            ));
        }
        if (t[3] >> 10) & 0x3F != ty::STATUS {
            self.violation(format!("control TD without Status stage: {t:x?}"));
            self.halt(slot, dci);
            return false;
        }
        let status_in = t[3] & (1 << 16) != 0;
        if status_in != (wlen == 0 || !dir_in) {
            self.violation("status stage direction".into());
        }
        let status_pa = pa;
        self.advance(slot, dci);
        let out = match data {
            Some((_, buf, len, _)) if !dir_in => {
                let mut b = vec![0u8; len as usize];
                self.mem_read(buf, &mut b);
                b
            }
            _ => Vec::new(),
        };
        let dev = self.ports[usize::from(port) - 1].device.as_mut().unwrap();
        match dev.control(sp, &out) {
            Err(()) => {
                let at = data.map_or(status_pa, |d| d.0);
                self.halt(slot, dci);
                self.transfer_event(at, cc::STALL, 0, slot, dci);
                false
            }
            Ok(resp) => {
                if let Some((dpa, buf, len, dt)) = data {
                    if dir_in {
                        let n = resp.len().min(len as usize);
                        self.mem_write(buf, &resp[..n]);
                        if (n as u32) < len && dt[3] & (1 << 2) != 0 {
                            self.transfer_event(dpa, cc::SHORT_PACKET, len - n as u32, slot, dci);
                        }
                    }
                }
                if t[3] & (1 << 5) != 0 {
                    self.transfer_event(status_pa, cc::SUCCESS, 0, slot, dci);
                }
                true
            }
        }
    }

    fn interrupt_td(&mut self, slot: u8, dci: u8, port: u8) -> bool {
        let Some((pa, t)) = self.next_trb(slot, dci) else {
            return false;
        };
        if (t[3] >> 10) & 0x3F != ty::NORMAL {
            self.violation(format!(
                "interrupt ring holds TRB type {}",
                (t[3] >> 10) & 0x3F
            ));
            self.halt(slot, dci);
            return false;
        }
        let buf = u64::from(t[0]) | u64::from(t[1]) << 32;
        let len = t[2] & 0x1_FFFF;
        if buf >> 16 != (buf + u64::from(len) - 1) >> 16 {
            self.violation("interrupt buffer crosses 64 KiB".into());
        }
        let dev = self.ports[usize::from(port) - 1].device.as_mut().unwrap();
        if dev.stall_interrupt {
            dev.stall_interrupt = false;
            self.halt(slot, dci);
            self.transfer_event(pa, cc::STALL, len, slot, dci);
            return false;
        }
        let Some(report) = dev.reports.pop_front() else {
            return false; // NAK: wait for a report
        };
        self.advance(slot, dci);
        let n = report.len().min(len as usize);
        self.mem_write(buf, &report[..n]);
        if (n as u32) < len && t[3] & (1 << 2) != 0 {
            self.transfer_event(pa, cc::SHORT_PACKET, len - n as u32, slot, dci);
        } else if t[3] & (1 << 5) != 0 {
            self.transfer_event(pa, cc::SUCCESS, 0, slot, dci);
        }
        true
    }

    // ---- register writes ----------------------------------------------------

    fn hc_reset(&mut self) {
        self.stats.hc_resets += 1;
        if self.usbsts & HCH == 0 {
            self.violation("HCRST while the controller runs".into());
        }
        self.usbcmd = HCRST;
        self.usbsts = HCH | CNR;
        self.reset_at = self.cfg.reset_us.map(|d| self.now + d);
        self.cnr_at = match (self.cfg.reset_us, self.cfg.cnr_us) {
            (Some(r), Some(c)) => Some(self.now + r + c),
            _ => None,
        };
        self.crcr_ptr = 0;
        self.ccs = false;
        self.crr = false;
        self.stuck = Stuck::None;
        self.dcbaap = 0;
        self.config = 0;
        self.iman = 0;
        self.erstsz = 0;
        self.erstba = 0;
        self.erdp = 0;
        self.seg = 0;
        self.backlog.clear();
        self.slots = vec![Slot::default(); 256];
        self.configured = false;
    }

    fn write_usbcmd(&mut self, v: u32) {
        if v & HCRST != 0 {
            self.hc_reset();
            return;
        }
        let was = self.usbcmd & RS != 0;
        self.usbcmd = v & (RS | INTE | 8);
        if v & RS != 0 && !was {
            if self.usbsts & HCH == 0 {
                self.violation("R/S set while not halted".into());
            }
            if self.usbsts & CNR != 0 {
                self.violation("R/S set while CNR".into());
            }
            if self.dcbaap == 0 || self.crcr_ptr == 0 || self.erstba == 0 || self.config & 0xFF == 0
            {
                self.violation("R/S set before DCBAAP/CRCR/ERSTBA/CONFIG".into());
            }
            if self.cfg.scratchpads > 0 {
                let arr = self.r64(self.dcbaap);
                for i in 0..u64::from(self.cfg.scratchpads) {
                    let p = self.r64(arr + 8 * i);
                    if p == 0 || !p.is_multiple_of(4096) || !self.allocs.contains_key(&p) {
                        self.violation(format!("scratchpad {i} = {p:#x}"));
                    }
                }
            }
            self.run_at = Some(self.now + self.cfg.run_us);
        } else if v & RS == 0 && was {
            self.halt_at = self.cfg.halt_us.map(|d| self.now + d);
        }
    }

    fn write_crcr(&mut self, v: u64) {
        if self.crr {
            if v & 6 != 0 {
                // Command Abort or Command Stop.
                self.abort_commands();
            } else {
                self.violation("CRCR pointer written while the ring runs".into());
            }
            return;
        }
        if v & 6 != 0 && self.stuck == Stuck::None {
            // Abort/stop of an idle ring: stopped event only.
            self.command_event(self.crcr_ptr, cc::RING_STOPPED, 0);
            return;
        }
        self.crcr_ptr = v & !0x3F;
        self.ccs = v & 1 != 0;
    }

    fn write_erstba(&mut self, v: u64) {
        self.erstba = v & !0x3F;
        if self.erstsz & 0xFFFF != 1 {
            self.violation(format!("ERSTBA written with ERSTSZ {}", self.erstsz));
        }
        self.seg = self.r64(self.erstba) & !0x3F;
        self.seg_size = self.r32(self.erstba + 8) & 0xFFFF;
        if self.seg_size < 16 || self.erdp & !0xF != self.seg {
            self.violation(format!(
                "event ring: size {} ERDP {:#x} segment {:#x}",
                self.seg_size, self.erdp, self.seg
            ));
        }
        self.evt_enq = 0;
        self.pcs = true;
    }

    fn write_erdp(&mut self, v: u64) {
        let p = v & !0xF;
        if self.seg != 0 && (p < self.seg || p >= self.seg + 16 * u64::from(self.seg_size)) {
            self.violation(format!("ERDP {p:#x} outside the event ring"));
        }
        self.erdp = p;
        self.flush_events();
    }

    fn write_portsc(&mut self, port: u8, v: u32) {
        let now = self.now;
        let reset_us = self.cfg.port_reset_us;
        let i = usize::from(port) - 1;
        if v & PED != 0 {
            self.violation(format!(
                "PORTSC {port}: PED written as 1 (disables the port)"
            ));
            self.ports[i].ped = false;
        }
        if v & PP == 0 {
            self.violation(format!("PORTSC {port}: PP not preserved"));
        }
        let p = &mut self.ports[i];
        p.changes &= !(v & CHANGES);
        if v & (PR | WPR) != 0 && p.ccs {
            p.pr = true;
            p.warm = v & WPR != 0;
            p.ped = false;
            p.reset_done = Some(now + reset_us);
            if p.warm {
                self.stats.warm_resets += 1;
            } else {
                self.stats.port_resets += 1;
            }
        }
    }
}

impl Mmio for Hw {
    fn read32(&mut self, off: u32) -> u32 {
        self.process();
        let ports = self.ports.len() as u32;
        match off {
            0x00 => CAPLEN | 0x0120 << 16,
            0x04 => u32::from(self.cfg.max_slots) | 1 << 8 | ports << 24,
            0x08 => {
                let sp = u32::from(self.cfg.scratchpads);
                (sp & 0x1F) << 27 | (sp >> 5) << 21 | 1 << 4
            }
            0x0C => 0,
            0x10 => {
                let x = if self.cfg.legacy {
                    XECP_LEGACY
                } else {
                    XECP_USB2
                };
                u32::from(self.cfg.ac64) | u32::from(self.cfg.csz) << 2 | (x / 4) << 16
            }
            0x14 => DB,
            0x18 => RTS,
            0x1C => 0,
            o if o == OP => self.usbcmd,
            o if o == OP + 4 => self.usbsts,
            o if o == OP + 8 => 1,
            o if o == OP + 0x18 => u32::from(self.crr) << 3,
            o if o == OP + 0x1C => 0,
            o if o == OP + 0x30 => self.dcbaap as u32,
            o if o == OP + 0x34 => (self.dcbaap >> 32) as u32,
            o if o == OP + 0x38 => self.config,
            o if (PORTS..PORTS + 0x10 * ports).contains(&o) && (o - PORTS).is_multiple_of(0x10) => {
                self.ports[((o - PORTS) / 0x10) as usize].portsc()
            }
            o if (PORTS..PORTS + 0x10 * ports).contains(&o) => 0,
            o if o == IR0 => self.iman,
            o if o == IR0 + 4 => self.imod,
            o if o == IR0 + 8 => self.erstsz,
            o if o == IR0 + 0x10 => self.erstba as u32,
            o if o == IR0 + 0x14 => (self.erstba >> 32) as u32,
            o if o == IR0 + 0x18 => self.erdp as u32,
            o if o == IR0 + 0x1C => (self.erdp >> 32) as u32,
            o if o == XECP_LEGACY => {
                if self.cfg.legacy {
                    self.legsup
                } else {
                    0
                }
            }
            o if o == XECP_LEGACY + 4 => self.legctl,
            o if o == XECP_USB2 => 2 | ((XECP_USB3 - XECP_USB2) / 4) << 8 | 0x02 << 24,
            o if o == XECP_USB2 + 4 || o == XECP_USB3 + 4 => 0x2042_5355,
            o if o == XECP_USB2 + 8 => 1 | 2 << 8,
            o if o == XECP_USB3 => 2 | 0x03 << 24,
            o if o == XECP_USB3 + 8 => 3 | 2 << 8,
            o if o == XECP_USB2 + 12 || o == XECP_USB3 + 12 => 0,
            _ => 0,
        }
    }

    fn write32(&mut self, off: u32, v: u32) {
        self.process();
        let ports = self.ports.len() as u32;
        match off {
            o if o == OP => self.write_usbcmd(v),
            o if o == OP + 4 => self.usbsts &= !(v & (HSE | EINT | PCD | 1 << 10)),
            o if o == OP + 0x18 => {
                self.crcr_lo = Some(v);
            }
            o if o == OP + 0x1C => {
                let lo = self.crcr_lo.take().unwrap_or(0);
                self.write_crcr(u64::from(lo) | u64::from(v) << 32);
            }
            o if o == OP + 0x30 => {
                self.dcbaap = (self.dcbaap & !0xFFFF_FFFF) | u64::from(v & !0x3F)
            }
            o if o == OP + 0x34 => self.dcbaap = (self.dcbaap & 0xFFFF_FFFF) | u64::from(v) << 32,
            o if o == OP + 0x38 => {
                if self.running() {
                    self.violation("CONFIG written while running".into());
                }
                if v & 0xFF > u32::from(self.cfg.max_slots) {
                    self.violation(format!("MaxSlotsEn {} > MaxSlots", v & 0xFF));
                }
                self.config = v;
            }
            o if o == OP + 0x14 => {}
            o if (PORTS..PORTS + 0x10 * ports).contains(&o) => {
                if (o - PORTS).is_multiple_of(0x10) {
                    self.write_portsc(((o - PORTS) / 0x10) as u8 + 1, v);
                }
            }
            o if o == IR0 => {
                self.iman = (self.iman & !2) | (v & 2);
                if v & 1 != 0 {
                    self.iman &= !1;
                }
            }
            o if o == IR0 + 4 => self.imod = v,
            o if o == IR0 + 8 => self.erstsz = v,
            o if o == IR0 + 0x10 => self.erstba_lo = Some(v),
            o if o == IR0 + 0x14 => {
                let lo = self.erstba_lo.take().unwrap_or(0);
                self.write_erstba(u64::from(lo) | u64::from(v) << 32);
            }
            o if o == IR0 + 0x18 => self.erdp_lo = Some(v),
            o if o == IR0 + 0x1C => {
                let lo = self.erdp_lo.take().unwrap_or(0);
                self.write_erdp(u64::from(lo) | u64::from(v) << 32);
            }
            o if o == XECP_LEGACY => {
                if v & OS_OWNED != 0 && self.legsup & OS_OWNED == 0 {
                    self.bios_release_at = self.cfg.bios_release_us.map(|d| self.now + d);
                }
                self.legsup = (self.legsup & 0xFFFF) | (v & (OS_OWNED | BIOS_OWNED));
                if self.legsup & BIOS_OWNED == 0 {
                    self.bios_release_at = None;
                }
            }
            o if o == XECP_LEGACY + 4 => {
                self.legctl = (v & 0xE001) | (v & 0x10) | (self.legctl & 0xE000_0000 & !v);
            }
            o if (DB..DB + 4 * 256).contains(&o) => {
                self.stats.doorbells += 1;
                let slot = ((o - DB) / 4) as u8;
                if !self.running() {
                    self.violation(format!("doorbell {slot} rung while halted"));
                    return;
                }
                if slot == 0 {
                    if v != 0 {
                        self.violation(format!("host controller doorbell target {v}"));
                    }
                    self.cmd_kick_at = Some(self.now + self.cfg.cmd_latency_us);
                } else {
                    let dci = (v & 0xFF) as u8;
                    if !(1..=31).contains(&dci) || !self.slots[usize::from(slot)].enabled {
                        self.violation(format!("doorbell slot {slot} target {dci}"));
                        return;
                    }
                    let e = &mut self.slots[usize::from(slot)].eps[usize::from(dci)];
                    if e.state == EP_STOPPED {
                        e.state = EP_RUNNING;
                    }
                    self.run_endpoint(slot, dci);
                }
            }
            _ => {}
        }
    }
}

impl DmaMemory for Hw {
    fn read(&mut self, pa: u64, buf: &mut [u8]) {
        self.mem_read(pa, buf);
    }
    fn write(&mut self, pa: u64, data: &[u8]) {
        self.mem_write(pa, data);
    }
}

impl DmaAlloc for Hw {
    fn alloc(&mut self, len: usize, align: usize) -> Option<u64> {
        if len == 0 || !align.is_power_of_two() {
            self.violation(format!("alloc({len}, {align})"));
            return None;
        }
        let mut pa = MEM_BASE;
        for (&start, &l) in &self.allocs {
            let cand = pa.next_multiple_of(align as u64);
            if cand + len as u64 <= start {
                break;
            }
            pa = pa.max(start + l as u64);
        }
        let pa = pa.next_multiple_of(align as u64);
        if pa + len as u64 > USER_BASE {
            return None;
        }
        // Garbage, so nothing works because memory happened to be zero.
        self.mem_write(pa, &vec![0x5A; len]);
        self.allocs.insert(pa, len);
        Some(pa)
    }

    fn free(&mut self, pa: u64, len: usize) {
        match self.allocs.get(&pa) {
            Some(&l) if l == len => {
                self.allocs.remove(&pa);
            }
            Some(&l) => self.violation(format!("free({pa:#x}, {len}) of a {l}-byte block")),
            None => self.violation(format!("free({pa:#x}, {len}) of an unallocated block")),
        }
    }
}

impl Clock for Hw {
    fn now_us(&mut self) -> u64 {
        self.now += self.cfg.tick_us;
        self.process();
        self.now
    }
}

// ---- driver-side helpers ----------------------------------------------------

pub fn config() -> Config {
    Config {
        mmio_len: MMIO_LEN,
        ..Config::default()
    }
}

/// Model plus a running controller.
pub fn running(mcfg: ModelConfig, cfg: Config) -> (Hw, Controller) {
    let mut hw = Hw::new(mcfg);
    let mut c = Controller::new(&mut hw, cfg).expect("new");
    c.init(&mut hw).expect("init");
    (hw, c)
}

/// Deterministic xorshift64* generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
