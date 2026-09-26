//! IRQ-aware FIFO ticket spinlock.
//!
//! Acquisition order:
//! 1. save the IRQ state and disable IRQs ([`IrqControl::save_and_disable`]);
//! 2. read the executing CPU id; if this CPU already owns the lock, restore
//!    the IRQ state and return [`LockError::Recursive`] **without taking a
//!    ticket**, so the lock is unaffected and nothing deadlocks;
//! 3. take a ticket and spin until it is served (FIFO fairness).
//!
//! Release (guard drop) clears the owner, hands the lock to the next ticket and
//! only then restores the saved IRQ state: IRQs are never enabled while the
//! lock is held. Guards must be dropped in LIFO order when several locks are
//! held (the project rule of M1 is to avoid nesting altogether).
//!
//! The recursion check is always on, not only in debug builds: it costs one
//! relaxed load of a word the lock already touches, and a deadlocked CPU with
//! IRQs disabled is undiagnosable. The typical source is an NMI (or a buggy
//! path) re-entering code that holds the lock.
//!
//! Memory ordering:
//! - `next.fetch_add(Relaxed)`: a ticket number publishes no data.
//! - `serving.load(Acquire)` in the spin loop pairs with the
//!   `serving.store(Release)` of the previous holder, so everything written in
//!   the previous critical section is visible in the next one.
//! - `try_lock` loads `serving` with `Acquire` and then claims ticket
//!   `serving` by `compare_exchange` on `next`; success means no ticket was
//!   outstanding, so `serving` could not move in between. The ABA case would
//!   need 2^32 acquisitions between two adjacent instructions executed with
//!   IRQs disabled, which is unreachable; 32-bit tickets are kept on purpose.
//! - `owner` is `Relaxed`: it is only compared with the *executing* CPU id.
//!   A CPU reads its own id there only if it stored it and has not yet
//!   cleared it (read-after-write coherence on one location); other CPUs'
//!   values can never equal ours. For other CPUs it is diagnostic only.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// Interrupt masking and CPU identity of the executing CPU, implemented by the
/// caller (kernel: `pushfq; cli` / `popfq` and a per-CPU id; tests: a model).
pub trait IrqControl {
    /// Saved interrupt state.
    type State;
    /// Saves the current IRQ state and disables IRQs.
    fn save_and_disable(&self) -> Self::State;
    /// Restores a state returned by [`save_and_disable`](Self::save_and_disable).
    fn restore(&self, state: Self::State);
    /// Id of the executing CPU. Called only with IRQs disabled, so the caller
    /// cannot migrate. Must not be `u32::MAX` (reserved for "no owner").
    fn current_cpu(&self) -> u32;
}

const NO_OWNER: u32 = u32::MAX;

/// Why [`TicketLock::lock`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockError {
    /// The executing CPU already holds this lock.
    Recursive {
        /// The CPU id.
        cpu: u32,
    },
    /// [`IrqControl::current_cpu`] returned the reserved `u32::MAX`.
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
    /// [`IrqControl::current_cpu`] returned the reserved `u32::MAX`.
    InvalidCpuId,
}

/// FIFO ticket spinlock protecting a `T`.
pub struct TicketLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
    owner: AtomicU32,
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
        if cpu == NO_OWNER {
            irq.restore(state);
            return Err(LockError::InvalidCpuId);
        }
        if self.owner.load(Ordering::Relaxed) == cpu {
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
        if cpu == NO_OWNER {
            irq.restore(state);
            return Err(TryLockError::InvalidCpuId);
        }
        if self.owner.load(Ordering::Relaxed) == cpu {
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
            _not_send: PhantomData,
        }
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
        self.lock.owner.store(NO_OWNER, Ordering::Relaxed);
        self.lock
            .serving
            .store(self.ticket.wrapping_add(1), Ordering::Release);
        if let Some(state) = self.state.take() {
            self.irq.restore(state);
        }
    }
}
