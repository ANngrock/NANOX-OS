mod common;

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use hw_smp::{IrqControl, LockError, TicketLock, TryLockError};

/// Model of one CPU's interrupt flag. Not shared: each thread is one CPU.
struct ModelIrq {
    cpu: u32,
    enabled: Cell<bool>,
    saves: Cell<u32>,
}

impl ModelIrq {
    fn new(cpu: u32) -> Self {
        Self {
            cpu,
            enabled: Cell::new(true),
            saves: Cell::new(0),
        }
    }
}

impl IrqControl for ModelIrq {
    type State = bool;
    fn save_and_disable(&self) -> bool {
        self.saves.set(self.saves.get() + 1);
        self.enabled.replace(false)
    }
    fn restore(&self, state: bool) {
        self.enabled.set(state);
    }
    fn current_cpu(&self) -> u32 {
        assert!(!self.enabled.get(), "cpu id read with IRQs enabled");
        self.cpu
    }
}

const THREADS: usize = 8;
const ITERS: u64 = 15_000;

#[test]
fn mutual_exclusion_under_contention() {
    common::with_watchdog(300, || {
        let lock = Arc::new(TicketLock::new(0u64));
        let inside = Arc::new(AtomicU32::new(0));
        let would_block = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|cpu| {
                let (lock, inside, would_block, start) = (
                    lock.clone(),
                    inside.clone(),
                    would_block.clone(),
                    start.clone(),
                );
                thread::spawn(move || {
                    let irq = ModelIrq::new(cpu as u32);
                    start.wait();
                    for i in 0..ITERS {
                        let mut g = if i % 5 == 0 {
                            loop {
                                match lock.try_lock(&irq) {
                                    Ok(g) => break g,
                                    Err(TryLockError::WouldBlock) => {
                                        assert!(irq.enabled.get(), "restored after refusal");
                                        would_block.fetch_add(1, Ordering::Relaxed);
                                        std::hint::spin_loop();
                                    }
                                    Err(e) => panic!("{e:?}"),
                                }
                            }
                        } else {
                            lock.lock(&irq).unwrap()
                        };
                        assert!(!irq.enabled.get(), "IRQs off inside the lock");
                        assert_eq!(inside.fetch_add(1, Ordering::Relaxed), 0, "two holders");
                        // Non-atomic read-modify-write with a widened window.
                        let v = *g;
                        for _ in 0..(i % 8) {
                            std::hint::spin_loop();
                        }
                        *g = v + 1;
                        inside.fetch_sub(1, Ordering::Relaxed);
                        drop(g);
                        assert!(irq.enabled.get(), "IRQs restored after unlock");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let lock = Arc::try_unwrap(lock).ok().unwrap();
        assert_eq!(lock.into_inner(), THREADS as u64 * ITERS, "lost updates");
        eprintln!(
            "mutual exclusion: {} increments, {} try_lock refusals",
            THREADS as u64 * ITERS,
            would_block.load(Ordering::Relaxed)
        );
    });
}

#[test]
fn waiters_are_served_in_ticket_order() {
    common::with_watchdog(60, || {
        let lock = Arc::new(TicketLock::new(Vec::<(u32, u32)>::new()));
        let main_irq = ModelIrq::new(0);
        let held = lock.lock(&main_irq).unwrap();
        let mut handles = Vec::new();
        for cpu in 1..=THREADS as u32 {
            let lock2 = lock.clone();
            handles.push(thread::spawn(move || {
                let irq = ModelIrq::new(cpu);
                let mut g = lock2.lock(&irq).unwrap();
                let ticket = g.ticket();
                g.push((cpu, ticket));
            }));
            // Wait until this waiter has taken its ticket before starting the
            // next one, so arrival order is known.
            let deadline = Instant::now() + Duration::from_secs(10);
            while lock.queued() != cpu + 1 {
                assert!(Instant::now() < deadline, "waiter {cpu} never queued");
                thread::yield_now();
            }
        }
        assert_eq!(lock.owner(), Some(0));
        drop(held);
        for h in handles {
            h.join().unwrap();
        }
        let order = lock.lock(&main_irq).unwrap().clone();
        let cpus: Vec<u32> = order.iter().map(|&(c, _)| c).collect();
        assert_eq!(cpus, (1..=THREADS as u32).collect::<Vec<_>>(), "FIFO");
        assert!(order.windows(2).all(|w| w[1].1 == w[0].1 + 1));
    });
}

#[test]
fn recursive_acquire_is_refused_without_side_effects() {
    let lock = TicketLock::new(1u32);
    let irq = ModelIrq::new(3);
    let g = lock.lock(&irq).unwrap();
    assert_eq!(lock.owner(), Some(3));
    assert_eq!(lock.lock(&irq).err(), Some(LockError::Recursive { cpu: 3 }));
    assert_eq!(
        lock.try_lock(&irq).err(),
        Some(TryLockError::Recursive { cpu: 3 })
    );
    assert!(!irq.enabled.get(), "outer guard still masks IRQs");
    assert_eq!(lock.queued(), 1, "refusals took no ticket");
    drop(g);
    assert!(irq.enabled.get());
    assert_eq!(lock.owner(), None);
    assert!(!lock.is_locked());
    // The lock still works for the same CPU afterwards.
    assert_eq!(*lock.lock(&irq).unwrap(), 1);
}

#[test]
fn irq_state_is_restored_in_lifo_order() {
    let a = TicketLock::new(());
    let b = TicketLock::new(());
    let irq = ModelIrq::new(1);
    let ga = a.lock(&irq).unwrap();
    let gb = b.lock(&irq).unwrap();
    assert!(!irq.enabled.get());
    drop(gb);
    assert!(!irq.enabled.get(), "inner release keeps IRQs off");
    drop(ga);
    assert!(irq.enabled.get(), "outer release re-enables");

    // IRQs already off before locking stay off after unlocking.
    irq.enabled.set(false);
    drop(a.lock(&irq).unwrap());
    assert!(!irq.enabled.get());
}

#[test]
fn try_lock_refuses_while_another_cpu_holds() {
    common::with_watchdog(30, || {
        let lock = Arc::new(TicketLock::new(0u32));
        let holding = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let t = {
            let (lock, holding, release) = (lock.clone(), holding.clone(), release.clone());
            thread::spawn(move || {
                let irq = ModelIrq::new(1);
                let mut g = lock.lock(&irq).unwrap();
                *g = 7;
                holding.wait();
                release.wait();
            })
        };
        holding.wait();
        let irq = ModelIrq::new(0);
        assert_eq!(lock.try_lock(&irq).err(), Some(TryLockError::WouldBlock));
        assert!(irq.enabled.get(), "state restored after WouldBlock");
        assert_eq!(irq.saves.get(), 1, "IRQs were masked for the attempt");
        release.wait();
        t.join().unwrap();
        assert_eq!(
            *lock.try_lock(&irq).unwrap(),
            7,
            "sees previous holder's write"
        );
    });
}

#[test]
fn reserved_cpu_id_is_rejected() {
    let lock = TicketLock::new(());
    let irq = ModelIrq::new(u32::MAX);
    assert_eq!(lock.lock(&irq).err(), Some(LockError::InvalidCpuId));
    assert_eq!(lock.try_lock(&irq).err(), Some(TryLockError::InvalidCpuId));
    assert!(irq.enabled.get());
    assert!(!lock.is_locked());
}
