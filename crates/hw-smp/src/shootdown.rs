//! TLB shootdown protocol.
//!
//! # Protocol
//!
//! One shootdown runs at a time per [`ShootdownDomain`] (the *slot*).
//! The initiator, after it has already changed the page tables:
//!
//! 1. marks itself as initiating (per-CPU flag), then acquires the slot;
//!    while spinning for it, it keeps [servicing] its own pending requests, so
//!    two CPUs that want to initiate at the same time cannot deadlock waiting
//!    for each other's acknowledgement. The flag is set *before* the slot
//!    attempt, so a nested call on the same CPU (interrupt or NMI at any point
//!    from there until the shootdown ends) gets [`ShootdownError::Reentrant`]
//!    at once instead of spinning for its own slot;
//! 2. assigns the next generation `g` and publishes the request (range or
//!    full flush) under a seqlock;
//! 3. computes the wait set: requested targets, minus itself, minus offline
//!    CPUs (reported as skipped); stores `pending[t] = g` and sends an IPI to
//!    each member; flushes locally if it targeted itself;
//! 4. waits until every member has `acked[t] == g` (an acknowledgement of an
//!    older generation is ignored), dropping members that go offline, until
//!    the timeout. Success yields a [`ShootdownComplete`]; timeout yields
//!    [`ShootdownError::Timeout`] with the unacknowledged CPUs.
//!
//! A target (IPI handler or any polling loop) calls
//! [`ShootdownDomain::service`]: it reads `pending`, reads the request
//! through the seqlock, flushes, and publishes `acked = pending`. If the
//! request it reads is torn or already replaced by a newer one it flushes the
//! whole TLB instead: flushing more than asked is always safe.
//!
//! Frames: a frame unmapped from a virtual address may be reused only after a
//! shootdown covering that address has been acknowledged by every target.
//! [`RetiredFrame`] holds such a frame and gives it back only for a matching
//! [`ShootdownComplete`] token started after the frame was retired.
//!
//! # Offline contract
//!
//! A CPU that is offline does not use its TLB. [`ShootdownDomain::mark_online`]
//! publishes the online flag, issues a `SeqCst` fence and then flushes the
//! whole local TLB, before the CPU uses any translation. The initiator issues
//! a `SeqCst` fence after its page-table update and before reading online
//! flags. If the initiator reads "offline" for a CPU, that CPU's next online
//! fence is ordered after the initiator's fence, so its post-online flush and
//! page walks observe the updated page tables. Such CPUs are therefore safe to
//! skip, both at start and while waiting.
//!
//! # Memory ordering
//!
//! - Slot: `busy` CAS `Acquire` / store `Release` serialises initiators and
//!   hands over `last_gen`.
//! - Re-entrancy flag `initiating[me]`: read and written only by CPU `me`
//!   (including its interrupt handlers). `swap(true, Acquire)` keeps the slot
//!   CAS from being hoisted above it; `store(false, Release)` keeps the slot
//!   release from sinking below it. It is cleared on every exit: slot timeout,
//!   error while waiting for the slot, and [`InFlight`] drop.
//! - Request: seqlock (`seq` odd → `Release` fence → relaxed fields → `seq`
//!   even with `Release`; reader: `Acquire` load, relaxed fields, `Acquire`
//!   fence, re-load).
//! - `pending[t].store(g, Release)` after the request and after the caller's
//!   page-table writes; the target's `pending.load(Acquire)` therefore sees
//!   both, and its flush happens after the page-table change is visible.
//! - `acked[t].fetch_max(g, Release)` after the flush; the initiator's
//!   `acked.load(Acquire)` makes the flush happen-before the frame release.
//!
//! [servicing]: ShootdownDomain::service

#![forbid(unsafe_code)]

use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use crate::mask::CpuMask;
use crate::{MAX_CPUS, PAGE_SIZE};

/// Encoding of [`FlushRequest::All`] in `req_pages`; never a valid range length.
const ALL_PAGES: u64 = u64::MAX;

/// What to invalidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushRequest {
    /// `pages` pages starting at page-aligned `start`.
    Range {
        /// First virtual address (page aligned).
        start: u64,
        /// Number of pages (non-zero).
        pages: u64,
    },
    /// The whole (non-global) TLB.
    All,
}

impl FlushRequest {
    /// A validated range request.
    pub fn range(start: u64, pages: u64) -> Result<Self, ShootdownError> {
        if !start.is_multiple_of(PAGE_SIZE) || pages == 0 || pages == ALL_PAGES {
            return Err(ShootdownError::InvalidRange);
        }
        pages
            .checked_mul(PAGE_SIZE)
            .and_then(|len| start.checked_add(len))
            .ok_or(ShootdownError::InvalidRange)?;
        Ok(Self::Range { start, pages })
    }

    /// Whether the request invalidates translations of `va`.
    pub fn covers(&self, va: u64) -> bool {
        match *self {
            Self::All => true,
            Self::Range { start, pages } => va >= start && (va - start) / PAGE_SIZE < pages,
        }
    }

    fn encode(self) -> (u64, u64) {
        match self {
            Self::Range { start, pages } => (start, pages),
            Self::All => (0, ALL_PAGES),
        }
    }
}

/// Local TLB invalidation of the executing CPU (kernel: `invlpg`/CR3 reload).
pub trait LocalTlb {
    /// Invalidates `pages` pages starting at `start`.
    fn flush_range(&mut self, start: u64, pages: u64);
    /// Invalidates the whole TLB.
    fn flush_all(&mut self);
}

/// Initiator-side machine effects.
pub trait ShootdownOps {
    /// Sends the shootdown IPI to logical CPU `cpu`.
    fn send_ipi(&mut self, cpu: usize);
    /// Monotonic tick counter; the unit of the domain timeout.
    fn now_ticks(&mut self) -> u64;
}

/// Errors of the shootdown protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShootdownError {
    /// CPU index not below the domain size.
    InvalidCpu(usize),
    /// Misaligned, empty or overflowing range.
    InvalidRange,
    /// The executing CPU already runs a shootdown in this domain.
    Reentrant,
    /// Another shootdown held the slot for longer than the timeout.
    SlotTimeout,
    /// Some targets did not acknowledge in time. The frames covered by this
    /// request must not be reused.
    Timeout {
        /// Generation that timed out.
        generation: u64,
        /// Online targets that did not acknowledge.
        unacked: CpuMask,
    },
}

struct CpuSlot {
    pending: AtomicU64,
    acked: AtomicU64,
    online: AtomicBool,
    initiating: AtomicBool,
}

impl CpuSlot {
    const fn new() -> Self {
        Self {
            pending: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            online: AtomicBool::new(false),
            initiating: AtomicBool::new(false),
        }
    }
}

/// Shared shootdown state for up to `N` CPUs (`N <= MAX_CPUS`). All CPUs
/// start offline; each calls [`mark_online`](Self::mark_online) before using
/// its TLB.
pub struct ShootdownDomain<const N: usize> {
    busy: AtomicBool,
    last_gen: AtomicU64,
    seq: AtomicU64,
    req_gen: AtomicU64,
    req_start: AtomicU64,
    req_pages: AtomicU64,
    cpus: [CpuSlot; N],
    timeout: u64,
}

impl<const N: usize> ShootdownDomain<N> {
    const SIZE_OK: () = assert!(N > 0 && N <= MAX_CPUS, "domain size must be 1..=MAX_CPUS");

    /// A domain whose waits (slot and acknowledgements) give up after
    /// `timeout_ticks` of [`ShootdownOps::now_ticks`].
    pub const fn new(timeout_ticks: u64) -> Self {
        #[allow(clippy::let_unit_value)]
        let () = Self::SIZE_OK;
        Self {
            busy: AtomicBool::new(false),
            last_gen: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            req_gen: AtomicU64::new(0),
            req_start: AtomicU64::new(0),
            req_pages: AtomicU64::new(0),
            cpus: [const { CpuSlot::new() }; N],
            timeout: timeout_ticks,
        }
    }

    fn slot(&self, cpu: usize) -> Result<&CpuSlot, ShootdownError> {
        self.cpus.get(cpu).ok_or(ShootdownError::InvalidCpu(cpu))
    }

    /// Last assigned generation (0 before the first shootdown).
    pub fn generation(&self) -> u64 {
        self.last_gen.load(Ordering::Acquire)
    }

    /// Whether `cpu` is currently online.
    pub fn is_online(&self, cpu: usize) -> bool {
        self.cpus
            .get(cpu)
            .is_some_and(|s| s.online.load(Ordering::SeqCst))
    }

    /// Called by CPU `me` before it starts using translations: publishes the
    /// online flag, then flushes the whole local TLB (see the offline contract).
    pub fn mark_online<T: LocalTlb>(&self, me: usize, tlb: &mut T) -> Result<(), ShootdownError> {
        let slot = self.slot(me)?;
        slot.online.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        tlb.flush_all();
        Ok(())
    }

    /// Called by CPU `me` when it stops using translations. Requests that were
    /// already published to it are serviced before returning.
    pub fn mark_offline<T: LocalTlb>(&self, me: usize, tlb: &mut T) -> Result<(), ShootdownError> {
        let slot = self.slot(me)?;
        slot.online.store(false, Ordering::SeqCst);
        self.service(me, tlb)?;
        Ok(())
    }

    /// Target side: performs every request pending for CPU `me` and
    /// acknowledges it. Call from the shootdown IPI handler (IRQs disabled) and
    /// from spin loops. Returns the number of generations acknowledged.
    pub fn service<T: LocalTlb>(&self, me: usize, tlb: &mut T) -> Result<usize, ShootdownError> {
        let slot = self.slot(me)?;
        let mut handled = 0;
        loop {
            let pending = slot.pending.load(Ordering::Acquire);
            if pending <= slot.acked.load(Ordering::Relaxed) {
                return Ok(handled);
            }
            let s1 = self.seq.load(Ordering::Acquire);
            let gen = self.req_gen.load(Ordering::Relaxed);
            let start = self.req_start.load(Ordering::Relaxed);
            let pages = self.req_pages.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 && s1.is_multiple_of(2) && gen == pending && pages != ALL_PAGES {
                tlb.flush_range(start, pages);
            } else {
                // Full request, torn read, or a newer request replaced ours.
                tlb.flush_all();
            }
            slot.acked.fetch_max(pending, Ordering::Release);
            handled += 1;
        }
    }

    /// Initiator side, phase 1: acquires the slot, publishes `request` to the
    /// online CPUs of `targets` (other than `me`) and flushes locally if `me`
    /// is a target. The caller must already have updated the page tables.
    ///
    /// Dropping the returned [`InFlight`] without [`InFlight::wait`] abandons
    /// the shootdown: the slot is released, late acknowledgements are ignored
    /// and no [`ShootdownComplete`] is ever produced for it.
    pub fn start<O: ShootdownOps, T: LocalTlb>(
        &self,
        me: usize,
        request: FlushRequest,
        targets: CpuMask,
        ops: &mut O,
        tlb: &mut T,
    ) -> Result<InFlight<'_, N>, ShootdownError> {
        self.slot(me)?;
        if let Some(high) = targets.highest() {
            self.slot(high)?;
        }
        if let FlushRequest::Range { start, pages } = request {
            FlushRequest::range(start, pages)?;
        }

        let initiating = &self.cpus[me].initiating;
        if initiating.swap(true, Ordering::Acquire) {
            // The outer call on this CPU owns the flag; leave it set.
            return Err(ShootdownError::Reentrant);
        }
        let t0 = ops.now_ticks();
        while self
            .busy
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            let step = self.service(me, tlb).and_then(|_| {
                if ops.now_ticks().wrapping_sub(t0) > self.timeout {
                    Err(ShootdownError::SlotTimeout)
                } else {
                    Ok(())
                }
            });
            if let Err(e) = step {
                initiating.store(false, Ordering::Release);
                return Err(e);
            }
            core::hint::spin_loop();
        }
        // From here on the slot and the flag are released by `InFlight::drop`.
        let gen = self.last_gen.load(Ordering::Relaxed) + 1;
        self.last_gen.store(gen, Ordering::Release);

        let (start, pages) = request.encode();
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.req_gen.store(gen, Ordering::Relaxed);
        self.req_start.store(start, Ordering::Relaxed);
        self.req_pages.store(pages, Ordering::Relaxed);
        self.seq.store(s.wrapping_add(2), Ordering::Release);

        // Orders the caller's page-table writes before the online reads
        // (offline contract).
        fence(Ordering::SeqCst);
        let mut waiting = CpuMask::empty();
        let mut skipped_offline = CpuMask::empty();
        for cpu in targets.iter().filter(|&c| c != me) {
            // Both inserts are in range: `cpu < N <= MAX_CPUS`.
            if self.cpus[cpu].online.load(Ordering::SeqCst) {
                let _ = waiting.insert(cpu);
            } else {
                let _ = skipped_offline.insert(cpu);
            }
        }
        for cpu in waiting.iter() {
            self.cpus[cpu].pending.store(gen, Ordering::Release);
            ops.send_ipi(cpu);
        }
        if targets.contains(me) {
            match request {
                FlushRequest::Range { start, pages } => tlb.flush_range(start, pages),
                FlushRequest::All => tlb.flush_all(),
            }
        }
        Ok(InFlight {
            domain: self,
            gen,
            me,
            request,
            waiting,
            acked: CpuMask::empty(),
            skipped_offline,
            started_at: ops.now_ticks(),
        })
    }

    /// [`start`](Self::start) followed by [`InFlight::wait`].
    pub fn shootdown<O: ShootdownOps, T: LocalTlb>(
        &self,
        me: usize,
        request: FlushRequest,
        targets: CpuMask,
        ops: &mut O,
        tlb: &mut T,
    ) -> Result<ShootdownComplete, ShootdownError> {
        self.start(me, request, targets, ops, tlb)?.wait(ops, tlb)
    }

    fn domain_id(&self) -> usize {
        self as *const Self as usize
    }
}

/// A published shootdown whose acknowledgements have not been collected yet.
/// Holds the domain slot until dropped.
#[must_use = "dropping abandons the shootdown without a completion token"]
pub struct InFlight<'d, const N: usize> {
    domain: &'d ShootdownDomain<N>,
    gen: u64,
    me: usize,
    request: FlushRequest,
    waiting: CpuMask,
    acked: CpuMask,
    skipped_offline: CpuMask,
    started_at: u64,
}

impl<const N: usize> InFlight<'_, N> {
    /// Generation of this shootdown.
    pub fn generation(&self) -> u64 {
        self.gen
    }

    /// CPUs an acknowledgement is still expected from.
    pub fn waiting(&self) -> CpuMask {
        self.waiting
    }

    /// Phase 2: waits for every target to acknowledge this generation.
    pub fn wait<O: ShootdownOps, T: LocalTlb>(
        mut self,
        ops: &mut O,
        tlb: &mut T,
    ) -> Result<ShootdownComplete, ShootdownError> {
        let domain = self.domain;
        loop {
            for cpu in self.waiting.iter() {
                let slot = &domain.cpus[cpu];
                if slot.acked.load(Ordering::Acquire) == self.gen {
                    self.waiting.remove(cpu);
                    let _ = self.acked.insert(cpu);
                } else if !slot.online.load(Ordering::SeqCst) {
                    self.waiting.remove(cpu);
                    let _ = self.skipped_offline.insert(cpu);
                }
            }
            if self.waiting.is_empty() {
                return Ok(ShootdownComplete {
                    domain: domain.domain_id(),
                    gen: self.gen,
                    initiator: self.me,
                    request: self.request,
                    acked: self.acked,
                    skipped_offline: self.skipped_offline,
                });
            }
            if ops.now_ticks().wrapping_sub(self.started_at) > domain.timeout {
                return Err(ShootdownError::Timeout {
                    generation: self.gen,
                    unacked: self.waiting,
                });
            }
            // Cannot be targeted while holding the slot, but a stale request
            // may still be pending; servicing it is cheap and harmless.
            domain.service(self.me, tlb)?;
            core::hint::spin_loop();
        }
    }
}

impl<const N: usize> Drop for InFlight<'_, N> {
    fn drop(&mut self) {
        self.domain.busy.store(false, Ordering::Release);
        self.domain.cpus[self.me]
            .initiating
            .store(false, Ordering::Release);
    }
}

/// Proof that every online target acknowledged one shootdown generation.
/// Neither `Clone` nor constructible outside this module.
#[derive(Debug, PartialEq, Eq)]
pub struct ShootdownComplete {
    domain: usize,
    gen: u64,
    initiator: usize,
    request: FlushRequest,
    acked: CpuMask,
    skipped_offline: CpuMask,
}

impl ShootdownComplete {
    /// Generation that completed.
    pub fn generation(&self) -> u64 {
        self.gen
    }
    /// Initiating CPU.
    pub fn initiator(&self) -> usize {
        self.initiator
    }
    /// What was invalidated.
    pub fn request(&self) -> FlushRequest {
        self.request
    }
    /// CPUs that acknowledged.
    pub fn acked(&self) -> CpuMask {
        self.acked
    }
    /// Targets skipped because they were (or went) offline.
    pub fn skipped_offline(&self) -> CpuMask {
        self.skipped_offline
    }
}

/// A physical frame whose last mapping at `va` has been removed but whose
/// stale translations may still be cached. The frame can be taken back only
/// with a [`ShootdownComplete`] that
/// - belongs to the same domain,
/// - was initiated by the retiring CPU with a generation newer than the one
///   current at retirement (so the shootdown started after the unmap, in that
///   CPU's program order), and
/// - covers `va`.
#[derive(Debug)]
pub struct RetiredFrame<F> {
    frame: F,
    va: u64,
    cpu: usize,
    domain: usize,
    after_gen: u64,
}

impl<F> RetiredFrame<F> {
    /// Records `frame`, previously mapped at `va`; call on CPU `me` after the
    /// page-table entry has been cleared or replaced.
    pub fn retire<const N: usize>(
        domain: &ShootdownDomain<N>,
        me: usize,
        va: u64,
        frame: F,
    ) -> Self {
        Self {
            frame,
            va,
            cpu: me,
            domain: domain.domain_id(),
            after_gen: domain.generation(),
        }
    }

    /// The address whose translations must be gone.
    pub fn va(&self) -> u64 {
        self.va
    }

    /// Returns the frame if `done` proves its stale translations are gone;
    /// otherwise gives `self` back unchanged.
    pub fn release(self, done: &ShootdownComplete) -> Result<F, Self> {
        if done.domain == self.domain
            && done.initiator == self.cpu
            && done.gen > self.after_gen
            && done.request.covers(self.va)
        {
            Ok(self.frame)
        } else {
            Err(self)
        }
    }
}
