//! IRQ-aware FIFO ticket spinlock.
//!
//! Acquisition order:
//! 1. save the IRQ state and disable IRQs;
//! 2. read and validate the executing logical CPU id;
//! 3. mark this CPU active for this lock; re-entry from an NMI is refused even
//!    while the interrupted context is still waiting for its ticket;
//! 4. take a ticket and spin until it is served (FIFO fairness).
//!
//! Release publishes the next ticket, clears the diagnostic owner only if it
//! still names this CPU, clears this CPU's active bit, and only then restores
//! the saved IRQ state. The active bit spans ticket wait, ownership, and
//! handoff, so an NMI cannot queue behind a context it has interrupted.
//! Guards must be dropped in LIFO order when several locks are held (the
//! project rule of M1 is to avoid nesting altogether).
//!
//! The recursion check is always on, not only in debug builds. A bounded
//! per-lock CPU bitmap detects re-entry while a CPU holds the lock or waits for
//! its ticket; an NMI must never wait behind the context it interrupted.
//!
//! Memory ordering:
//! - next.fetch_add(Relaxed) publishes no data.
//! - serving.load(Acquire) pairs with serving.store(Release), so writes in
//!   the previous critical section are visible to the next holder.
//! - active_cpus tracks every CPU that entered this lock, including waiters.
//!   A bit is claimed before taking a ticket and released after serving moves,
//!   closing both the NMI re-entry window and handoff race.
//! - owner is diagnostic only. Its compare-exchange during release prevents
//!   an old holder from erasing a new owner's snapshot after ticket handoff.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::MAX_CPUS;

/// Interrupt masking and CPU identity of the executing CPU, implemented by the
/// caller (kernel: `pushfq; cli` / `popfq` and a per-CPU id; tests: a model).
pub trait IrqControl {
    /// Saved interrupt state.
    type State;
    /// Saves the current IRQ state and disables IRQs.
    fn save_and_disable(&self) -> Self::State;
    /// Restores a state returned by [`save_and_disable`](Self::save_and_disable).
    fn restore(&self, state: Self::State);
    /// Dense logical id of the executing CPU. Called only with IRQs disabled,
    /// so the caller cannot migrate. Must return an id below MAX_CPUS.
    fn current_cpu(&self) -> u32;
}

const NO_OWNER: u32 = u32::MAX;
const ACTIVE_WORD_BITS: usize = u64::BITS as usize;
const ACTIVE_WORDS: usize = MAX_CPUS.div_ceil(ACTIVE_WORD_BITS);

/// Why [`TicketLock::lock`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockError {
    /// The executing CPU already holds this lock.
    Recursive {
        /// The CPU id.
        cpu: u32,
    },
    /// The current CPU id is outside 0..MAX_CPUS.
    InvalidCpuId,
}

/// Why [`TicketLock::try_lock`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryLockError {
    /// Another CPU holds or waits for the lock.
    WouldBlock,
    /// The executing CPU already holds this lock.
    Recursive {
        /// The CPU id.
        cpu: u32,
    },
    /// The current CPU id is outside 0..MAX_CPUS.
    InvalidCpuId,
}

/// FIFO ticket spinlock protecting a `T`.
pub struct TicketLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
    owner: AtomicU32,
    active_cpus: [AtomicU64; ACTIVE_WORDS],
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands out access to `data` to one guard at a time: a guard
// exists only for the ticket equal to `serving`, and `serving` advances only
// when that guard is dropped (Release) and is observed by the next holder
// (Acquire). Hence `&TicketLock<T>` shared between threads never yields
// aliased `&mut T`, and moving the `T` access between threads requires only
// `T: Send` (as for `std::sync::Mutex`).
unsafe impl<T: Send> Sync for TicketLock<T> {}

impl<T> TicketLock<T> {
    /// A new unlocked lock.
    pub const fn new(value: T) -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            owner: AtomicU32::new(NO_OWNER),
            active_cpus: [const { AtomicU64::new(0) }; ACTIVE_WORDS],
            data: UnsafeCell::new(value),
        }
    }

    /// Acquires the lock with IRQs disabled, spinning in FIFO order.
    pub fn lock<'a, I: IrqControl>(
        &'a self,
        irq: &'a I,
    ) -> Result<TicketGuard<'a, T, I>, LockError> {
        let state = irq.save_and_disable();
        let cpu = irq.current_cpu();
        if cpu as usize >= MAX_CPUS {
            irq.restore(state);
            return Err(LockError::InvalidCpuId);
        }
        if !self.claim_cpu(cpu) {
            irq.restore(state);
            return Err(LockError::Recursive { cpu });
        }
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
        Ok(self.granted(irq, state, cpu, ticket))
    }

    /// Acquires the lock only if nobody holds or waits for it.
    pub fn try_lock<'a, I: IrqControl>(
        &'a self,
        irq: &'a I,
    ) -> Result<TicketGuard<'a, T, I>, TryLockError> {
        let state = irq.save_and_disable();
        let cpu = irq.current_cpu();
        if cpu as usize >= MAX_CPUS {
            irq.restore(state);
            return Err(TryLockError::InvalidCpuId);
        }
        if !self.claim_cpu(cpu) {
            irq.restore(state);
            return Err(TryLockError::Recursive { cpu });
        }
        let ticket = self.serving.load(Ordering::Acquire);
        if self
            .next
            .compare_exchange(
                ticket,
                ticket.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_err()
        {
            self.release_cpu(cpu);
            irq.restore(state);
            return Err(TryLockError::WouldBlock);
        }
        Ok(self.granted(irq, state, cpu, ticket))
    }

    fn granted<'a, I: IrqControl>(
        &'a self,
        irq: &'a I,
        state: I::State,
        cpu: u32,
        ticket: u32,
    ) -> TicketGuard<'a, T, I> {
        self.owner.store(cpu, Ordering::Relaxed);
        TicketGuard {
            lock: self,
            irq,
            state: Some(state),
            ticket,
            cpu,
            _not_send: PhantomData,
        }
    }

    fn cpu_bit(&self, cpu: u32) -> (&AtomicU64, u64) {
        let index = cpu as usize;
        let mask = 1u64 << (cpu % ACTIVE_WORD_BITS as u32);
        (&self.active_cpus[index / ACTIVE_WORD_BITS], mask)
    }

    fn claim_cpu(&self, cpu: u32) -> bool {
        let (word, mask) = self.cpu_bit(cpu);
        word.fetch_or(mask, Ordering::AcqRel) & mask == 0
    }

    fn release_cpu(&self, cpu: u32) {
        let (word, mask) = self.cpu_bit(cpu);
        word.fetch_and(!mask, Ordering::AcqRel);
    }

    #[inline]
    fn release_ticket_with<F: FnOnce()>(&self, cpu: u32, ticket: u32, after_serving: F) {
        self.serving
            .store(ticket.wrapping_add(1), Ordering::Release);
        after_serving();
        let _ = self
            .owner
            .compare_exchange(cpu, NO_OWNER, Ordering::Relaxed, Ordering::Relaxed);
        self.release_cpu(cpu);
    }

    /// Whether some CPU holds the lock (a racy snapshot).
    pub fn is_locked(&self) -> bool {
        self.queued() != 0
    }

    /// Tickets taken and not yet released: holder plus waiters (snapshot).
    pub fn queued(&self) -> u32 {
        let next = self.next.load(Ordering::Relaxed);
        next.wrapping_sub(self.serving.load(Ordering::Relaxed))
    }

    /// Current owner CPU id (diagnostic snapshot).
    pub fn owner(&self) -> Option<u32> {
        match self.owner.load(Ordering::Relaxed) {
            NO_OWNER => None,
            cpu => Some(cpu),
        }
    }

    /// Mutable access without locking; `&mut self` proves exclusivity.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    /// Consumes the lock.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

/// Proof of holding a [`TicketLock`]; releases it and restores the IRQ state
/// on drop. Not `Send`: it must be released on the CPU that acquired it.
///
/// Leaking a guard (`core::mem::forget`, a reference cycle, an endless loop
/// while holding it) keeps the lock held forever: every later caller spins
/// with its IRQs disabled, and the leaking CPU keeps IRQs disabled as well.
/// As with `std::sync::Mutex`, not leaking guards is the caller's contract;
/// the lock cannot detect it.
#[must_use = "dropping the guard immediately releases the lock"]
pub struct TicketGuard<'a, T, I: IrqControl> {
    lock: &'a TicketLock<T>,
    irq: &'a I,
    state: Option<I::State>,
    ticket: u32,
    cpu: u32,
    _not_send: PhantomData<*mut ()>,
}

impl<T, I: IrqControl> TicketGuard<'_, T, I> {
    /// The ticket this guard was served with (acquisition sequence number).
    pub fn ticket(&self) -> u32 {
        self.ticket
    }
}

impl<T, I: IrqControl> Deref for TicketGuard<'_, T, I> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while its ticket is being served, so
        // no other guard for this lock exists (see the `Sync` impl). The
        // pointer comes from a live `UnsafeCell` inside `self.lock`, which
        // outlives the guard, is non-null and properly aligned. Shared access
        // is tied to `&self`, so it cannot overlap `deref_mut`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T, I: IrqControl> DerefMut for TicketGuard<'_, T, I> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, this guard is the unique holder; `&mut self`
        // makes the returned reference the only one derived from this guard,
        // and it cannot outlive the guard, which releases the lock on drop.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T, I: IrqControl> Drop for TicketGuard<'_, T, I> {
    fn drop(&mut self) {
        self.lock.release_ticket_with(self.cpu, self.ticket, || {});
        if let Some(state) = self.state.take() {
            self.irq.restore(state);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{IrqControl, LockError, TicketLock};
    use core::cell::Cell;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    struct ModelIrq {
        cpu: u32,
        enabled: Cell<bool>,
    }

    impl ModelIrq {
        fn new(cpu: u32) -> Self {
            Self {
                cpu,
                enabled: Cell::new(true),
            }
        }
    }

    impl IrqControl for ModelIrq {
        type State = bool;

        fn save_and_disable(&self) -> bool {
            self.enabled.replace(false)
        }

        fn restore(&self, state: bool) {
            self.enabled.set(state);
        }

        fn current_cpu(&self) -> u32 {
            assert!(!self.enabled.get());
            self.cpu
        }
    }

    struct ReleaseWaiter(Arc<AtomicBool>);

    impl Drop for ReleaseWaiter {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn handoff_window_rejects_nmi_and_preserves_new_owner() {
        let lock = Arc::new(TicketLock::new(()));
        let irq0 = ModelIrq::new(0);
        let mut held = lock.lock(&irq0).unwrap();
        let ticket = held.ticket;
        let saved_irq_state = held.state.take().unwrap();
        core::mem::forget(held);

        let waiter_acquired = Arc::new(AtomicBool::new(false));
        let release_waiter = Arc::new(AtomicBool::new(false));
        let worker_lock = lock.clone();
        let worker_acquired = waiter_acquired.clone();
        let worker_release = release_waiter.clone();
        let waiter = thread::spawn(move || {
            let irq1 = ModelIrq::new(1);
            let _guard = worker_lock.lock(&irq1).unwrap();
            worker_acquired.store(true, Ordering::Release);
            while !worker_release.load(Ordering::Acquire) {
                thread::yield_now();
            }
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while lock.queued() != 2 {
            assert!(Instant::now() < deadline, "CPU 1 never queued");
            thread::yield_now();
        }

        // Ensure a failed assertion cannot strand the modeled next owner.
        let _release_waiter = ReleaseWaiter(release_waiter.clone());
        lock.release_ticket_with(0, ticket, || {
            assert_eq!(
                lock.lock(&irq0).err(),
                Some(LockError::Recursive { cpu: 0 }),
                "NMI re-entry must remain blocked after serving advances"
            );
            while !waiter_acquired.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "CPU 1 never acquired handoff");
                thread::yield_now();
            }
            assert_eq!(lock.owner(), Some(1));
        });

        irq0.restore(saved_irq_state);
        assert_eq!(
            lock.owner(),
            Some(1),
            "releasing CPU 0 must not erase the new owner's diagnostic id"
        );
        release_waiter.store(true, Ordering::Release);
        waiter.join().unwrap();
        assert_eq!(lock.owner(), None);
        assert_eq!(lock.queued(), 0);
    }
}
