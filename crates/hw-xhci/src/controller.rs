//! The xHCI driver proper.
//!
//! # Initialization (xHCI §4.2)
//!
//! [`Controller::init`] runs a state machine ([`InitPhase`]):
//!
//! ```text
//! Handoff ─► Halt ─► Reset ─► WaitReady ─► Setup ─► Run ─► Done
//! ```
//!
//! * Handoff: USB Legacy Support BIOS→OS ownership (§7.1.1), bounded by
//!   [`Timeouts::bios_handoff_us`]; on timeout either fail or, with
//!   [`Config::force_bios_takeover`], clear the BIOS semaphore. SMIs are
//!   disabled afterwards.
//! * Halt: USBCMD.R/S = 0, wait USBSTS.HCH = 1 (§5.4.1: within 16 ms).
//! * Reset: USBCMD.HCRST = 1, wait HCRST = 0; WaitReady: wait CNR = 0.
//! * Setup: CONFIG.MaxSlotsEn, DCBAA with scratchpad buffers (§4.20),
//!   command ring (CRCR), event ring (ERSTSZ, ERDP, ERSTBA written last),
//!   IMOD/IMAN.
//! * Run: USBCMD.R/S = 1, wait HCH = 0.
//!
//! USBSTS.HSE or HCE, or an all-ones register read, at any later point
//! moves the controller to [`State::Failed`]: every pending operation
//! completes with that error exactly once, and [`Controller::recover`]
//! repeats the state machine from Handoff (a full reset). DMA memory given
//! to the controller is only freed after the controller has been halted
//! and reset.
//!
//! # Operation
//!
//! Commands and control transfers are synchronous with timeouts; interrupt
//! IN transfers are asynchronous and complete through
//! [`Controller::poll`], which also reports port changes and detached
//! devices and performs deferred recovery (Reset Endpoint + Set TR Dequeue
//! Pointer + CLEAR_FEATURE(ENDPOINT_HALT) after a stall, Disable Slot
//! after a disconnect).

use crate::context::{self, ep_state, ep_type, EndpointContext, InputControl, Layout, SlotContext};
use crate::descriptor::{
    self, dtype, ConfigDescriptor, DeviceDescriptor, EndpointDescriptor, SetupPacket, SsCompanion,
    TransferType,
};
use crate::extcap::{self, legacy, ExtCaps, Protocols};
use crate::port::{self, portsc, LinkState, PortStatus, Speed};
use crate::regs::{crcr, erdp, iman, op, rt, usbcmd, usbsts, Capabilities};
use crate::ring::{EventRing, ProducerRing, RingPos, Td};
use crate::trb::{flag, Command, CompletionCode, Event, Trb, Trt};
use crate::{DescError, Error, Hal, Mmio, Wait};

/// Device slots tracked by the driver (CONFIG.MaxSlotsEn is capped here).
pub const MAX_SLOTS: usize = 32;
/// Transfers that can be outstanding at once (including undelivered
/// completions).
pub const MAX_INFLIGHT: usize = 64;
/// Size of the driver-owned bounce buffer for control transfers.
pub const CONTROL_BUFFER_LEN: usize = 4096;
/// Size of the DCBAA allocation (256 entries).
const DCBAA_LEN: usize = 2048;

/// Timeouts in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// BIOS handoff (xHCI §7.1.1; Linux waits 1 s).
    pub bios_handoff_us: u64,
    /// HCH after R/S = 0 (§5.4.1: 16 ms).
    pub halt_us: u64,
    /// HCRST self-clear.
    pub reset_us: u64,
    /// CNR = 0.
    pub ready_us: u64,
    /// HCH = 0 after R/S = 1.
    pub run_us: u64,
    /// One command.
    pub command_us: u64,
    /// CRR = 0 after Command Abort.
    pub command_abort_us: u64,
    /// One control transfer.
    pub transfer_us: u64,
    /// Port reset completion (PRC/WRC).
    pub port_reset_us: u64,
    /// USB3 link training to U0.
    pub port_enable_us: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            bios_handoff_us: 1_000_000,
            halt_us: 32_000,
            reset_us: 1_000_000,
            ready_us: 1_000_000,
            run_us: 32_000,
            command_us: 5_000_000,
            command_abort_us: 5_000_000,
            transfer_us: 5_000_000,
            port_reset_us: 500_000,
            port_enable_us: 500_000,
        }
    }
}

/// Driver configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Size of the MMIO window in bytes.
    pub mmio_len: u32,
    /// Timeouts.
    pub timeouts: Timeouts,
    /// Take ownership from a BIOS that does not release it in time
    /// (clears HC BIOS Owned, as Linux does) instead of failing.
    pub force_bios_takeover: bool,
    /// Command ring entries (including the Link TRB).
    pub command_ring_trbs: u16,
    /// Transfer ring entries per endpoint (including the Link TRB).
    pub transfer_ring_trbs: u16,
    /// Event ring segment entries.
    pub event_ring_trbs: u16,
    /// Upper bound for CONFIG.MaxSlotsEn (at most [`MAX_SLOTS`]).
    pub max_slots: u8,
    /// Enable interrupter 0 (IMAN.IE, USBCMD.INTE).
    pub interrupts: bool,
    /// IMOD interval in 250 ns units.
    pub imod_interval: u16,
    /// Enumerate with Address Device BSR = 1 first (read the first 8
    /// bytes of the device descriptor at the default address), then
    /// BSR = 0.
    pub address_with_bsr: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mmio_len: 0x1_0000,
            timeouts: Timeouts::default(),
            force_bios_takeover: false,
            command_ring_trbs: 256,
            transfer_ring_trbs: 256,
            event_ring_trbs: 256,
            max_slots: MAX_SLOTS as u8,
            interrupts: true,
            imod_interval: 4000,
            address_with_bsr: false,
        }
    }
}

/// Controller state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not initialized (or shut down).
    Uninit,
    /// Initialization in progress.
    Initializing,
    /// Running.
    Running,
    /// Failed; [`Controller::recover`] is required.
    Failed(Error),
}

/// Phases of the initialization state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitPhase {
    /// Not started.
    Idle,
    /// BIOS/OS handoff.
    Handoff,
    /// Stopping the controller.
    Halt,
    /// HCRST.
    Reset,
    /// Waiting for CNR = 0.
    WaitReady,
    /// Programming data structures.
    Setup,
    /// Starting the controller.
    Run,
    /// Running.
    Done,
}

/// Handle of an asynchronous transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(pub u64);

/// Result of an asynchronous transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Transfer handle.
    pub id: TransferId,
    /// Slot ID.
    pub slot: u8,
    /// Device Context Index.
    pub dci: u8,
    /// Bytes transferred, or the error.
    pub result: Result<u32, Error>,
}

/// Something [`Controller::poll`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    /// An asynchronous transfer finished.
    Transfer(Completion),
    /// A port's status changed; `status.raw` includes every change bit the
    /// driver observed and cleared since the last report.
    PortChanged {
        /// Port number.
        port: u8,
        /// PORTSC plus accumulated change bits.
        status: PortStatus,
    },
    /// A device was disconnected and its slot released.
    DeviceDetached {
        /// Former slot ID.
        slot: u8,
        /// Port it was on.
        port: u8,
    },
}

/// A device after enumeration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnumeratedDevice {
    /// Slot ID.
    pub slot: u8,
    /// Root hub port.
    pub port: u8,
    /// Speed.
    pub speed: Speed,
    /// Device descriptor.
    pub descriptor: DeviceDescriptor,
    /// EP0 max packet size in the device context.
    pub ep0_max_packet: u16,
}

/// Device state as tracked by the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    /// Slot enabled, not addressed.
    Enabled,
    /// Address Device with BSR = 1 done.
    Default,
    /// Addressed.
    Addressed,
    /// At least one endpoint besides EP0 configured.
    Configured,
    /// Disconnected; slot not yet released.
    Detached,
}

/// Public view of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Slot ID.
    pub slot: u8,
    /// Root hub port.
    pub port: u8,
    /// Speed.
    pub speed: Speed,
    /// State.
    pub state: DeviceState,
    /// EP0 max packet size.
    pub ep0_max_packet: u16,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events consumed.
    pub events: u64,
    /// Command completions that matched no pending command.
    pub stale_command_events: u64,
    /// Transfer events that matched no pending transfer.
    pub stale_transfer_events: u64,
    /// Host Controller events.
    pub host_controller_events: u64,
    /// Other events.
    pub other_events: u64,
    /// Commands issued.
    pub commands: u64,
    /// Command aborts.
    pub command_aborts: u64,
    /// Endpoint recoveries (Reset Endpoint + Set TR Dequeue).
    pub endpoint_recoveries: u64,
    /// Controller resets by [`Controller::recover`].
    pub recoveries: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DmaBuf {
    pa: u64,
    len: usize,
}

#[derive(Clone, Copy, Debug)]
struct Core {
    dcbaa: DmaBuf,
    sp_array: Option<DmaBuf>,
    sp_count: u16,
    cmd_buf: DmaBuf,
    cmd: ProducerRing,
    evt_buf: DmaBuf,
    erst_buf: DmaBuf,
    evt: EventRing,
    ctrl: DmaBuf,
}

#[derive(Clone, Copy, Debug)]
struct Endpoint {
    ring: ProducerRing,
    buf: DmaBuf,
    address: u8,
    halted: bool,
    resume_at: Option<RingPos>,
}

#[derive(Clone, Copy, Debug)]
struct Device {
    slot: u8,
    port: u8,
    speed: Speed,
    out_ctx: DmaBuf,
    in_ctx: DmaBuf,
    eps: [Option<Endpoint>; 31],
    state: DeviceState,
    ep0_mps: u16,
    disable_failed: bool,
}

#[derive(Clone, Copy, Debug)]
struct InFlight {
    id: TransferId,
    slot: u8,
    dci: u8,
    td: Td,
    data_len: u32,
    short: Option<u32>,
    sync: bool,
    result: Option<Result<u32, Error>>,
}

#[derive(Clone, Copy, Debug)]
struct Queue {
    items: [Option<Completion>; MAX_INFLIGHT],
    head: usize,
    len: usize,
}

impl Queue {
    const fn new() -> Self {
        Self {
            items: [None; MAX_INFLIGHT],
            head: 0,
            len: 0,
        }
    }
    fn push(&mut self, c: Completion) {
        // Capacity is guaranteed by `Controller::claim_inflight`.
        if self.len < MAX_INFLIGHT {
            self.items[(self.head + self.len) % MAX_INFLIGHT] = Some(c);
            self.len += 1;
        }
    }
    fn pop(&mut self) -> Option<Completion> {
        if self.len == 0 {
            return None;
        }
        let c = self.items[self.head].take();
        self.head = (self.head + 1) % MAX_INFLIGHT;
        self.len -= 1;
        c
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PortSet([u64; 4]);

impl PortSet {
    fn set(&mut self, port: u8) {
        self.0[usize::from(port / 64)] |= 1 << (port % 64);
    }
    fn take_first(&mut self) -> Option<u8> {
        for (i, w) in self.0.iter_mut().enumerate() {
            if *w != 0 {
                let b = w.trailing_zeros();
                *w &= !(1 << b);
                return Some((i as u32 * 64 + b) as u8);
            }
        }
        None
    }
}

fn ring_align(len: usize) -> usize {
    len.next_power_of_two().max(64)
}

/// The driver for one xHCI controller.
#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    caps: Capabilities,
    layout: Layout,
    protocols: Protocols,
    legacy: Option<u32>,
    state: State,
    phase: InitPhase,
    core: Option<Core>,
    max_slots_en: u8,
    devices: [Option<Device>; MAX_SLOTS],
    inflight: [Option<InFlight>; MAX_INFLIGHT],
    done: Queue,
    port_pending: PortSet,
    port_notify: PortSet,
    port_changes: [u32; 256],
    detached_notify: [Option<u8>; MAX_SLOTS],
    next_id: u64,
    cmd_pending: Option<u64>,
    cmd_result: Option<(CompletionCode, u8, u32)>,
    stats: Stats,
}

impl Controller {
    /// Reads and validates the capability registers and the extended
    /// capability list. Does not write any register.
    pub fn new<M: Mmio + ?Sized>(mmio: &mut M, cfg: Config) -> Result<Self, Error> {
        if !(ProducerRing::MIN_TRBS..=ProducerRing::MAX_TRBS).contains(&cfg.command_ring_trbs)
            || !(ProducerRing::MIN_TRBS..=ProducerRing::MAX_TRBS).contains(&cfg.transfer_ring_trbs)
            || !(EventRing::MIN_TRBS..=EventRing::MAX_TRBS).contains(&cfg.event_ring_trbs)
        {
            return Err(Error::BadRequest("ring size"));
        }
        if cfg.max_slots == 0 {
            return Err(Error::BadRequest("max_slots"));
        }
        let caps = Capabilities::read(mmio, cfg.mmio_len)?;
        let ext = ExtCaps::read(mmio, caps.xecp, cfg.mmio_len)?;
        let protocols = Protocols::read(mmio, &ext, caps.max_ports, cfg.mmio_len)?;
        let legacy = ext.find(extcap::ID_LEGACY).map(|c| c.offset);
        if let Some(off) = legacy {
            if u64::from(off) + 8 > u64::from(cfg.mmio_len) {
                return Err(Error::BadExtCap("legacy support capability outside window"));
            }
        }
        Ok(Self {
            cfg,
            caps,
            layout: Layout::new(caps.csz),
            protocols,
            legacy,
            state: State::Uninit,
            phase: InitPhase::Idle,
            core: None,
            max_slots_en: 0,
            devices: [None; MAX_SLOTS],
            inflight: [None; MAX_INFLIGHT],
            done: Queue::new(),
            port_pending: PortSet::default(),
            port_notify: PortSet::default(),
            port_changes: [0; 256],
            detached_notify: [None; MAX_SLOTS],
            next_id: 1,
            cmd_pending: None,
            cmd_result: None,
            stats: Stats::default(),
        })
    }

    /// Capability registers.
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    /// Supported Protocol capabilities.
    pub fn protocols(&self) -> &Protocols {
        &self.protocols
    }
    /// Current state.
    pub fn state(&self) -> State {
        self.state
    }
    /// Last initialization phase entered.
    pub fn init_phase(&self) -> InitPhase {
        self.phase
    }
    /// Counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }
    /// CONFIG.MaxSlotsEn programmed at init.
    pub fn max_slots_enabled(&self) -> u8 {
        self.max_slots_en
    }
    /// Number of transfers not yet reported through [`Controller::poll`].
    pub fn pending_transfers(&self) -> usize {
        self.inflight.iter().flatten().count() + self.done.len
    }
    /// Public view of the device in `slot`.
    pub fn device_info(&self, slot: u8) -> Option<DeviceInfo> {
        let d = self
            .devices
            .get(usize::from(slot).checked_sub(1)?)?
            .as_ref()?;
        Some(DeviceInfo {
            slot: d.slot,
            port: d.port,
            speed: d.speed,
            state: d.state,
            ep0_max_packet: d.ep0_mps,
        })
    }
    /// True when `port` belongs to a USB3 Supported Protocol capability.
    pub fn is_usb3_port(&self, port: u8) -> bool {
        self.protocols.for_port(port).is_some_and(|p| p.is_usb3())
    }

    // ----------------------------------------------------------------
    // Initialization, recovery, shutdown
    // ----------------------------------------------------------------

    /// Initializes and starts the controller.
    pub fn init<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        if self.state == State::Running {
            return Err(Error::BadRequest("controller already running"));
        }
        self.bring_up(hw)
    }

    /// Full reset and re-initialization after a failure (or at any time).
    /// Every pending operation completes with [`Error::ControllerReset`]
    /// (unless it already completed with the failure) and every device is
    /// forgotten; ports are reported again through [`Controller::poll`].
    pub fn recover<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        self.stats.recoveries += 1;
        self.fail_all(Error::ControllerReset);
        self.cmd_pending = None;
        self.cmd_result = None;
        self.bring_up(hw)
    }

    /// Halts and resets the controller and frees all DMA memory. If the
    /// controller cannot be halted the memory is kept (the controller may
    /// still write to it) and the error is returned.
    pub fn shutdown<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        self.fail_all(Error::ControllerReset);
        self.cmd_pending = None;
        self.cmd_result = None;
        let r = self.halt_and_reset(hw);
        if let Err(e) = r {
            self.state = State::Failed(e);
            return Err(e);
        }
        self.release_devices(hw);
        self.free_core(hw);
        self.port_pending = PortSet::default();
        self.port_notify = PortSet::default();
        self.port_changes = [0; 256];
        self.state = State::Uninit;
        self.phase = InitPhase::Idle;
        Ok(())
    }

    fn halt_and_reset<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        let cmd_off = self.caps.op(op::USBCMD);
        let sts_off = self.caps.op(op::USBSTS);
        let c = hw.read32(cmd_off);
        if c == u32::MAX {
            return Err(Error::ControllerGone);
        }
        if c & usbcmd::RS != 0 {
            hw.write32(cmd_off, c & !(usbcmd::RS | usbcmd::INTE));
        }
        let t = self.cfg.timeouts;
        self.wait_bits(
            hw,
            sts_off,
            (usbsts::HCH, usbsts::HCH),
            t.halt_us,
            Wait::Halt,
        )?;
        hw.write32(cmd_off, usbcmd::HCRST);
        self.wait_bits(hw, cmd_off, (usbcmd::HCRST, 0), t.reset_us, Wait::Reset)?;
        self.wait_bits(
            hw,
            sts_off,
            (usbsts::CNR, 0),
            t.ready_us,
            Wait::ControllerNotReady,
        )
    }

    fn bring_up<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        self.state = State::Initializing;
        match self.init_machine(hw) {
            Ok(()) => {
                self.state = State::Running;
                self.phase = InitPhase::Done;
                self.scan_ports(hw);
                Ok(())
            }
            Err(e) => {
                self.state = State::Failed(e);
                self.fail_all(e);
                Err(e)
            }
        }
    }

    fn init_machine<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        let t = self.cfg.timeouts;
        let cmd_off = self.caps.op(op::USBCMD);
        let sts_off = self.caps.op(op::USBSTS);
        let mut phase = InitPhase::Handoff;
        loop {
            self.phase = phase;
            phase = match phase {
                InitPhase::Idle | InitPhase::Handoff => {
                    if let Some(off) = self.legacy {
                        self.bios_handoff(hw, off)?;
                    }
                    InitPhase::Halt
                }
                InitPhase::Halt => {
                    let c = hw.read32(cmd_off);
                    if c == u32::MAX {
                        return Err(Error::ControllerGone);
                    }
                    if c & usbcmd::RS != 0 {
                        hw.write32(cmd_off, c & !(usbcmd::RS | usbcmd::INTE));
                    }
                    self.wait_bits(
                        hw,
                        sts_off,
                        (usbsts::HCH, usbsts::HCH),
                        t.halt_us,
                        Wait::Halt,
                    )?;
                    InitPhase::Reset
                }
                InitPhase::Reset => {
                    hw.write32(cmd_off, usbcmd::HCRST);
                    self.wait_bits(hw, cmd_off, (usbcmd::HCRST, 0), t.reset_us, Wait::Reset)?;
                    InitPhase::WaitReady
                }
                InitPhase::WaitReady => {
                    self.wait_bits(
                        hw,
                        sts_off,
                        (usbsts::CNR, 0),
                        t.ready_us,
                        Wait::ControllerNotReady,
                    )?;
                    InitPhase::Setup
                }
                InitPhase::Setup => {
                    // The controller is halted and reset: nothing it held can
                    // be touched by it any more.
                    self.release_devices(hw);
                    self.setup(hw)?;
                    InitPhase::Run
                }
                InitPhase::Run => {
                    let mut c = usbcmd::RS;
                    if self.cfg.interrupts {
                        c |= usbcmd::INTE;
                    }
                    hw.write32(cmd_off, c);
                    self.wait(hw, t.run_us, Wait::Run, |_, hw| {
                        let s = hw.read32(sts_off);
                        fatal_status(s)?;
                        Ok(s & usbsts::HCH == 0)
                    })?;
                    InitPhase::Done
                }
                InitPhase::Done => {
                    fatal_status(hw.read32(sts_off))?;
                    return Ok(());
                }
            };
        }
    }

    fn bios_handoff<H: Hal>(&mut self, hw: &mut H, off: u32) -> Result<(), Error> {
        let v = hw.read32(off);
        if v == u32::MAX {
            return Err(Error::ControllerGone);
        }
        if v & legacy::BIOS_OWNED != 0 || v & legacy::OS_OWNED == 0 {
            hw.write32(off, v | legacy::OS_OWNED);
            let r = self.wait(
                hw,
                self.cfg.timeouts.bios_handoff_us,
                Wait::BiosHandoff,
                |_, hw| {
                    let v = hw.read32(off);
                    if v == u32::MAX {
                        return Err(Error::ControllerGone);
                    }
                    Ok(v & legacy::BIOS_OWNED == 0)
                },
            );
            match r {
                Ok(()) => {}
                Err(Error::Timeout(_)) if self.cfg.force_bios_takeover => {
                    let v = hw.read32(off);
                    hw.write32(off, (v & !legacy::BIOS_OWNED) | legacy::OS_OWNED);
                }
                Err(e) => return Err(e),
            }
        }
        // Disable SMIs and clear pending SMI events (RW1C).
        let ctl = hw.read32(off + legacy::CTLSTS);
        hw.write32(
            off + legacy::CTLSTS,
            (ctl & !legacy::SMI_ENABLES) | legacy::SMI_EVENTS,
        );
        Ok(())
    }

    fn setup<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        let caps = self.caps;
        if hw.read32(caps.op(op::PAGESIZE)) & 1 == 0 {
            return Err(Error::Unsupported("4 KiB page size"));
        }
        self.max_slots_en = caps.max_slots.min(self.cfg.max_slots).min(MAX_SLOTS as u8);
        if self.core.is_none() {
            self.core = Some(self.alloc_core(hw)?);
        }
        let interrupts = self.cfg.interrupts;
        let imod = self.cfg.imod_interval;
        let Some(core) = self.core.as_mut() else {
            return Err(Error::NotRunning);
        };
        hw.fill_zero(core.dcbaa.pa, core.dcbaa.len);
        if let Some(sp) = core.sp_array {
            hw.write_u64(core.dcbaa.pa, sp.pa);
        }
        core.cmd.reset(hw);
        core.evt.reset(hw);
        let cfg_off = caps.op(op::CONFIG);
        let c = hw.read32(cfg_off);
        hw.write32(
            cfg_off,
            (c & !crate::regs::config::MAX_SLOTS_EN_MASK) | u32::from(self.max_slots_en),
        );
        hw.write64(caps.op(op::DCBAAP), core.dcbaa.pa);
        hw.write64(caps.op(op::CRCR), core.cmd.base() | crcr::RCS);
        let sz = hw.read32(caps.ir0(rt::ERSTSZ));
        hw.write32(caps.ir0(rt::ERSTSZ), (sz & !0xFFFF) | 1);
        hw.write64(caps.ir0(rt::ERDP), core.evt.erdp() | erdp::EHB);
        // ERSTBA last: writing it makes the event ring live (§4.9.4).
        hw.write64(caps.ir0(rt::ERSTBA), core.evt.erst());
        hw.write32(caps.ir0(rt::IMOD), u32::from(imod));
        hw.write32(
            caps.ir0(rt::IMAN),
            if interrupts {
                iman::IE | iman::IP
            } else {
                iman::IP
            },
        );
        Ok(())
    }

    fn dma_alloc<H: Hal>(&self, hw: &mut H, len: usize, align: usize) -> Result<DmaBuf, Error> {
        let pa = hw.alloc(len, align).ok_or(Error::OutOfDmaMemory)?;
        let end = pa.checked_add(len as u64 - 1);
        let ok = pa != 0
            && pa % align as u64 == 0
            && end.is_some_and(|e| self.caps.ac64 || e <= u64::from(u32::MAX));
        if !ok {
            hw.free(pa, len);
            return Err(Error::BadDmaAddress);
        }
        hw.fill_zero(pa, len);
        Ok(DmaBuf { pa, len })
    }

    fn alloc_core<H: Hal>(&self, hw: &mut H) -> Result<Core, Error> {
        let mut owned: [Option<DmaBuf>; 6] = [None; 6];
        let r = self.alloc_core_inner(hw, &mut owned);
        if r.is_err() {
            for b in owned.iter().flatten() {
                hw.free(b.pa, b.len);
            }
        }
        r
    }

    fn alloc_core_inner<H: Hal>(
        &self,
        hw: &mut H,
        owned: &mut [Option<DmaBuf>; 6],
    ) -> Result<Core, Error> {
        let dcbaa = self.dma_alloc(hw, DCBAA_LEN, DCBAA_LEN)?;
        owned[0] = Some(dcbaa);
        let cmd_len = ProducerRing::bytes(self.cfg.command_ring_trbs);
        let cmd_buf = self.dma_alloc(hw, cmd_len, ring_align(cmd_len))?;
        owned[1] = Some(cmd_buf);
        let cmd = ProducerRing::new(cmd_buf.pa, self.cfg.command_ring_trbs)?;
        let evt_len = usize::from(self.cfg.event_ring_trbs) * 16;
        let evt_buf = self.dma_alloc(hw, evt_len, ring_align(evt_len))?;
        owned[2] = Some(evt_buf);
        let erst_buf = self.dma_alloc(hw, 64, 64)?;
        owned[3] = Some(erst_buf);
        let evt = EventRing::new(evt_buf.pa, erst_buf.pa, self.cfg.event_ring_trbs)?;
        let ctrl = self.dma_alloc(hw, CONTROL_BUFFER_LEN, 4096)?;
        owned[4] = Some(ctrl);
        let sp_count = self.caps.max_scratchpad;
        let sp_array = if sp_count > 0 {
            let arr = self.dma_alloc(hw, usize::from(sp_count) * 8, 4096)?;
            owned[5] = Some(arr);
            for i in 0..sp_count {
                match self.dma_alloc(hw, 4096, 4096) {
                    Ok(p) => hw.write_u64(arr.pa + 8 * u64::from(i), p.pa),
                    Err(e) => {
                        free_scratchpad_pages(hw, arr, i);
                        return Err(e);
                    }
                }
            }
            Some(arr)
        } else {
            None
        };
        Ok(Core {
            dcbaa,
            sp_array,
            sp_count,
            cmd_buf,
            cmd,
            evt_buf,
            erst_buf,
            evt,
            ctrl,
        })
    }

    fn free_core<H: Hal>(&mut self, hw: &mut H) {
        if let Some(core) = self.core.take() {
            if let Some(arr) = core.sp_array {
                free_scratchpad_pages(hw, arr, core.sp_count);
                hw.free(arr.pa, arr.len);
            }
            for b in [
                core.dcbaa,
                core.cmd_buf,
                core.evt_buf,
                core.erst_buf,
                core.ctrl,
            ] {
                hw.free(b.pa, b.len);
            }
        }
    }

    fn release_devices<H: Hal>(&mut self, hw: &mut H) {
        for i in 0..MAX_SLOTS {
            self.free_device(hw, i);
        }
        self.detached_notify = [None; MAX_SLOTS];
    }

    fn free_device<H: Hal>(&mut self, hw: &mut H, idx: usize) {
        let Some(d) = self.devices[idx].take() else {
            return;
        };
        for i in 0..MAX_INFLIGHT {
            if self.inflight[i].is_some_and(|f| f.slot == d.slot) {
                self.complete(i, Err(Error::Disconnected));
            }
        }
        if let Some(core) = self.core {
            hw.write_u64(core.dcbaa.pa + 8 * u64::from(d.slot), 0);
        }
        for ep in d.eps.iter().flatten() {
            hw.free(ep.buf.pa, ep.buf.len);
        }
        hw.free(d.in_ctx.pa, d.in_ctx.len);
        hw.free(d.out_ctx.pa, d.out_ctx.len);
    }

    fn scan_ports<H: Hal>(&mut self, hw: &mut H) {
        self.port_pending = PortSet::default();
        self.port_notify = PortSet::default();
        self.port_changes = [0; 256];
        for p in 1..=self.caps.max_ports {
            let v = hw.read32(self.caps.portsc(p));
            if v != u32::MAX && v & (portsc::CCS | portsc::CHANGE_BITS) != 0 {
                self.port_pending.set(p);
            }
        }
    }

    // ----------------------------------------------------------------
    // Waiting, failure, events
    // ----------------------------------------------------------------

    fn wait<H: Hal, F>(
        &mut self,
        hw: &mut H,
        timeout: u64,
        what: Wait,
        mut done: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&mut Self, &mut H) -> Result<bool, Error>,
    {
        let start = hw.now_us();
        loop {
            if done(self, hw)? {
                return Ok(());
            }
            if hw.now_us().saturating_sub(start) >= timeout {
                return if done(self, hw)? {
                    Ok(())
                } else {
                    Err(Error::Timeout(what))
                };
            }
        }
    }

    fn wait_bits<H: Hal>(
        &mut self,
        hw: &mut H,
        off: u32,
        (mask, want): (u32, u32),
        timeout: u64,
        what: Wait,
    ) -> Result<(), Error> {
        self.wait(hw, timeout, what, |_, hw| {
            let v = hw.read32(off);
            if v == u32::MAX {
                return Err(Error::ControllerGone);
            }
            Ok(v & mask == want)
        })
    }

    fn ensure_running(&self) -> Result<(), Error> {
        match self.state {
            State::Running => Ok(()),
            State::Failed(e) => Err(e),
            _ => Err(Error::NotRunning),
        }
    }

    fn check_fatal<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        let s = hw.read32(self.caps.op(op::USBSTS));
        if let Err(e) = fatal_status(s) {
            self.fail(e);
            return Err(e);
        }
        Ok(())
    }

    fn fail(&mut self, e: Error) {
        self.state = State::Failed(e);
        self.cmd_pending = None;
        self.fail_all(e);
    }

    fn fail_all(&mut self, e: Error) {
        for i in 0..MAX_INFLIGHT {
            self.complete(i, Err(e));
        }
    }

    /// Delivers the result of in-flight entry `idx` exactly once: a
    /// synchronous waiter finds it in the entry, an asynchronous one is
    /// moved to the completion queue and its entry freed.
    fn complete(&mut self, idx: usize, result: Result<u32, Error>) {
        let Some(f) = self.inflight[idx].as_mut() else {
            return;
        };
        if f.sync {
            if f.result.is_none() {
                f.result = Some(result);
            }
            return;
        }
        let c = Completion {
            id: f.id,
            slot: f.slot,
            dci: f.dci,
            result,
        };
        self.inflight[idx] = None;
        self.done.push(c);
    }

    fn drain_events<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        let mut n = 0u32;
        loop {
            let Some(core) = self.core.as_mut() else {
                return Err(Error::NotRunning);
            };
            if n >= u32::from(core.evt.size()) {
                break;
            }
            let Some(t) = core.evt.pop(hw) else {
                break;
            };
            n += 1;
            self.stats.events += 1;
            self.handle_event(hw, Event::decode(t));
        }
        if n > 0 {
            if let Some(core) = self.core {
                hw.write64(self.caps.ir0(rt::ERDP), core.evt.erdp() | erdp::EHB);
            }
            let ie = if self.cfg.interrupts { iman::IE } else { 0 };
            hw.write32(self.caps.ir0(rt::IMAN), ie | iman::IP);
            hw.write32(self.caps.op(op::USBSTS), usbsts::EINT);
        }
        Ok(())
    }

    fn handle_event<H: Hal>(&mut self, hw: &mut H, ev: Event) {
        match ev {
            Event::CommandCompletion {
                trb,
                code,
                slot,
                param,
            } => {
                if self.cmd_pending == Some(trb) && self.cmd_result.is_none() {
                    if let Some(core) = self.core.as_mut() {
                        core.cmd.consumed(trb);
                    }
                    self.cmd_result = Some((code, slot, param));
                } else {
                    self.stats.stale_command_events += 1;
                }
            }
            Event::Transfer {
                trb,
                residual,
                code,
                slot,
                dci,
                event_data,
            } => {
                if event_data {
                    self.stats.stale_transfer_events += 1;
                } else {
                    self.on_transfer_event(hw, trb, residual, code, slot, dci);
                }
            }
            Event::PortStatusChange { port, .. } => {
                if port == 0 || port > self.caps.max_ports {
                    self.stats.other_events += 1;
                } else {
                    self.port_pending.set(port);
                    self.check_detach(hw, port);
                }
            }
            Event::HostController { .. } => self.stats.host_controller_events += 1,
            Event::Other { .. } => self.stats.other_events += 1,
        }
    }

    fn on_transfer_event<H: Hal>(
        &mut self,
        hw: &mut H,
        trb: u64,
        residual: u32,
        code: CompletionCode,
        slot: u8,
        dci: u8,
    ) {
        let found = self.inflight.iter().position(|f| {
            f.is_some_and(|f| {
                f.slot == slot && f.dci == dci && f.result.is_none() && f.td.contains(trb)
            })
        });
        let (Some(idx), Some(dev_idx)) = (found, self.dev_index(slot).ok()) else {
            self.stats.stale_transfer_events += 1;
            return;
        };
        let Some(f) = self.inflight[idx] else {
            return;
        };
        let out_ctx = {
            let Some(dev) = self.devices[dev_idx].as_mut() else {
                return;
            };
            let Some(ep) = dev.eps[usize::from(dci) - 1].as_mut() else {
                self.stats.stale_transfer_events += 1;
                return;
            };
            ep.ring.consumed(trb);
            dev.out_ctx
        };
        let last = trb == f.td.last();
        if code.is_success() {
            if last {
                self.complete(idx, Ok(f.short.unwrap_or(f.data_len)));
            }
        } else if code == CompletionCode::SHORT_PACKET {
            let actual = f.data_len.saturating_sub(residual);
            if last {
                self.complete(idx, Ok(actual));
            } else if let Some(e) = self.inflight[idx].as_mut() {
                e.short = Some(actual);
            }
        } else {
            let st =
                context::read_dwords::<_, 1>(hw, out_ctx.pa + self.layout.output_ep(dci))[0] & 7;
            let halted = u8::try_from(st).ok() == Some(ep_state::HALTED)
                || matches!(
                    code,
                    CompletionCode::STALL_ERROR
                        | CompletionCode::BABBLE_DETECTED
                        | CompletionCode::USB_TRANSACTION_ERROR
                        | CompletionCode::SPLIT_TRANSACTION
                );
            let port = self.devices[dev_idx].map_or(0, |d| d.port);
            let gone = port != 0 && !PortStatus::new(hw.read32(self.caps.portsc(port))).connected();
            if let Some(ep) = self.devices[dev_idx]
                .as_mut()
                .and_then(|d| d.eps[usize::from(dci) - 1].as_mut())
            {
                if halted {
                    ep.halted = true;
                    if !f.sync {
                        ep.resume_at = Some(f.td.end);
                    }
                }
            }
            let err = if gone {
                Error::Disconnected
            } else if code == CompletionCode::STALL_ERROR {
                Error::Stall
            } else {
                Error::Transfer(code)
            };
            if gone {
                self.detach_port(port);
            }
            self.complete(idx, Err(err));
        }
    }

    fn check_detach<H: Hal>(&mut self, hw: &mut H, port: u8) {
        let v = hw.read32(self.caps.portsc(port));
        if v == u32::MAX {
            return;
        }
        let st = PortStatus::new(v);
        if !st.connected() || !st.enabled() {
            self.detach_port(port);
        }
    }

    fn detach_port(&mut self, port: u8) {
        for i in 0..MAX_SLOTS {
            if self.devices[i].is_some_and(|d| d.port == port && d.state != DeviceState::Detached) {
                self.detach(i);
            }
        }
    }

    fn detach(&mut self, idx: usize) {
        let Some(d) = self.devices[idx].as_mut() else {
            return;
        };
        d.state = DeviceState::Detached;
        let slot = d.slot;
        for i in 0..MAX_INFLIGHT {
            if self.inflight[i].is_some_and(|f| f.slot == slot) {
                self.complete(i, Err(Error::Disconnected));
            }
        }
    }

    fn dev_index(&self, slot: u8) -> Result<usize, Error> {
        let idx = usize::from(slot).checked_sub(1).ok_or(Error::InvalidSlot)?;
        match self.devices.get(idx) {
            Some(Some(_)) => Ok(idx),
            _ => Err(Error::InvalidSlot),
        }
    }

    fn ring_doorbell<H: Hal>(&mut self, hw: &mut H, slot: u8, target: u8) {
        hw.write32(self.caps.doorbell(slot), u32::from(target));
    }

    fn alloc_id(&mut self) -> TransferId {
        let id = TransferId(self.next_id);
        self.next_id += 1;
        id
    }

    fn claim_inflight(&self) -> Result<usize, Error> {
        if self.pending_transfers() >= MAX_INFLIGHT {
            return Err(Error::TooManyTransfers);
        }
        self.inflight
            .iter()
            .position(Option::is_none)
            .ok_or(Error::TooManyTransfers)
    }

    // ----------------------------------------------------------------
    // Commands
    // ----------------------------------------------------------------

    /// Issues a No Op command.
    pub fn noop<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        self.command(hw, Command::NoOp).map(|_| ())
    }

    /// Runs one command to completion. Returns (slot ID, parameter). On
    /// timeout the command ring is aborted (CRCR.CA, §4.6.1.2) and
    /// restarted; if the abort itself times out the controller fails.
    fn command<H: Hal>(&mut self, hw: &mut H, cmd: Command) -> Result<(u8, u32), Error> {
        self.ensure_running()?;
        // A halted (HSE/HCE) controller must not see new work or doorbells.
        self.check_fatal(hw)?;
        if self.cmd_pending.is_some() {
            return Err(Error::BadRequest("command already pending"));
        }
        let Some(core) = self.core.as_mut() else {
            return Err(Error::NotRunning);
        };
        let td = core.cmd.push(hw, &[cmd.encode()])?;
        self.stats.commands += 1;
        self.cmd_pending = Some(td.trbs[0]);
        self.cmd_result = None;
        self.ring_doorbell(hw, 0, 0);
        let r = self.wait(hw, self.cfg.timeouts.command_us, Wait::Command, |s, hw| {
            s.check_fatal(hw)?;
            s.drain_events(hw)?;
            Ok(s.cmd_result.is_some())
        });
        match r {
            Ok(()) => {}
            Err(Error::Timeout(_)) => {
                self.abort_command(hw)?;
                // A completion that raced with the abort still counts.
                return match self.cmd_result.take() {
                    Some((code, slot, param))
                        if code != CompletionCode::COMMAND_ABORTED
                            && code != CompletionCode::COMMAND_RING_STOPPED =>
                    {
                        if code.is_success() {
                            Ok((slot, param))
                        } else {
                            Err(Error::Command(code))
                        }
                    }
                    _ => Err(Error::Timeout(Wait::Command)),
                };
            }
            Err(e) => {
                self.cmd_pending = None;
                return Err(e);
            }
        }
        self.cmd_pending = None;
        match self.cmd_result.take() {
            Some((code, slot, param)) if code.is_success() => Ok((slot, param)),
            Some((code, _, _)) => Err(Error::Command(code)),
            None => Err(Error::Timeout(Wait::Command)),
        }
    }

    fn abort_command<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        self.stats.command_aborts += 1;
        let off = self.caps.op(op::CRCR);
        hw.write64(off, crcr::CA);
        let r = self.wait(
            hw,
            self.cfg.timeouts.command_abort_us,
            Wait::CommandAbort,
            |s, hw| {
                s.check_fatal(hw)?;
                s.drain_events(hw)?;
                Ok(hw.read64(off) & crcr::CRR == 0)
            },
        );
        if let Err(e) = r {
            if matches!(e, Error::Timeout(_)) {
                self.fail(e);
            }
            self.cmd_pending = None;
            return Err(e);
        }
        self.drain_events(hw)?;
        self.cmd_pending = None;
        // CRR = 0, so CRCR accepts a new dequeue pointer (§5.4.5): restart
        // the ring from its base.
        let Some(core) = self.core.as_mut() else {
            return Err(Error::NotRunning);
        };
        core.cmd.reset(hw);
        hw.write64(off, core.cmd.base() | crcr::RCS);
        Ok(())
    }

    // ----------------------------------------------------------------
    // Ports
    // ----------------------------------------------------------------

    fn check_port(&self, port: u8) -> Result<(), Error> {
        if port == 0 || port > self.caps.max_ports {
            Err(Error::InvalidPort)
        } else {
            Ok(())
        }
    }

    /// Reads PORTSC of `port` without changing anything.
    pub fn port_status<H: Hal>(&mut self, hw: &mut H, port: u8) -> Result<PortStatus, Error> {
        self.check_port(port)?;
        let v = hw.read32(self.caps.portsc(port));
        if v == u32::MAX {
            return Err(Error::ControllerGone);
        }
        Ok(PortStatus::new(v))
    }

    /// Clears exactly the change bits present in one PORTSC read and
    /// records them for the next [`Notification::PortChanged`].
    fn ack_changes<H: Hal>(&mut self, hw: &mut H, port: u8) -> Result<PortStatus, Error> {
        let st = self.port_status(hw, port)?;
        let ch = st.changes();
        if ch != 0 {
            hw.write32(self.caps.portsc(port), port::clear_changes(st.raw, ch));
            self.port_changes[usize::from(port)] |= ch;
            self.port_notify.set(port);
        }
        Ok(st)
    }

    /// Resets `port` and waits until it is enabled. USB2 ports get a port
    /// reset (PR). USB3 ports enable themselves after link training; they
    /// are only warm-reset (WPR) when the link is in SS.Inactive or
    /// Compliance Mode or does not reach U0 in time (xHCI §4.19.1.2 (unverified)).
    /// A device previously enumerated on the port is detached.
    pub fn reset_port<H: Hal>(&mut self, hw: &mut H, port: u8) -> Result<PortStatus, Error> {
        self.ensure_running()?;
        self.check_port(port)?;
        self.detach_port(port);
        self.cleanup_detached(hw)?;
        let st = self.ack_changes(hw, port)?;
        if !st.connected() {
            return Err(Error::PortNotConnected);
        }
        if self.is_usb3_port(port) {
            if !(st.enabled() && st.link_state() == LinkState::U0) {
                let mut ok = false;
                if !matches!(st.link_state(), LinkState::Inactive | LinkState::Compliance) {
                    let r = self.wait(
                        hw,
                        self.cfg.timeouts.port_enable_us,
                        Wait::PortEnable,
                        |s, hw| {
                            s.check_fatal(hw)?;
                            s.drain_events(hw)?;
                            let p = s.port_status(hw, port)?;
                            Ok(!p.connected()
                                || (p.enabled() && p.link_state() == LinkState::U0)
                                || matches!(
                                    p.link_state(),
                                    LinkState::Inactive | LinkState::Compliance
                                ))
                        },
                    );
                    match r {
                        Ok(()) | Err(Error::Timeout(_)) => {}
                        Err(e) => return Err(e),
                    }
                    let p = self.port_status(hw, port)?;
                    if !p.connected() {
                        return Err(Error::PortNotConnected);
                    }
                    ok = p.enabled() && p.link_state() == LinkState::U0;
                }
                if !ok {
                    self.do_port_reset(hw, port, portsc::WPR, portsc::WRC)?;
                }
            }
        } else {
            self.do_port_reset(hw, port, portsc::PR, portsc::PRC)?;
        }
        let st = self.ack_changes(hw, port)?;
        if !st.connected() {
            return Err(Error::PortNotConnected);
        }
        if !st.enabled() {
            return Err(Error::PortNotEnabled);
        }
        Ok(st)
    }

    fn do_port_reset<H: Hal>(
        &mut self,
        hw: &mut H,
        port: u8,
        bit: u32,
        done: u32,
    ) -> Result<(), Error> {
        let off = self.caps.portsc(port);
        let raw = hw.read32(off);
        if raw == u32::MAX {
            return Err(Error::ControllerGone);
        }
        hw.write32(off, port::neutral(raw) | bit);
        self.wait(
            hw,
            self.cfg.timeouts.port_reset_us,
            Wait::PortReset,
            |s, hw| {
                s.check_fatal(hw)?;
                s.drain_events(hw)?;
                let p = s.port_status(hw, port)?;
                Ok(!p.connected()
                    || (p.raw & done != 0 && !p.in_reset() && p.raw & portsc::WPR == 0))
            },
        )
    }

    // ----------------------------------------------------------------
    // Devices
    // ----------------------------------------------------------------

    /// [`Controller::reset_port`] followed by [`Controller::enumerate`].
    pub fn attach<H: Hal>(&mut self, hw: &mut H, port: u8) -> Result<EnumeratedDevice, Error> {
        self.reset_port(hw, port)?;
        self.enumerate(hw, port)
    }

    /// Enumerates the device on an enabled port: Enable Slot, Address
    /// Device (optionally BSR = 1 first), GET_DESCRIPTOR(DEVICE, 8),
    /// Evaluate Context if bMaxPacketSize0 differs from the default,
    /// GET_DESCRIPTOR(DEVICE, 18). On failure the slot is disabled and its
    /// memory freed (or kept until [`Controller::recover`] if the
    /// controller failed).
    pub fn enumerate<H: Hal>(&mut self, hw: &mut H, port: u8) -> Result<EnumeratedDevice, Error> {
        self.ensure_running()?;
        self.check_port(port)?;
        let st = self.port_status(hw, port)?;
        if !st.connected() {
            return Err(Error::PortNotConnected);
        }
        if !st.enabled() {
            return Err(Error::PortNotEnabled);
        }
        if self
            .devices
            .iter()
            .flatten()
            .any(|d| d.port == port && d.state != DeviceState::Detached)
        {
            return Err(Error::BadRequest("port already has a device"));
        }
        let speed = st.speed().ok_or(Error::Unsupported("port speed ID"))?;
        let slot_type = self.protocols.for_port(port).map_or(0, |p| p.slot_type);
        let (slot, _) = self.command(hw, Command::EnableSlot { slot_type })?;
        if slot == 0 || slot > self.max_slots_en || self.devices[usize::from(slot) - 1].is_some() {
            return Err(Error::InvalidSlot);
        }
        let idx = usize::from(slot) - 1;
        match self.setup_device(hw, idx, slot, port, speed) {
            Ok(d) => Ok(d),
            Err(e) => {
                self.abandon_device(hw, idx, slot);
                Err(e)
            }
        }
    }

    fn abandon_device<H: Hal>(&mut self, hw: &mut H, idx: usize, slot: u8) {
        if self.state != State::Running {
            // Memory stays until the controller has been reset.
            if let Some(d) = self.devices[idx].as_mut() {
                d.state = DeviceState::Detached;
            }
            return;
        }
        match self.command(hw, Command::DisableSlot { slot }) {
            Ok(_) | Err(Error::Command(_)) => self.free_device(hw, idx),
            Err(_) => {
                if let Some(d) = self.devices[idx].as_mut() {
                    d.state = DeviceState::Detached;
                    d.disable_failed = true;
                }
            }
        }
    }

    fn setup_device<H: Hal>(
        &mut self,
        hw: &mut H,
        idx: usize,
        slot: u8,
        port: u8,
        speed: Speed,
    ) -> Result<EnumeratedDevice, Error> {
        let out_ctx = self.dma_alloc(hw, self.layout.output_len(), 4096)?;
        let in_ctx = match self.dma_alloc(hw, self.layout.input_len(), 4096) {
            Ok(b) => b,
            Err(e) => {
                hw.free(out_ctx.pa, out_ctx.len);
                return Err(e);
            }
        };
        let ep0 = match self.alloc_ring(hw) {
            Ok(r) => r,
            Err(e) => {
                hw.free(in_ctx.pa, in_ctx.len);
                hw.free(out_ctx.pa, out_ctx.len);
                return Err(e);
            }
        };
        let mut eps = [None; 31];
        eps[0] = Some(Endpoint {
            ring: ep0.0,
            buf: ep0.1,
            address: 0,
            halted: false,
            resume_at: None,
        });
        let mps0 = speed.default_ep0_mps();
        self.devices[idx] = Some(Device {
            slot,
            port,
            speed,
            out_ctx,
            in_ctx,
            eps,
            state: DeviceState::Enabled,
            ep0_mps: mps0,
            disable_failed: false,
        });
        // From here on the device record owns the memory.
        if let Some(core) = self.core {
            hw.write_u64(core.dcbaa.pa + 8 * u64::from(slot), out_ctx.pa);
        }
        self.write_ep0_input(hw, idx, 0b11, mps0)?;
        let mps = if self.cfg.address_with_bsr {
            self.command(
                hw,
                Command::AddressDevice {
                    slot,
                    input_context: in_ctx.pa,
                    bsr: true,
                },
            )?;
            self.set_state(idx, DeviceState::Default);
            let mps = self.read_ep0_mps(hw, slot, speed)?;
            self.write_ep0_input(hw, idx, 0b11, mps)?;
            self.command(
                hw,
                Command::AddressDevice {
                    slot,
                    input_context: in_ctx.pa,
                    bsr: false,
                },
            )?;
            mps
        } else {
            self.command(
                hw,
                Command::AddressDevice {
                    slot,
                    input_context: in_ctx.pa,
                    bsr: false,
                },
            )?;
            self.set_state(idx, DeviceState::Addressed);
            let mps = self.read_ep0_mps(hw, slot, speed)?;
            if mps != mps0 {
                self.write_ep0_input(hw, idx, 0b10, mps)?;
                self.command(
                    hw,
                    Command::EvaluateContext {
                        slot,
                        input_context: in_ctx.pa,
                    },
                )?;
            }
            mps
        };
        if let Some(d) = self.devices[idx].as_mut() {
            d.ep0_mps = mps;
            d.state = DeviceState::Addressed;
        }
        let mut buf = [0u8; descriptor::DEVICE_DESCRIPTOR_LEN];
        let n = self.control_in(
            hw,
            slot,
            SetupPacket::get_descriptor(dtype::DEVICE, 0, buf.len() as u16),
            &mut buf,
        )?;
        if n < buf.len() {
            return Err(Error::Descriptor(DescError::TooShort));
        }
        let desc = DeviceDescriptor::parse(&buf)?;
        if descriptor::ep0_max_packet(&buf, speed.is_superspeed())? != mps {
            return Err(Error::Descriptor(DescError::BadMaxPacket));
        }
        Ok(EnumeratedDevice {
            slot,
            port,
            speed,
            descriptor: desc,
            ep0_max_packet: mps,
        })
    }

    fn set_state(&mut self, idx: usize, s: DeviceState) {
        if let Some(d) = self.devices[idx].as_mut() {
            if d.state != DeviceState::Detached {
                d.state = s;
            }
        }
    }

    fn alloc_ring<H: Hal>(&self, hw: &mut H) -> Result<(ProducerRing, DmaBuf), Error> {
        let len = ProducerRing::bytes(self.cfg.transfer_ring_trbs);
        let buf = self.dma_alloc(hw, len, ring_align(len))?;
        match ProducerRing::new(buf.pa, self.cfg.transfer_ring_trbs) {
            Ok(mut r) => {
                r.reset(hw);
                Ok((r, buf))
            }
            Err(e) => {
                hw.free(buf.pa, buf.len);
                Err(e)
            }
        }
    }

    /// Writes an input context with the given add flags, a slot context
    /// for a root-port device and the EP0 context (dequeue pointer = the
    /// ring's current enqueue position).
    fn write_ep0_input<H: Hal>(
        &mut self,
        hw: &mut H,
        idx: usize,
        add: u32,
        mps: u16,
    ) -> Result<(), Error> {
        let d = self.devices[idx].ok_or(Error::InvalidSlot)?;
        let ring = d.eps[0].ok_or(Error::InvalidEndpoint)?.ring;
        let pos = ring.enqueue();
        let l = self.layout;
        hw.fill_zero(d.in_ctx.pa, l.input_len());
        let icc = InputControl {
            add,
            ..InputControl::default()
        };
        let sc = SlotContext {
            speed: d.speed.psi(),
            context_entries: 1,
            root_port: d.port,
            ..SlotContext::default()
        };
        let ec = EndpointContext {
            ep_type: ep_type::CONTROL,
            cerr: 3,
            max_packet: mps,
            dequeue: ring.pa_of(pos.index),
            dcs: pos.cycle,
            avg_trb_len: 8,
            ..EndpointContext::default()
        };
        context::write_dwords(hw, d.in_ctx.pa, &icc.encode());
        context::write_dwords(hw, d.in_ctx.pa + l.input_slot(), &sc.encode());
        context::write_dwords(hw, d.in_ctx.pa + l.input_ep(1), &ec.encode());
        Ok(())
    }

    fn read_ep0_mps<H: Hal>(&mut self, hw: &mut H, slot: u8, speed: Speed) -> Result<u16, Error> {
        let mut b = [0u8; 8];
        let n = self.control_in(
            hw,
            slot,
            SetupPacket::get_descriptor(dtype::DEVICE, 0, 8),
            &mut b,
        )?;
        if n < 8 {
            return Err(Error::Descriptor(DescError::TooShort));
        }
        Ok(descriptor::ep0_max_packet(&b, speed.is_superspeed())?)
    }

    /// Reads configuration descriptor set `index` into `buf` (header
    /// first, then wTotalLength bytes) and validates every descriptor.
    /// Returns wTotalLength.
    pub fn read_configuration<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        index: u8,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let mut h = [0u8; descriptor::CONFIG_DESCRIPTOR_LEN];
        let n = self.control_in(
            hw,
            slot,
            SetupPacket::get_descriptor(dtype::CONFIGURATION, index, h.len() as u16),
            &mut h,
        )?;
        let hdr = ConfigDescriptor::parse_header(&h[..n])?;
        let total = usize::from(hdr.total_length);
        if total > buf.len() || total > CONTROL_BUFFER_LEN {
            return Err(Error::BufferTooSmall);
        }
        let n = self.control_in(
            hw,
            slot,
            SetupPacket::get_descriptor(dtype::CONFIGURATION, index, total as u16),
            &mut buf[..total],
        )?;
        if n != total {
            return Err(Error::Descriptor(DescError::BadTotalLength));
        }
        let (h2, walker) = descriptor::parse_configuration(&buf[..n])?;
        if h2.total_length != hdr.total_length {
            return Err(Error::Descriptor(DescError::BadTotalLength));
        }
        for d in walker {
            d?;
        }
        Ok(n)
    }

    /// Configures an interrupt IN endpoint (Configure Endpoint command,
    /// xHCI §4.6.6) and selects `config_value` with SET_CONFIGURATION.
    /// Returns the endpoint's DCI for [`Controller::submit_interrupt_in`].
    pub fn configure_interrupt_in<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        config_value: u8,
        ep: &EndpointDescriptor,
        companion: Option<&SsCompanion>,
    ) -> Result<u8, Error> {
        self.ensure_running()?;
        let idx = self.dev_index(slot)?;
        let d = self.devices[idx].ok_or(Error::InvalidSlot)?;
        match d.state {
            DeviceState::Addressed | DeviceState::Configured => {}
            DeviceState::Detached => return Err(Error::Disconnected),
            _ => return Err(Error::BadRequest("device not addressed")),
        }
        if !ep.is_in() || ep.transfer_type() != TransferType::Interrupt || ep.number() == 0 {
            return Err(Error::InvalidEndpoint);
        }
        let mps = ep.max_packet();
        let limit = match d.speed {
            Speed::Low => 8,
            Speed::Full => 64,
            _ => 1024,
        };
        if mps == 0 || mps > limit {
            return Err(Error::Descriptor(DescError::BadMaxPacket));
        }
        let interval = context::interrupt_interval(d.speed, ep.interval)?;
        let (burst, esit) = if d.speed.is_superspeed() {
            let b = companion.map_or(0, |c| c.max_burst);
            if b > 15 {
                return Err(Error::Descriptor(DescError::BadMaxPacket));
            }
            let esit = companion.map_or(u32::from(mps) * (u32::from(b) + 1), |c| {
                u32::from(c.bytes_per_interval)
            });
            (b, esit)
        } else if d.speed == Speed::High {
            let b = ep.hs_extra_transactions();
            if b > 2 {
                return Err(Error::Descriptor(DescError::BadMaxPacket));
            }
            (b, u32::from(mps) * (u32::from(b) + 1))
        } else {
            (0, u32::from(mps))
        };
        let dci = context::dci(ep.number(), true);
        if d.eps[usize::from(dci) - 1].is_some() {
            return Err(Error::BadRequest("endpoint already configured"));
        }
        let (ring, rbuf) = self.alloc_ring(hw)?;
        let l = self.layout;
        let out_slot: [u32; 4] = context::read_dwords(hw, d.out_ctx.pa);
        let mut sc = SlotContext::decode(out_slot);
        sc.context_entries = sc.context_entries.max(dci);
        sc.slot_state = 0;
        sc.usb_address = 0;
        let icc = InputControl {
            add: 1 | 1 << dci,
            config_value,
            ..InputControl::default()
        };
        let ec = EndpointContext {
            ep_type: ep_type::INTERRUPT_IN,
            cerr: 3,
            max_packet: mps,
            max_burst: burst,
            interval,
            max_esit_payload: esit,
            dequeue: ring.base(),
            dcs: true,
            avg_trb_len: mps,
            ..EndpointContext::default()
        };
        hw.fill_zero(d.in_ctx.pa, l.input_len());
        context::write_dwords(hw, d.in_ctx.pa, &icc.encode());
        context::write_dwords(hw, d.in_ctx.pa + l.input_slot(), &sc.encode());
        context::write_dwords(hw, d.in_ctx.pa + l.input_ep(dci), &ec.encode());
        let cmd = Command::ConfigureEndpoint {
            slot,
            input_context: d.in_ctx.pa,
            deconfigure: false,
        };
        if let Err(e) = self.command(hw, cmd) {
            // On a failed command the controller did not take the ring. A
            // command timeout leaves it unknown; keep the memory then.
            if matches!(e, Error::Command(_)) {
                hw.free(rbuf.pa, rbuf.len);
            }
            return Err(e);
        }
        if let Some(dev) = self.devices[idx].as_mut() {
            dev.eps[usize::from(dci) - 1] = Some(Endpoint {
                ring,
                buf: rbuf,
                address: ep.address,
                halted: false,
                resume_at: None,
            });
        }
        if let Err(e) =
            self.control_out(hw, slot, SetupPacket::set_configuration(config_value), &[])
        {
            self.drop_endpoint(hw, idx, slot, dci, sc);
            return Err(e);
        }
        self.set_state(idx, DeviceState::Configured);
        Ok(dci)
    }

    fn drop_endpoint<H: Hal>(
        &mut self,
        hw: &mut H,
        idx: usize,
        slot: u8,
        dci: u8,
        sc: SlotContext,
    ) {
        let Some(d) = self.devices[idx] else {
            return;
        };
        if self.state != State::Running || d.state == DeviceState::Detached {
            return;
        }
        let l = self.layout;
        let icc = InputControl {
            drop: 1 << dci,
            add: 1,
            ..InputControl::default()
        };
        hw.fill_zero(d.in_ctx.pa, l.input_len());
        context::write_dwords(hw, d.in_ctx.pa, &icc.encode());
        context::write_dwords(hw, d.in_ctx.pa + l.input_slot(), &sc.encode());
        let cmd = Command::ConfigureEndpoint {
            slot,
            input_context: d.in_ctx.pa,
            deconfigure: false,
        };
        if self.command(hw, cmd).is_ok() {
            if let Some(ep) = self.devices[idx]
                .as_mut()
                .and_then(|d| d.eps[usize::from(dci) - 1].take())
            {
                hw.free(ep.buf.pa, ep.buf.len);
            }
        }
    }

    /// Releases a device: pending transfers fail with
    /// [`Error::Disconnected`], the slot is disabled and its memory freed.
    pub fn disable_device<H: Hal>(&mut self, hw: &mut H, slot: u8) -> Result<(), Error> {
        let idx = self.dev_index(slot)?;
        self.detach(idx);
        self.cleanup_detached(hw)
    }

    fn cleanup_detached<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        self.ensure_running()?;
        for i in 0..MAX_SLOTS {
            let Some(d) = self.devices[i] else {
                continue;
            };
            if d.state != DeviceState::Detached || d.disable_failed {
                continue;
            }
            match self.command(hw, Command::DisableSlot { slot: d.slot }) {
                Ok(_) | Err(Error::Command(_)) => {
                    self.free_device(hw, i);
                    self.detached_notify[i] = Some(d.port);
                }
                Err(Error::Timeout(Wait::Command)) => {
                    if let Some(d) = self.devices[i].as_mut() {
                        d.disable_failed = true;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------------
    // Transfers
    // ----------------------------------------------------------------

    /// Control transfer with an IN data stage (or none if `setup.length`
    /// is 0). Returns the bytes received, copied into `buf`.
    pub fn control_in<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        setup: SetupPacket,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        if setup.length > 0 && !setup.is_in() {
            return Err(Error::BadRequest("control_in with OUT setup"));
        }
        if usize::from(setup.length) > buf.len() {
            return Err(Error::BufferTooSmall);
        }
        let n = (self.control_raw(hw, slot, setup, &[])? as usize).min(usize::from(setup.length));
        let ctrl = self.core.ok_or(Error::NotRunning)?.ctrl.pa;
        hw.read(ctrl, &mut buf[..n]);
        Ok(n)
    }

    /// Control transfer with an OUT data stage (or none).
    pub fn control_out<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        setup: SetupPacket,
        data: &[u8],
    ) -> Result<(), Error> {
        if setup.length > 0 && setup.is_in() {
            return Err(Error::BadRequest("control_out with IN setup"));
        }
        if data.len() != usize::from(setup.length) {
            return Err(Error::BadRequest("data length differs from wLength"));
        }
        self.control_raw(hw, slot, setup, data).map(|_| ())
    }

    fn control_raw<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        setup: SetupPacket,
        out: &[u8],
    ) -> Result<u32, Error> {
        self.ensure_running()?;
        self.check_fatal(hw)?;
        let idx = self.dev_index(slot)?;
        let d = self.devices[idx].ok_or(Error::InvalidSlot)?;
        if d.state == DeviceState::Detached {
            return Err(Error::Disconnected);
        }
        let ep = d.eps[0].ok_or(Error::InvalidEndpoint)?;
        if ep.halted {
            return Err(Error::EndpointHalted);
        }
        let len = u32::from(setup.length);
        if len as usize > CONTROL_BUFFER_LEN {
            return Err(Error::BadRequest("control transfer too long"));
        }
        let ctrl = self.core.ok_or(Error::NotRunning)?.ctrl.pa;
        let data_in = setup.is_in();
        let trt = if len == 0 {
            Trt::NoData
        } else if data_in {
            Trt::In
        } else {
            Trt::Out
        };
        let mut trbs = [Trb::default(); 3];
        let mut n = 0;
        trbs[n] = Trb::setup_stage(&setup, trt, 0);
        n += 1;
        if len > 0 {
            trbs[n] = Trb::data_stage(ctrl, len, data_in, if data_in { flag::ISP } else { 0 });
            n += 1;
        }
        // Status stage direction is opposite to the data stage; IN when
        // there is no data stage (xHCI §4.11.2.2).
        trbs[n] = Trb::status_stage(len == 0 || !data_in, flag::IOC);
        n += 1;
        let fi = self.claim_inflight()?;
        if !out.is_empty() {
            hw.write(ctrl, out);
        }
        let td = {
            let Some(ep) = self.devices[idx].as_mut().and_then(|d| d.eps[0].as_mut()) else {
                return Err(Error::InvalidEndpoint);
            };
            ep.ring.push(hw, &trbs[..n])?
        };
        let id = self.alloc_id();
        self.inflight[fi] = Some(InFlight {
            id,
            slot,
            dci: 1,
            td,
            data_len: len,
            short: None,
            sync: true,
            result: None,
        });
        self.ring_doorbell(hw, slot, 1);
        let r = self.wait(
            hw,
            self.cfg.timeouts.transfer_us,
            Wait::Transfer,
            |s, hw| {
                s.check_fatal(hw)?;
                s.drain_events(hw)?;
                Ok(s.inflight[fi].is_none_or(|f| f.result.is_some()))
            },
        );
        let entry = self.inflight[fi].take();
        let result = match r {
            Ok(()) => entry
                .and_then(|e| e.result)
                .unwrap_or(Err(Error::ControllerReset)),
            Err(Error::Timeout(_)) => {
                self.cancel_td(hw, slot, 1, td.end)?;
                return Err(Error::Timeout(Wait::Transfer));
            }
            Err(e) => return Err(e),
        };
        if result.is_err() {
            let halted = self.devices[idx]
                .and_then(|d| {
                    if d.state == DeviceState::Detached {
                        None
                    } else {
                        d.eps[0]
                    }
                })
                .is_some_and(|e| e.halted);
            if halted {
                self.recover_endpoint(hw, slot, 1, td.end)?;
            }
        }
        result
    }

    /// Stops an endpoint whose TD did not complete and moves its dequeue
    /// pointer past that TD.
    fn cancel_td<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        dci: u8,
        end: RingPos,
    ) -> Result<(), Error> {
        let idx = self.dev_index(slot)?;
        if self.devices[idx].is_none_or(|d| d.state == DeviceState::Detached) {
            return Ok(());
        }
        match self.command(
            hw,
            Command::StopEndpoint {
                slot,
                dci,
                suspend: false,
            },
        ) {
            Ok(_) | Err(Error::Command(CompletionCode::CONTEXT_STATE_ERROR)) => {}
            Err(e) => return Err(e),
        }
        self.set_dequeue(hw, idx, slot, dci, end)
    }

    fn set_dequeue<H: Hal>(
        &mut self,
        hw: &mut H,
        idx: usize,
        slot: u8,
        dci: u8,
        pos: RingPos,
    ) -> Result<(), Error> {
        let ring = self.devices[idx]
            .and_then(|d| d.eps[usize::from(dci) - 1])
            .ok_or(Error::InvalidEndpoint)?
            .ring;
        let cmd = Command::SetTrDequeue {
            slot,
            dci,
            dequeue: ring.pa_of(pos.index),
            cycle: pos.cycle,
        };
        self.command(hw, cmd)?;
        if let Some(ep) = self.devices[idx]
            .as_mut()
            .and_then(|d| d.eps[usize::from(dci) - 1].as_mut())
        {
            ep.ring.set_dequeue(pos.index);
        }
        Ok(())
    }

    /// Recovers a halted endpoint (xHCI §4.6.8, §4.6.10): Reset Endpoint,
    /// Set TR Dequeue Pointer past the failed TD, and for non-control
    /// endpoints CLEAR_FEATURE(ENDPOINT_HALT) so the device resets its
    /// data toggle too; then restarts queued TDs.
    fn recover_endpoint<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        dci: u8,
        pos: RingPos,
    ) -> Result<(), Error> {
        self.stats.endpoint_recoveries += 1;
        let idx = self.dev_index(slot)?;
        if let Some(ep) = self.devices[idx]
            .as_mut()
            .and_then(|d| d.eps[usize::from(dci) - 1].as_mut())
        {
            ep.resume_at = None;
        }
        match self.command(
            hw,
            Command::ResetEndpoint {
                slot,
                dci,
                preserve: false,
            },
        ) {
            Ok(_) => {}
            Err(Error::Command(CompletionCode::CONTEXT_STATE_ERROR)) => {
                // Not halted after all: stop it instead so the dequeue
                // pointer can be moved.
                match self.command(
                    hw,
                    Command::StopEndpoint {
                        slot,
                        dci,
                        suspend: false,
                    },
                ) {
                    Ok(_) | Err(Error::Command(CompletionCode::CONTEXT_STATE_ERROR)) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
        self.set_dequeue(hw, idx, slot, dci, pos)?;
        let addr = match self.devices[idx]
            .as_mut()
            .and_then(|d| d.eps[usize::from(dci) - 1].as_mut())
        {
            Some(ep) => {
                ep.halted = false;
                ep.address
            }
            None => return Ok(()),
        };
        if dci != 1 {
            match self.control_out(hw, slot, SetupPacket::clear_endpoint_halt(addr), &[]) {
                Ok(()) | Err(Error::Stall) | Err(Error::Transfer(_)) => {}
                Err(e) => return Err(e),
            }
            if self
                .inflight
                .iter()
                .flatten()
                .any(|f| f.slot == slot && f.dci == dci)
            {
                self.ring_doorbell(hw, slot, dci);
            }
        }
        Ok(())
    }

    /// Queues an interrupt IN transfer of `len` bytes into the caller's
    /// DMA buffer at `buf_pa` (must not cross a 64 KiB boundary, xHCI
    /// §6.4.1). The result arrives through [`Controller::poll`].
    pub fn submit_interrupt_in<H: Hal>(
        &mut self,
        hw: &mut H,
        slot: u8,
        dci: u8,
        buf_pa: u64,
        len: u32,
    ) -> Result<TransferId, Error> {
        self.ensure_running()?;
        if len == 0 || len > 0x1_0000 {
            return Err(Error::BadRequest("transfer length"));
        }
        let end = buf_pa
            .checked_add(u64::from(len) - 1)
            .ok_or(Error::BadDmaAddress)?;
        if buf_pa >> 16 != end >> 16 {
            return Err(Error::BadRequest("buffer crosses a 64 KiB boundary"));
        }
        if !self.caps.ac64 && end > u64::from(u32::MAX) {
            return Err(Error::BadDmaAddress);
        }
        let idx = self.dev_index(slot)?;
        let d = self.devices[idx].ok_or(Error::InvalidSlot)?;
        if d.state == DeviceState::Detached {
            return Err(Error::Disconnected);
        }
        if !(2..=31).contains(&dci) || dci.is_multiple_of(2) {
            return Err(Error::InvalidEndpoint);
        }
        let ep = d.eps[usize::from(dci) - 1].ok_or(Error::InvalidEndpoint)?;
        if ep.halted || ep.resume_at.is_some() {
            return Err(Error::EndpointHalted);
        }
        self.check_fatal(hw)?;
        let fi = self.claim_inflight()?;
        let td = {
            let Some(ep) = self.devices[idx]
                .as_mut()
                .and_then(|d| d.eps[usize::from(dci) - 1].as_mut())
            else {
                return Err(Error::InvalidEndpoint);
            };
            ep.ring
                .push(hw, &[Trb::normal(buf_pa, len, flag::ISP | flag::IOC)])?
        };
        let id = self.alloc_id();
        self.inflight[fi] = Some(InFlight {
            id,
            slot,
            dci,
            td,
            data_len: len,
            short: None,
            sync: false,
            result: None,
        });
        self.ring_doorbell(hw, slot, dci);
        Ok(id)
    }

    /// Processes events and deferred work and returns the next
    /// notification. Completions queued before a failure are delivered
    /// first; afterwards a failed controller reports its error until
    /// [`Controller::recover`].
    pub fn poll<H: Hal>(&mut self, hw: &mut H) -> Result<Option<Notification>, Error> {
        if let Some(c) = self.done.pop() {
            return Ok(Some(Notification::Transfer(c)));
        }
        if self.state == State::Running {
            self.check_fatal(hw)?;
            self.drain_events(hw)?;
            self.deferred(hw)?;
            if let Some(c) = self.done.pop() {
                return Ok(Some(Notification::Transfer(c)));
            }
        }
        for i in 0..MAX_SLOTS {
            if let Some(port) = self.detached_notify[i].take() {
                return Ok(Some(Notification::DeviceDetached {
                    slot: i as u8 + 1,
                    port,
                }));
            }
        }
        if let Some(port) = self.port_notify.take_first() {
            let raw = hw.read32(self.caps.portsc(port));
            if raw == u32::MAX {
                return Err(Error::ControllerGone);
            }
            let ch = core::mem::take(&mut self.port_changes[usize::from(port)]);
            return Ok(Some(Notification::PortChanged {
                port,
                status: PortStatus::new(raw | ch),
            }));
        }
        match self.state {
            State::Failed(e) => Err(e),
            _ => Ok(None),
        }
    }

    fn deferred<H: Hal>(&mut self, hw: &mut H) -> Result<(), Error> {
        while let Some(port) = self.port_pending.take_first() {
            self.ack_changes(hw, port)?;
            self.check_detach(hw, port);
            self.port_notify.set(port);
        }
        self.cleanup_detached(hw)?;
        for i in 0..MAX_SLOTS {
            for e in 1..31usize {
                let Some(d) = self.devices[i] else {
                    break;
                };
                if d.state == DeviceState::Detached {
                    break;
                }
                let Some(pos) = d.eps[e].and_then(|ep| ep.resume_at) else {
                    continue;
                };
                if let Err(err) = self.recover_endpoint(hw, d.slot, e as u8 + 1, pos) {
                    if self.state != State::Running {
                        return Err(err);
                    }
                }
            }
        }
        Ok(())
    }
}

fn fatal_status(s: u32) -> Result<(), Error> {
    if s == u32::MAX {
        Err(Error::ControllerGone)
    } else if s & usbsts::HSE != 0 {
        Err(Error::HostSystemError)
    } else if s & usbsts::HCE != 0 {
        Err(Error::HostControllerError)
    } else {
        Ok(())
    }
}

fn free_scratchpad_pages<H: Hal>(hw: &mut H, arr: DmaBuf, count: u16) {
    for i in 0..count {
        let pa = hw.read_u64(arr.pa + 8 * u64::from(i));
        if pa != 0 {
            hw.free(pa, 4096);
        }
    }
}
