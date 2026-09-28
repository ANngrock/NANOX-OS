//! Controller bring-up, I/O, recovery and shutdown.
//!
//! Initialisation (NVMe Base 2.0 §3.5.1 "Memory-based Controller
//! Initialization" (?)) is a state machine advanced by [`Controller::step`]:
//!
//! 1. `Disabling`: if CC.EN = 1 and CSTS.RDY = 0 wait (CAP.TO) for RDY to
//!    match EN, then clear CC.EN and wait (CAP.TO) for CSTS.RDY = 0. Once
//!    RDY = 0 the controller has stopped processing and DMA, and every
//!    outstanding caller command is completed with [`Outcome::Reset`].
//! 2. Admin queue registers (AQA, ASQ, ACQ) and CC (NVM command set,
//!    4 KiB pages, round robin, IOSQES = 6, IOCQES = 4) with CC.EN = 1.
//! 3. `Enabling`: wait (CAP.TO) for CSTS.RDY = 1; CSTS.CFS = 1 fails.
//! 4. `Admin` steps, one command at a time with the admin timeout:
//!    Identify Controller, Identify Namespace, Set Features (Number of
//!    Queues), Create I/O CQ, Create I/O SQ. CSTS.CFS = 1 fails.
//!
//! In `Ready`, [`Controller::submit`] queues I/O and [`Controller::poll`]
//! reaps completions, checks CSTS and command deadlines. A command that
//! exceeds the I/O timeout gets an Abort (within the controller's Abort
//! Command Limit); if it has not completed `abort_timeout_ns` later, or the
//! Abort itself times out, or CSTS.CFS = 1, the controller enters
//! `NeedsReset` and the caller runs [`Controller::reset`], which disables
//! the controller, completes every outstanding command exactly once with
//! [`Outcome::Reset`] and initialises again. If even the disable times out
//! the controller may still be doing DMA; the commands stay outstanding
//! until the caller has stopped the device by other means (Bus Master
//! Enable, function reset) and calls [`Controller::abandon`].
//!
//! Every command accepted by [`Controller::submit`] is reported exactly
//! once through a sink closure of `poll`, `reset`/`step` or `abandon`.
//!
//! Shutdown (NVMe Base 2.0 §3.6 (?)): delete the I/O SQ and CQ, set
//! CC.SHN = 01b and wait for CSTS.SHST = 10b.

use crate::command::{cns, Command, CompletionEntry};
use crate::identify::{ControllerInfo, IdentifyError, NamespaceInfo, IDENTIFY_PARSE_LEN};
use crate::prp::{self, DataBuffer, PrpError, PrpList};
use crate::queue::{CompletionError, CompletionRing, Kind, SlotState, Slots, SubmissionRing};
use crate::regs::{self, cc, CapError, Capabilities, ControllerStatus, ShutdownStatus, Version};
use crate::status::Status;
use crate::{
    ConfigError, Error, Hardware, TimeoutPhase, Unsupported, PAGE_SIZE, PHYS_ADDRESS_LIMIT,
};

/// Admin commands tracked at once.
pub const ADMIN_SLOTS: usize = 8;
/// I/O commands tracked at once.
pub const IO_SLOTS: usize = 64;
/// Queue identifier of the single I/O queue pair.
pub const IO_QID: u16 = 1;
/// Largest `prp_pages_per_command` accepted.
pub const MAX_PRP_LIST_PAGES: u32 = 64;
/// Largest transfer the driver ever builds.
const TRANSFER_CAP: u64 = 1 << 31;

/// Physically contiguous memory of one queue pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueMemory {
    /// 4 KiB aligned submission queue, `entries * 64` bytes.
    pub sq: u64,
    /// 4 KiB aligned completion queue, `entries * 16` bytes.
    pub cq: u64,
    /// Entries of each queue.
    pub entries: u32,
}

/// Memory and limits the driver works with. All addresses are physical
/// (bus) addresses of memory the caller owns for the controller's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Size of BAR0 in bytes.
    pub bar_size: u64,
    /// Admin queue pair, 2..=4096 entries.
    pub admin: QueueMemory,
    /// I/O queue pair (qid 1), 2..=CAP.MQES+1 entries.
    pub io: QueueMemory,
    /// 4 KiB aligned page receiving Identify data.
    pub identify_buffer: u64,
    /// 4 KiB aligned PRP list pool of [`Config::prp_pool_bytes`] bytes.
    pub prp_pool: u64,
    /// PRP list pages reserved per I/O command (0..=64).
    pub prp_pages_per_command: u32,
    /// Namespace used for I/O.
    pub nsid: u32,
    /// MSI-X vector for the I/O CQ, or `None` for a polled queue.
    pub io_vector: Option<u16>,
    /// Bound for each admin command.
    pub admin_timeout_ns: u64,
    /// Bound for each I/O command before it is aborted.
    pub io_timeout_ns: u64,
    /// Time an aborted command gets to complete before a reset.
    pub abort_timeout_ns: u64,
    /// Minimum bound for shutdown (Identify RTD3E raises it).
    pub shutdown_timeout_ns: u64,
}

impl Config {
    /// I/O commands tracked at once for this configuration.
    #[must_use]
    pub fn io_slots(&self) -> usize {
        (self.io.entries.saturating_sub(1) as usize).min(IO_SLOTS)
    }

    /// Bytes of the PRP list pool.
    #[must_use]
    pub fn prp_pool_bytes(&self) -> u64 {
        self.io_slots() as u64 * u64::from(self.prp_pages_per_command) * PAGE_SIZE
    }
}

/// Admin commands the driver issues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminStep {
    /// Identify, CNS 01h.
    IdentifyController,
    /// Identify, CNS 00h.
    IdentifyNamespace,
    /// Set Features, Number of Queues.
    SetNumQueues,
    /// Create I/O Completion Queue.
    CreateIoCq,
    /// Create I/O Submission Queue.
    CreateIoSq,
    /// Delete I/O Submission Queue (shutdown).
    DeleteIoSq,
    /// Delete I/O Completion Queue (shutdown).
    DeleteIoCq,
}

/// Why the controller needs a reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetReason {
    /// CSTS.CFS = 1.
    ControllerFatal,
    /// CSTS read all ones.
    DeviceGone,
    /// A command did not complete within its timeout plus the abort window.
    CommandTimeout,
    /// An Abort command did not complete.
    AbortTimeout,
}

/// Externally visible state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Created, never initialised.
    Idle,
    /// Waiting for CSTS.RDY = 0.
    Disabling,
    /// Waiting for CSTS.RDY = 1.
    Enabling,
    /// Running an initialisation admin command.
    Admin(AdminStep),
    /// Accepting I/O.
    Ready,
    /// I/O stopped until [`Controller::reset`].
    NeedsReset(ResetReason),
    /// Shutdown completed.
    ShutDown,
    /// Initialisation or shutdown failed.
    Failed,
}

/// Result of one [`Controller::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Call `step` again.
    Pending,
    /// Initialisation finished.
    Ready,
}

/// How a caller command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Completed successfully.
    Success {
        /// Completion dword 0.
        dw0: u32,
    },
    /// Completed with an error status.
    Error(Status),
    /// Timed out and was aborted by the controller.
    TimedOut,
    /// Cancelled by a controller reset (the controller was disabled first).
    Reset,
    /// Given up by [`Controller::abandon`].
    Abandoned,
}

/// Completion of a caller command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Tag passed to [`Controller::submit`].
    pub tag: u64,
    /// Result.
    pub outcome: Outcome,
}

/// An I/O request on the configured namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    /// Read `blocks` logical blocks starting at `lba` into `buffer`.
    Read {
        /// First logical block.
        lba: u64,
        /// Number of blocks, 1..=65536.
        blocks: u32,
        /// Destination; `len` must equal `blocks * block size`.
        buffer: DataBuffer<'a>,
    },
    /// Write `blocks` logical blocks starting at `lba` from `buffer`.
    Write {
        /// First logical block.
        lba: u64,
        /// Number of blocks, 1..=65536.
        blocks: u32,
        /// Source; `len` must equal `blocks * block size`.
        buffer: DataBuffer<'a>,
    },
    /// Flush the volatile write cache.
    Flush,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Start,
    WaitReadyMatch {
        deadline: u64,
    },
    WaitDisabled {
        deadline: u64,
    },
    WaitEnabled {
        deadline: u64,
    },
    Admin {
        step: AdminStep,
        cid: Option<u16>,
        deadline: u64,
    },
    Ready,
    NeedsReset(ResetReason),
    ShutDown,
    Failed,
}

/// Driver state of one NVMe controller.
#[derive(Clone, Debug)]
pub struct Controller {
    cap: Capabilities,
    version: Version,
    cfg: Config,
    state: State,
    admin_sq: SubmissionRing,
    admin_cq: CompletionRing,
    admin_slots: Slots<ADMIN_SLOTS>,
    io_sq: SubmissionRing,
    io_cq: CompletionRing,
    io_slots: Slots<IO_SLOTS>,
    ctrl: Option<ControllerInfo>,
    ns: Option<NamespaceInfo>,
    abort_limit: usize,
    aborts: usize,
    max_transfer: u32,
}

fn aligned(pa: u64) -> bool {
    pa.is_multiple_of(PAGE_SIZE)
}

/// `[base, base + len)` lies below [`PHYS_ADDRESS_LIMIT`] without wrapping.
fn span_ok(base: u64, len: u64) -> bool {
    base.checked_add(len)
        .is_some_and(|end| end <= PHYS_ADDRESS_LIMIT)
}

fn region_ok(base: u64, entries: u32, entry_size: u64) -> bool {
    u64::from(entries)
        .checked_mul(entry_size)
        .is_some_and(|len| span_ok(base, len))
}

fn check_config(cap: Capabilities, cfg: &Config) -> Result<(), ConfigError> {
    if !(2..=4096).contains(&cfg.admin.entries) {
        return Err(ConfigError::AdminEntries);
    }
    if cfg.io.entries < 2 || cfg.io.entries > cap.max_queue_entries() {
        return Err(ConfigError::IoEntries);
    }
    let bases = [
        cfg.admin.sq,
        cfg.admin.cq,
        cfg.io.sq,
        cfg.io.cq,
        cfg.identify_buffer,
        cfg.prp_pool,
    ];
    if !bases.iter().all(|&b| aligned(b)) {
        return Err(ConfigError::Alignment);
    }
    if !region_ok(cfg.admin.sq, cfg.admin.entries, 64)
        || !region_ok(cfg.admin.cq, cfg.admin.entries, 16)
        || !region_ok(cfg.io.sq, cfg.io.entries, 64)
        || !region_ok(cfg.io.cq, cfg.io.entries, 16)
        || !span_ok(cfg.identify_buffer, PAGE_SIZE)
    {
        return Err(ConfigError::Region);
    }
    if cfg.nsid == 0 || cfg.nsid == u32::MAX {
        return Err(ConfigError::Namespace);
    }
    if cfg.admin_timeout_ns == 0
        || cfg.io_timeout_ns == 0
        || cfg.abort_timeout_ns == 0
        || cfg.shutdown_timeout_ns == 0
    {
        return Err(ConfigError::Timeout);
    }
    if cfg.prp_pages_per_command > MAX_PRP_LIST_PAGES
        || !span_ok(cfg.prp_pool, cfg.prp_pool_bytes())
    {
        return Err(ConfigError::PrpPool);
    }
    // The CQ head doorbell of the I/O queue is the highest offset used.
    let last = cap.cq_head_doorbell(IO_QID) + 4;
    if last > cfg.bar_size || last > u64::from(u32::MAX) {
        return Err(ConfigError::DoorbellOutsideBar);
    }
    Ok(())
}

/// Validates a completion entry against a queue pair without changing it.
fn check_entry<const N: usize>(
    sq: &SubmissionRing,
    slots: &Slots<N>,
    cqe: &CompletionEntry,
    qid: u16,
) -> Result<usize, CompletionError> {
    if cqe.sq_id != qid {
        return Err(CompletionError::WrongQueue {
            expected: qid,
            got: cqe.sq_id,
        });
    }
    sq.check_head(cqe.sq_head)?;
    slots.lookup(cqe.cid)
}

impl Controller {
    /// Reads and validates CAP and VS and checks `cfg`. Only reads
    /// registers.
    pub fn new<H: Hardware + ?Sized>(hw: &mut H, cfg: Config) -> Result<Self, Error> {
        let cap = Capabilities::parse(hw.read64(regs::CAP)).map_err(|e| match e {
            CapError::AllOnes => Error::DeviceGone,
            e => Error::InvalidCapabilities(e),
        })?;
        let vs = hw.read32(regs::VS);
        if vs == u32::MAX {
            return Err(Error::DeviceGone);
        }
        let version = Version::from_raw(vs);
        if version.major() == 0 {
            return Err(Error::Unsupported(Unsupported::Version));
        }
        if !cap.nvm_command_set() {
            return Err(Error::Unsupported(Unsupported::CommandSet));
        }
        if cap.mpsmin() > 0 {
            return Err(Error::Unsupported(Unsupported::PageSize));
        }
        check_config(cap, &cfg).map_err(Error::Config)?;
        let mut admin_slots = Slots::new();
        admin_slots.set_limit((cfg.admin.entries as usize - 1).min(ADMIN_SLOTS));
        let mut io_slots = Slots::new();
        io_slots.set_limit(cfg.io_slots());
        Ok(Self {
            cap,
            version,
            cfg,
            state: State::Idle,
            admin_sq: SubmissionRing::new(cfg.admin.sq, cfg.admin.entries),
            admin_cq: CompletionRing::new(cfg.admin.cq, cfg.admin.entries),
            admin_slots,
            io_sq: SubmissionRing::new(cfg.io.sq, cfg.io.entries),
            io_cq: CompletionRing::new(cfg.io.cq, cfg.io.entries),
            io_slots,
            ctrl: None,
            ns: None,
            abort_limit: 1,
            aborts: 0,
            max_transfer: 0,
        })
    }

    /// Validated CAP.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        self.cap
    }

    /// VS.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Identify Controller data (after the first identify step).
    #[must_use]
    pub fn controller_info(&self) -> Option<&ControllerInfo> {
        self.ctrl.as_ref()
    }

    /// Identify Namespace data (after the second identify step).
    #[must_use]
    pub fn namespace(&self) -> Option<&NamespaceInfo> {
        self.ns.as_ref()
    }

    /// Largest transfer of one Read/Write in bytes: the minimum of MDTS,
    /// the PRP list capacity (for any first-page offset) and 65536 blocks,
    /// rounded down to whole blocks. 0 before Identify.
    #[must_use]
    pub fn max_transfer_bytes(&self) -> u32 {
        self.max_transfer
    }

    /// I/O commands that can be outstanding at once.
    #[must_use]
    pub fn io_queue_depth(&self) -> usize {
        self.cfg.io_slots()
    }

    /// Caller commands submitted and not yet reported.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.io_slots.busy()
    }

    /// Current phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        match self.state {
            State::Idle => Phase::Idle,
            State::Start | State::WaitReadyMatch { .. } | State::WaitDisabled { .. } => {
                Phase::Disabling
            }
            State::WaitEnabled { .. } => Phase::Enabling,
            State::Admin { step, .. } => Phase::Admin(step),
            State::Ready => Phase::Ready,
            State::NeedsReset(r) => Phase::NeedsReset(r),
            State::ShutDown => Phase::ShutDown,
            State::Failed => Phase::Failed,
        }
    }

    fn csts<H: Hardware + ?Sized>(hw: &mut H) -> Result<ControllerStatus, Error> {
        let st = ControllerStatus(hw.read32(regs::CSTS));
        if st.is_all_ones() {
            Err(Error::DeviceGone)
        } else {
            Ok(st)
        }
    }

    fn sq_doorbell(&self, qid: u16) -> u32 {
        // In range: checked against the BAR size in `check_config`.
        self.cap.sq_tail_doorbell(qid) as u32
    }

    fn cq_doorbell(&self, qid: u16) -> u32 {
        self.cap.cq_head_doorbell(qid) as u32
    }

    fn list_region(&self, idx: usize) -> PrpList {
        let pages = self.cfg.prp_pages_per_command;
        PrpList {
            base: self.cfg.prp_pool + idx as u64 * u64::from(pages) * PAGE_SIZE,
            pages,
        }
    }

    fn needs_reset(&mut self, reason: ResetReason) -> Error {
        self.state = State::NeedsReset(reason);
        Error::NeedsReset(reason)
    }

    fn ensure_ready(&self) -> Result<(), Error> {
        match self.state {
            State::Ready => Ok(()),
            State::NeedsReset(r) => Err(Error::NeedsReset(r)),
            _ => Err(Error::NotReady),
        }
    }

    fn cancel_io<F: FnMut(Completion)>(&mut self, sink: &mut F, outcome: Outcome) {
        let mut i = 0;
        while let Some(idx) = self.io_slots.next_busy(i) {
            i = idx + 1;
            let kind = self.io_slots.get(idx).kind;
            self.io_slots.release(idx);
            if let Kind::Io { tag } = kind {
                sink(Completion { tag, outcome });
            }
        }
    }

    // ---- initialisation ------------------------------------------------

    /// First initialisation. Fails with [`Error::Busy`] if commands are
    /// outstanding (use [`Controller::reset`] then).
    pub fn init<H: Hardware + ?Sized>(&mut self, hw: &mut H) -> Result<(), Error> {
        if self.outstanding() > 0 {
            return Err(Error::Busy);
        }
        self.reset(hw, &mut |_| {})
    }

    /// Controller reset and re-initialisation, blocking until `Ready` or an
    /// error. Outstanding commands are reported to `sink` once the
    /// controller is disabled.
    pub fn reset<H, F>(&mut self, hw: &mut H, sink: &mut F) -> Result<(), Error>
    where
        H: Hardware + ?Sized,
        F: FnMut(Completion),
    {
        self.start_reset();
        loop {
            if self.step(hw, sink)? == Progress::Ready {
                return Ok(());
            }
        }
    }

    /// Starts (re-)initialisation; drive it with [`Controller::step`].
    pub fn start_reset(&mut self) {
        self.state = State::Start;
    }

    /// Advances initialisation by one observation. Errors leave the
    /// controller in [`Phase::Failed`]; [`Controller::reset`] may retry.
    pub fn step<H, F>(&mut self, hw: &mut H, sink: &mut F) -> Result<Progress, Error>
    where
        H: Hardware + ?Sized,
        F: FnMut(Completion),
    {
        match self.state {
            State::Ready => return Ok(Progress::Ready),
            State::Start
            | State::WaitReadyMatch { .. }
            | State::WaitDisabled { .. }
            | State::WaitEnabled { .. }
            | State::Admin { .. } => {}
            _ => return Err(Error::NotReady),
        }
        self.init_step(hw, sink)
            .inspect_err(|_| self.state = State::Failed)
    }

    fn init_step<H, F>(&mut self, hw: &mut H, sink: &mut F) -> Result<Progress, Error>
    where
        H: Hardware + ?Sized,
        F: FnMut(Completion),
    {
        let now = hw.now_ns();
        let ready_deadline = now.saturating_add(self.cap.ready_timeout_ns());
        match self.state {
            State::Start => {
                let st = Self::csts(hw)?;
                let cc = hw.read32(regs::CC);
                if cc == u32::MAX {
                    return Err(Error::DeviceGone);
                }
                if cc & cc::EN != 0 && !st.ready() && !st.fatal() {
                    // Changing EN before RDY matches it is undefined.
                    self.state = State::WaitReadyMatch {
                        deadline: ready_deadline,
                    };
                } else if cc & cc::EN != 0 {
                    hw.write32(regs::CC, cc & !cc::EN);
                    self.state = State::WaitDisabled {
                        deadline: ready_deadline,
                    };
                } else if st.ready() {
                    self.state = State::WaitDisabled {
                        deadline: ready_deadline,
                    };
                } else {
                    self.disabled(hw, sink, ready_deadline);
                }
            }
            State::WaitReadyMatch { deadline } => {
                let st = Self::csts(hw)?;
                if st.ready() || st.fatal() || now > deadline {
                    let cc = hw.read32(regs::CC);
                    if cc == u32::MAX {
                        return Err(Error::DeviceGone);
                    }
                    hw.write32(regs::CC, cc & !cc::EN);
                    self.state = State::WaitDisabled {
                        deadline: ready_deadline,
                    };
                }
            }
            State::WaitDisabled { deadline } => {
                let st = Self::csts(hw)?;
                if !st.ready() {
                    self.disabled(hw, sink, ready_deadline);
                } else if now > deadline {
                    return Err(Error::Timeout(TimeoutPhase::Disable));
                }
            }
            State::WaitEnabled { deadline } => {
                let st = Self::csts(hw)?;
                if st.fatal() {
                    return Err(Error::ControllerFatal);
                }
                if st.ready() {
                    self.state = State::Admin {
                        step: AdminStep::IdentifyController,
                        cid: None,
                        deadline: 0,
                    };
                } else if now > deadline {
                    return Err(Error::Timeout(TimeoutPhase::Enable));
                }
            }
            State::Admin {
                step,
                cid,
                deadline,
            } => return self.admin_step(hw, now, step, cid, deadline),
            _ => return Err(Error::NotReady),
        }
        Ok(Progress::Pending)
    }

    /// CSTS.RDY = 0: the controller processes no command and performs no
    /// DMA until enabled again, so outstanding commands are reported now.
    fn disabled<H, F>(&mut self, hw: &mut H, sink: &mut F, deadline: u64)
    where
        H: Hardware + ?Sized,
        F: FnMut(Completion),
    {
        self.cancel_io(sink, Outcome::Reset);
        self.admin_slots.release_all();
        self.aborts = 0;
        self.admin_sq.reset();
        self.admin_cq.reset(hw);
        self.io_sq.reset();
        let n = self.cfg.admin.entries - 1;
        hw.write32(regs::AQA, n | n << 16);
        hw.write64(regs::ASQ, self.cfg.admin.sq);
        hw.write64(regs::ACQ, self.cfg.admin.cq);
        hw.write32(regs::CC, cc::DRIVER_CONFIG | cc::EN);
        self.state = State::WaitEnabled { deadline };
    }

    fn admin_command<H: Hardware + ?Sized>(&mut self, hw: &mut H, step: AdminStep) -> Command {
        let io = self.cfg.io;
        // `io.entries` is 2..=65536, so the 0's based size fits in u16.
        let qsize0 = (io.entries - 1) as u16;
        match step {
            AdminStep::IdentifyController => {
                Command::identify(cns::CONTROLLER, 0, self.cfg.identify_buffer)
            }
            AdminStep::IdentifyNamespace => {
                Command::identify(cns::NAMESPACE, self.cfg.nsid, self.cfg.identify_buffer)
            }
            AdminStep::SetNumQueues => Command::set_num_queues(0, 0),
            AdminStep::CreateIoCq => {
                self.io_cq.reset(hw);
                Command::create_io_cq(IO_QID, qsize0, io.cq, self.cfg.io_vector)
            }
            AdminStep::CreateIoSq => {
                self.io_sq.reset();
                Command::create_io_sq(IO_QID, qsize0, io.sq, IO_QID)
            }
            AdminStep::DeleteIoSq => Command::delete_io_sq(IO_QID),
            AdminStep::DeleteIoCq => Command::delete_io_cq(IO_QID),
        }
    }

    fn submit_admin<H: Hardware + ?Sized>(
        &mut self,
        hw: &mut H,
        mut cmd: Command,
        kind: Kind,
        deadline: u64,
    ) -> Result<u16, Error> {
        if self.admin_sq.free() == 0 {
            return Err(Error::QueueFull);
        }
        let (idx, cid) = self
            .admin_slots
            .alloc(kind, deadline)
            .ok_or(Error::QueueFull)?;
        cmd.set_cid(cid);
        let Some(tail) = self.admin_sq.push(hw, &cmd) else {
            self.admin_slots.release(idx);
            return Err(Error::QueueFull);
        };
        hw.write32(self.sq_doorbell(0), tail);
        Ok(cid)
    }

    /// Consumes one admin completion, if any.
    fn reap_admin<H: Hardware + ?Sized>(
        &mut self,
        hw: &mut H,
    ) -> Result<Option<(Kind, CompletionEntry)>, Error> {
        let Some(cqe) = self.admin_cq.peek(hw) else {
            return Ok(None);
        };
        self.admin_cq.advance();
        hw.write32(self.cq_doorbell(0), self.admin_cq.head());
        let idx = check_entry(&self.admin_sq, &self.admin_slots, &cqe, 0)?;
        self.admin_sq.set_head(cqe.sq_head);
        let kind = self.admin_slots.get(idx).kind;
        self.admin_slots.release(idx);
        if kind == Kind::Abort {
            self.aborts = self.aborts.saturating_sub(1);
        }
        Ok(Some((kind, cqe)))
    }

    fn admin_step<H: Hardware + ?Sized>(
        &mut self,
        hw: &mut H,
        now: u64,
        step: AdminStep,
        cid: Option<u16>,
        deadline: u64,
    ) -> Result<Progress, Error> {
        if Self::csts(hw)?.fatal() {
            return Err(Error::ControllerFatal);
        }
        let Some(cid) = cid else {
            let cmd = self.admin_command(hw, step);
            let deadline = now.saturating_add(self.cfg.admin_timeout_ns);
            let cid = self.submit_admin(hw, cmd, Kind::Admin, deadline)?;
            self.state = State::Admin {
                step,
                cid: Some(cid),
                deadline,
            };
            return Ok(Progress::Pending);
        };
        match self.reap_admin(hw)? {
            None if now > deadline => Err(Error::Timeout(TimeoutPhase::Admin(step))),
            None => Ok(Progress::Pending),
            Some((_, cqe)) if cqe.cid != cid => Err(Error::InvalidCompletion(
                CompletionError::UnknownCid(cqe.cid),
            )),
            Some((_, cqe)) if !cqe.status.is_success() => Err(Error::AdminCommand {
                step,
                status: cqe.status,
            }),
            Some(_) => self.finish_admin(hw, step),
        }
    }

    fn finish_admin<H: Hardware + ?Sized>(
        &mut self,
        hw: &mut H,
        step: AdminStep,
    ) -> Result<Progress, Error> {
        let next = match step {
            AdminStep::IdentifyController => {
                let mut buf = [0u8; IDENTIFY_PARSE_LEN];
                hw.read(self.cfg.identify_buffer, &mut buf);
                let info = ControllerInfo::parse(&buf).map_err(|e| match e {
                    IdentifyError::EntrySize => Error::Unsupported(Unsupported::EntrySize),
                    e => Error::Identify(e),
                })?;
                if info.nn < self.cfg.nsid {
                    return Err(Error::NamespaceNotFound);
                }
                let admin_cap = (self.cfg.admin.entries as usize - 1).min(ADMIN_SLOTS);
                self.abort_limit = (usize::from(info.acl) + 1).min(admin_cap);
                self.ctrl = Some(info);
                AdminStep::IdentifyNamespace
            }
            AdminStep::IdentifyNamespace => {
                let mut buf = [0u8; IDENTIFY_PARSE_LEN];
                hw.read(self.cfg.identify_buffer, &mut buf);
                let ns = NamespaceInfo::parse(&buf)?;
                if ns.current_format().metadata_size != 0 {
                    return Err(Error::Unsupported(Unsupported::Metadata));
                }
                if let Some(old) = &self.ns {
                    if old.nsze != ns.nsze || old.lba_shift() != ns.lba_shift() {
                        return Err(Error::NamespaceChanged);
                    }
                }
                self.max_transfer = self.compute_max_transfer(ns.lba_shift());
                if self.max_transfer == 0 {
                    return Err(Error::Unsupported(Unsupported::LbaSize));
                }
                self.ns = Some(ns);
                AdminStep::SetNumQueues
            }
            // Counts are 0's based, so at least one pair is always granted.
            AdminStep::SetNumQueues => AdminStep::CreateIoCq,
            AdminStep::CreateIoCq => AdminStep::CreateIoSq,
            AdminStep::CreateIoSq => {
                self.state = State::Ready;
                return Ok(Progress::Ready);
            }
            AdminStep::DeleteIoSq | AdminStep::DeleteIoCq => return Err(Error::NotReady),
        };
        self.state = State::Admin {
            step: next,
            cid: None,
            deadline: 0,
        };
        Ok(Progress::Pending)
    }

    fn compute_max_transfer(&self, lba_shift: u8) -> u32 {
        let pages = prp::max_data_pages(self.cfg.prp_pages_per_command);
        let mut max = (pages - 1) * PAGE_SIZE;
        if let Some(mdts) = self
            .ctrl
            .as_ref()
            .and_then(ControllerInfo::max_transfer_bytes)
        {
            max = max.min(mdts);
        }
        max = max.min(65536u64 << lba_shift).min(TRANSFER_CAP);
        max &= !((1u64 << lba_shift) - 1);
        max as u32
    }

    // ---- I/O -------------------------------------------------------------

    /// Queues one request under the caller's `tag`. On error nothing was
    /// submitted and the tag will not be reported.
    pub fn submit<H: Hardware + ?Sized>(
        &mut self,
        hw: &mut H,
        tag: u64,
        request: Request<'_>,
    ) -> Result<(), Error> {
        self.ensure_ready()?;
        let (nsze, shift) = match &self.ns {
            Some(ns) => (ns.nsze, ns.lba_shift()),
            None => return Err(Error::NotReady),
        };
        let nsid = self.cfg.nsid;
        let (mut cmd, buffer) = match request {
            Request::Flush => (Command::flush(nsid), None),
            Request::Read {
                lba,
                blocks,
                buffer,
            }
            | Request::Write {
                lba,
                blocks,
                buffer,
            } => {
                if blocks == 0 || blocks > 65536 {
                    return Err(Error::LbaRange);
                }
                let end = lba.checked_add(u64::from(blocks)).ok_or(Error::LbaRange)?;
                if end > nsze {
                    return Err(Error::LbaRange);
                }
                let bytes = u64::from(blocks) << shift;
                if bytes > u64::from(self.max_transfer) {
                    return Err(Error::TransferTooLarge);
                }
                if u64::from(buffer.len) != bytes {
                    return Err(Error::Buffer(PrpError::Length));
                }
                prp::validate(&buffer, self.list_region(0))?;
                let nlb0 = (blocks - 1) as u16;
                let cmd = if matches!(request, Request::Read { .. }) {
                    Command::read(nsid, lba, nlb0)
                } else {
                    Command::write(nsid, lba, nlb0)
                };
                (cmd, Some(buffer))
            }
        };
        if self.io_sq.free() == 0 || !self.io_slots.has_free() {
            return Err(Error::QueueFull);
        }
        let deadline = hw.now_ns().saturating_add(self.cfg.io_timeout_ns);
        let (idx, cid) = self
            .io_slots
            .alloc(Kind::Io { tag }, deadline)
            .ok_or(Error::QueueFull)?;
        if let Some(buffer) = buffer {
            match prp::build(hw, &buffer, self.list_region(idx)) {
                Ok(p) => cmd.set_prp(p.prp1, p.prp2),
                Err(e) => {
                    self.io_slots.release(idx);
                    return Err(e.into());
                }
            }
        }
        cmd.set_cid(cid);
        let Some(tail) = self.io_sq.push(hw, &cmd) else {
            self.io_slots.release(idx);
            return Err(Error::QueueFull);
        };
        // The entry is in memory; only now the controller may fetch it.
        hw.write32(self.sq_doorbell(IO_QID), tail);
        Ok(())
    }

    /// Reaps completions, checks controller status and command deadlines.
    /// Returns the number of commands reported to `sink`.
    ///
    /// A malformed completion entry is consumed, dropped and reported as
    /// [`Error::InvalidCompletion`]; call `poll` again. [`Error::NeedsReset`]
    /// means I/O stopped until [`Controller::reset`].
    pub fn poll<H, F>(&mut self, hw: &mut H, sink: &mut F) -> Result<usize, Error>
    where
        H: Hardware + ?Sized,
        F: FnMut(Completion),
    {
        self.ensure_ready()?;
        let st = ControllerStatus(hw.read32(regs::CSTS));
        if st.is_all_ones() {
            return Err(self.needs_reset(ResetReason::DeviceGone));
        }
        if st.fatal() {
            return Err(self.needs_reset(ResetReason::ControllerFatal));
        }
        for _ in 0..self.cfg.admin.entries {
            if self.reap_admin(hw)?.is_none() {
                break;
            }
        }
        let n = self.reap_io(hw, sink)?;
        self.check_timeouts(hw)?;
        Ok(n)
    }

    fn reap_io<H, F>(&mut self, hw: &mut H, sink: &mut F) -> Result<usize, Error>
    where
        H: Hardware + ?Sized,
        F: FnMut(Completion),
    {
        let mut count = 0;
        let mut consumed = false;
        let mut result = Ok(());
        for _ in 0..self.cfg.io.entries {
            let Some(cqe) = self.io_cq.peek(hw) else {
                break;
            };
            self.io_cq.advance();
            consumed = true;
            let idx = match check_entry(&self.io_sq, &self.io_slots, &cqe, IO_QID) {
                Ok(idx) => idx,
                Err(e) => {
                    result = Err(Error::InvalidCompletion(e));
                    break;
                }
            };
            self.io_sq.set_head(cqe.sq_head);
            let slot = *self.io_slots.get(idx);
            self.io_slots.release(idx);
            let Kind::Io { tag } = slot.kind else {
                continue;
            };
            let outcome = if cqe.status.is_success() {
                Outcome::Success { dw0: cqe.dw0 }
            } else if matches!(slot.state, SlotState::Aborting { .. })
                && cqe.status.is_abort_requested()
            {
                Outcome::TimedOut
            } else {
                Outcome::Error(cqe.status)
            };
            sink(Completion { tag, outcome });
            count += 1;
        }
        if consumed {
            hw.write32(self.cq_doorbell(IO_QID), self.io_cq.head());
        }
        result.map(|()| count)
    }

    fn check_timeouts<H: Hardware + ?Sized>(&mut self, hw: &mut H) -> Result<(), Error> {
        let now = hw.now_ns();
        let mut i = 0;
        while let Some(idx) = self.admin_slots.next_busy(i) {
            i = idx + 1;
            if now > self.admin_slots.get(idx).deadline {
                return Err(self.needs_reset(ResetReason::AbortTimeout));
            }
        }
        i = 0;
        while let Some(idx) = self.io_slots.next_busy(i) {
            i = idx + 1;
            let slot = *self.io_slots.get(idx);
            match slot.state {
                SlotState::Active if now > slot.deadline => {
                    let last_chance = slot.deadline.saturating_add(self.cfg.abort_timeout_ns);
                    if !self.try_abort(hw, idx, now) && now > last_chance {
                        return Err(self.needs_reset(ResetReason::CommandTimeout));
                    }
                }
                SlotState::Aborting { until } if now > until => {
                    return Err(self.needs_reset(ResetReason::CommandTimeout));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn try_abort<H: Hardware + ?Sized>(&mut self, hw: &mut H, idx: usize, now: u64) -> bool {
        if self.aborts >= self.abort_limit {
            return false;
        }
        let cid = self.io_slots.cid(idx);
        let deadline = now.saturating_add(self.cfg.admin_timeout_ns);
        if self
            .submit_admin(hw, Command::abort(IO_QID, cid), Kind::Abort, deadline)
            .is_err()
        {
            return false;
        }
        self.aborts += 1;
        self.io_slots.get_mut(idx).state = SlotState::Aborting {
            until: now.saturating_add(self.cfg.abort_timeout_ns),
        };
        true
    }

    /// Reports every outstanding command as [`Outcome::Abandoned`] and
    /// forgets the queues. Only call after the device can no longer
    /// perform DMA (Bus Master Enable cleared, function reset); the
    /// controller must be reset before further use.
    pub fn abandon<F: FnMut(Completion)>(&mut self, sink: &mut F) {
        self.cancel_io(sink, Outcome::Abandoned);
        self.admin_slots.release_all();
        self.aborts = 0;
        self.state = State::Failed;
    }

    // ---- shutdown --------------------------------------------------------

    fn run_admin<H: Hardware + ?Sized>(
        &mut self,
        hw: &mut H,
        step: AdminStep,
    ) -> Result<CompletionEntry, Error> {
        let deadline = hw.now_ns().saturating_add(self.cfg.admin_timeout_ns);
        let cmd = self.admin_command(hw, step);
        let cid = self.submit_admin(hw, cmd, Kind::Admin, deadline)?;
        loop {
            if Self::csts(hw)?.fatal() {
                return Err(Error::ControllerFatal);
            }
            if let Some((_, cqe)) = self.reap_admin(hw)? {
                if cqe.cid == cid {
                    return if cqe.status.is_success() {
                        Ok(cqe)
                    } else {
                        Err(Error::AdminCommand {
                            step,
                            status: cqe.status,
                        })
                    };
                }
            }
            if hw.now_ns() > deadline {
                return Err(Error::Timeout(TimeoutPhase::Admin(step)));
            }
        }
    }

    /// Normal shutdown: requires `Ready` with nothing outstanding. The I/O
    /// queues are deleted first (failures there do not stop the shutdown),
    /// then CC.SHN = 01b and a bounded wait for CSTS.SHST = 10b. The bound
    /// is the larger of `shutdown_timeout_ns` and Identify RTD3E.
    pub fn shutdown<H: Hardware + ?Sized>(&mut self, hw: &mut H) -> Result<(), Error> {
        if self.state != State::Ready {
            return Err(Error::NotReady);
        }
        if self.io_slots.busy() > 0 || self.admin_slots.busy() > 0 {
            return Err(Error::Busy);
        }
        if self.run_admin(hw, AdminStep::DeleteIoSq).is_ok() {
            let _ = self.run_admin(hw, AdminStep::DeleteIoCq);
        }
        self.io_sq.reset();
        let rtd3e_ns = self
            .ctrl
            .as_ref()
            .map_or(0, |c| u64::from(c.rtd3e_us) * 1000);
        let deadline = hw
            .now_ns()
            .saturating_add(self.cfg.shutdown_timeout_ns.max(rtd3e_ns));
        self.state = State::Failed;
        let cc = hw.read32(regs::CC);
        if cc == u32::MAX {
            return Err(Error::DeviceGone);
        }
        hw.write32(regs::CC, (cc & !cc::SHN_MASK) | cc::SHN_NORMAL);
        loop {
            let now = hw.now_ns();
            let st = Self::csts(hw)?;
            if st.shutdown() == ShutdownStatus::Complete {
                self.state = State::ShutDown;
                return Ok(());
            }
            if now > deadline {
                return Err(Error::Timeout(TimeoutPhase::Shutdown));
            }
        }
    }
}
