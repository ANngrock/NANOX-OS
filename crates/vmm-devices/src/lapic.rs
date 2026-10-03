//! Local APIC in xAPIC mode (MMIO page at the APIC base) for one vCPU:
//! registers, the timer, IRR/ISR/TPR priority and EOI (Intel SDM Vol. 3
//! ch. 11, AMD APM Vol. 2 ch. 16). Not modeled: IPIs (ICR writes are stored
//! and counted), LINT pins, thermal/performance/error interrupts, TSC-
//! deadline mode, x2APIC. The timer counts at `bus_hz / divide`.

use crate::{events_in, ns_for_events};

pub const DEFAULT_BASE: u64 = 0xFEE0_0000;
pub const MSR_APIC_BASE: u32 = 0x1B;
pub const MSR_BSP: u64 = 1 << 8;
pub const MSR_X2APIC: u64 = 1 << 10;
pub const MSR_ENABLE: u64 = 1 << 11;
const MSR_BASE_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Register offsets in the APIC page.
pub mod reg {
    pub const ID: u32 = 0x020;
    pub const VERSION: u32 = 0x030;
    pub const TPR: u32 = 0x080;
    pub const APR: u32 = 0x090;
    pub const PPR: u32 = 0x0A0;
    pub const EOI: u32 = 0x0B0;
    pub const LDR: u32 = 0x0D0;
    pub const DFR: u32 = 0x0E0;
    pub const SVR: u32 = 0x0F0;
    pub const ISR: u32 = 0x100;
    pub const TMR: u32 = 0x180;
    pub const IRR: u32 = 0x200;
    pub const ESR: u32 = 0x280;
    pub const ICR_LOW: u32 = 0x300;
    pub const ICR_HIGH: u32 = 0x310;
    pub const LVT_TIMER: u32 = 0x320;
    pub const LVT_THERMAL: u32 = 0x330;
    pub const LVT_PERF: u32 = 0x340;
    pub const LVT_LINT0: u32 = 0x350;
    pub const LVT_LINT1: u32 = 0x360;
    pub const LVT_ERROR: u32 = 0x370;
    pub const TIMER_INITIAL: u32 = 0x380;
    pub const TIMER_CURRENT: u32 = 0x390;
    pub const TIMER_DIVIDE: u32 = 0x3E0;
}

pub const LVT_MASKED: u32 = 1 << 16;
pub const LVT_PERIODIC: u32 = 1 << 17;
const LVT_TIMER_BITS: u32 = 0xFF | LVT_MASKED | 3 << 17;
const LVT_OTHER_BITS: u32 = 0xFF | 7 << 8 | 1 << 13 | 1 << 15 | LVT_MASKED;
pub const SVR_ENABLE: u32 = 1 << 8;
/// Version 0x14 (integrated APIC), six LVT entries.
pub const VERSION: u32 = 0x0005_0014;

/// An IA32_APIC_BASE value this model does not support (x2APIC, another
/// base, clearing BSP).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsrRefused;

#[derive(Clone, Debug)]
pub struct Lapic {
    base_msr: u64,
    bus_hz: u64,
    tpr: u32,
    svr: u32,
    ldr: u32,
    dfr: u32,
    icr: [u32; 2],
    /// Timer, thermal, perf, LINT0, LINT1, error.
    lvt: [u32; 6],
    divide: u32,
    initial: u32,
    /// Start of the current count-down.
    start_ns: u64,
    /// Expirations since `start_ns` already processed.
    fired: u64,
    irr: [u32; 8],
    isr: [u32; 8],
    /// Timer expirations that found their vector already pending (or
    /// several in one update) and were merged, as real hardware does.
    pub coalesced: u64,
    /// Writes to unmodeled registers (ICR, read-only registers).
    pub ignored_writes: u32,
}

/// Divide configuration bits 3,1,0 → divisor.
fn divisor(divide: u32) -> u64 {
    match (divide >> 1 & 4) | (divide & 3) {
        7 => 1,
        v => 2 << v,
    }
}

impl Lapic {
    /// State after INIT/reset of the bootstrap processor with `bus_hz` as
    /// the timer's input clock; the APIC is globally enabled (as firmware
    /// leaves it) and software-disabled (SVR = 0xFF).
    pub fn new(bus_hz: u64) -> Self {
        assert!(bus_hz > 0, "bus_hz");
        Self {
            base_msr: DEFAULT_BASE | MSR_BSP | MSR_ENABLE,
            bus_hz,
            tpr: 0,
            svr: 0xFF,
            ldr: 0,
            dfr: 0xFFFF_FFFF,
            icr: [0; 2],
            lvt: [LVT_MASKED; 6],
            divide: 0,
            initial: 0,
            start_ns: 0,
            fired: 0,
            irr: [0; 8],
            isr: [0; 8],
            coalesced: 0,
            ignored_writes: 0,
        }
    }

    pub fn base(&self) -> u64 {
        self.base_msr & MSR_BASE_MASK
    }

    pub fn read_msr(&self) -> u64 {
        self.base_msr
    }

    /// WRMSR IA32_APIC_BASE. x2APIC and moving the base are refused (the
    /// VMM injects #GP); only the global enable bit may change.
    pub fn write_msr(&mut self, value: u64) -> Result<(), MsrRefused> {
        let allowed = MSR_BASE_MASK | MSR_BSP | MSR_ENABLE;
        if value & !allowed != 0
            || value & MSR_BASE_MASK != self.base()
            || value & MSR_BSP != self.base_msr & MSR_BSP
        {
            return Err(MsrRefused);
        }
        self.base_msr = value;
        Ok(())
    }

    fn enabled(&self) -> bool {
        self.base_msr & MSR_ENABLE != 0 && self.svr & SVR_ENABLE != 0
    }

    fn timer_hz(&self) -> u64 {
        (self.bus_hz / divisor(self.divide)).max(1)
    }

    fn periodic(&self) -> bool {
        self.lvt[0] & (3 << 17) == LVT_PERIODIC
    }

    fn elapsed_counts(&self, now: u64) -> u64 {
        events_in(now.saturating_sub(self.start_ns), self.timer_hz())
    }

    fn expirations(&self, now: u64) -> u64 {
        if self.initial == 0 {
            return 0;
        }
        let n = self.elapsed_counts(now) / u64::from(self.initial);
        if self.periodic() {
            n
        } else {
            n.min(1)
        }
    }

    /// Current Count register at `now`.
    pub fn current_count(&self, now: u64) -> u32 {
        if self.initial == 0 {
            return 0;
        }
        let e = self.elapsed_counts(now);
        let initial = u64::from(self.initial);
        if self.periodic() {
            (initial - e % initial) as u32
        } else {
            initial.saturating_sub(e) as u32
        }
    }

    /// Brings the timer up to `now`: new expirations set the timer vector
    /// in IRR unless the LVT entry is masked or the APIC disabled.
    pub fn update(&mut self, now: u64) {
        let n = self.expirations(now);
        if n <= self.fired {
            return;
        }
        let new = n - self.fired;
        self.fired = n;
        if self.lvt[0] & LVT_MASKED != 0 || !self.enabled() {
            return;
        }
        let v = self.lvt[0] & 0xFF;
        if v < 16 {
            return; // illegal vector: no interrupt (ESR not modeled)
        }
        let (w, b) = ((v / 32) as usize, 1 << (v % 32));
        let merged = new - 1 + u64::from(self.irr[w] & b != 0);
        self.coalesced += merged;
        self.irr[w] |= b;
    }

    /// When the timer next expires, if it is running unmasked.
    pub fn next_deadline(&self) -> Option<u64> {
        if self.initial == 0 || self.lvt[0] & LVT_MASKED != 0 || !self.enabled() {
            return None;
        }
        if !self.periodic() && self.fired >= 1 {
            return None;
        }
        let counts = (self.fired + 1).checked_mul(u64::from(self.initial))?;
        self.start_ns
            .checked_add(ns_for_events(counts, self.timer_hz()))
    }

    fn highest(bits: &[u32; 8]) -> Option<u32> {
        (0..8)
            .rev()
            .find(|&w| bits[w] != 0)
            .map(|w| w as u32 * 32 + 31 - bits[w].leading_zeros())
    }

    fn ppr(&self) -> u32 {
        let isr = Self::highest(&self.isr).unwrap_or(0) & 0xF0;
        (self.tpr & 0xFF).max(isr)
    }

    /// The vector to deliver when the guest can take an interrupt: the
    /// highest in IRR whose priority class is above the processor priority.
    pub fn pending(&self) -> Option<u8> {
        if !self.enabled() {
            return None;
        }
        let v = Self::highest(&self.irr)?;
        (v & 0xF0 > self.ppr() & 0xF0).then_some(v as u8)
    }

    /// An interrupt arrives from outside (the I/O APIC): the vector is set in IRR. Vectors below 16 are illegal and ignored.
    pub fn raise(&mut self, v: u8) {
        if v >= 16 {
            self.irr[usize::from(v / 32)] |= 1 << (v % 32);
        }
    }

    /// The highest vector in service, the one the next EOI completes.
    pub fn in_service(&self) -> Option<u8> {
        Self::highest(&self.isr).map(|v| v as u8)
    }

    /// Is the APIC enabled (globally and by the spurious-vector register)?
    pub fn is_enabled(&self) -> bool {
        self.enabled()
    }

    /// Does LINT0 pass the 8259's interrupts to the CPU (unmasked, ExtINT delivery)? Linux masks it once it uses the I/O APIC.
    pub fn lint0_extint(&self) -> bool {
        let l = self.lvt[3];
        self.enabled() && l & LVT_MASKED == 0 && (l >> 8) & 7 == 7
    }

    /// The vector was injected: IRR → ISR.
    pub fn accept(&mut self, v: u8) {
        let (w, b) = (usize::from(v / 32), 1 << (v % 32));
        self.irr[w] &= !b;
        self.isr[w] |= b;
    }

    /// Reads the 32-bit register at `offset` (16-byte aligned) at `now`.
    pub fn read(&mut self, offset: u32, now: u64) -> u32 {
        self.update(now);
        let group = |bits: &[u32; 8], base: u32| bits[((offset - base) / 16) as usize];
        match offset {
            reg::ID => 0,
            reg::VERSION => VERSION,
            reg::TPR => self.tpr,
            reg::APR => 0,
            reg::PPR => self.ppr(),
            reg::LDR => self.ldr,
            reg::DFR => self.dfr,
            reg::SVR => self.svr,
            0x100..=0x170 if offset.is_multiple_of(16) => group(&self.isr, reg::ISR),
            0x200..=0x270 if offset.is_multiple_of(16) => group(&self.irr, reg::IRR),
            reg::ICR_LOW => self.icr[0],
            reg::ICR_HIGH => self.icr[1],
            0x320..=0x370 if offset.is_multiple_of(16) => {
                self.lvt[((offset - reg::LVT_TIMER) / 16) as usize]
            }
            reg::TIMER_INITIAL => self.initial,
            reg::TIMER_CURRENT => self.current_count(now),
            reg::TIMER_DIVIDE => self.divide,
            _ => 0, // TMR, ESR, reserved
        }
    }

    /// Writes the 32-bit register at `offset` at `now`.
    pub fn write(&mut self, offset: u32, value: u32, now: u64) {
        self.update(now);
        match offset {
            reg::TPR => self.tpr = value & 0xFF,
            reg::EOI => {
                if let Some(v) = Self::highest(&self.isr) {
                    self.isr[(v / 32) as usize] &= !(1 << (v % 32));
                }
            }
            reg::LDR => self.ldr = value & 0xFF00_0000,
            reg::DFR => self.dfr = value | 0x0FFF_FFFF,
            reg::SVR => {
                self.svr = value & 0x13FF;
                if self.svr & SVR_ENABLE == 0 {
                    // Software disable masks every LVT entry.
                    for l in &mut self.lvt {
                        *l |= LVT_MASKED;
                    }
                }
            }
            reg::ICR_LOW | reg::ICR_HIGH => {
                self.icr[usize::from(offset == reg::ICR_HIGH)] = value;
                self.ignored_writes += 1;
            }
            reg::LVT_TIMER => {
                let mut v = value & LVT_TIMER_BITS;
                if self.svr & SVR_ENABLE == 0 {
                    v |= LVT_MASKED;
                }
                self.lvt[0] = v;
            }
            0x330..=0x370 if offset.is_multiple_of(16) => {
                let mut v = value & LVT_OTHER_BITS;
                if self.svr & SVR_ENABLE == 0 {
                    v |= LVT_MASKED;
                }
                self.lvt[((offset - reg::LVT_TIMER) / 16) as usize] = v;
            }
            reg::TIMER_INITIAL => {
                self.initial = value;
                self.start_ns = now;
                self.fired = 0;
            }
            reg::TIMER_DIVIDE => {
                // Keep the count continuous: rebase the running count-down
                // onto the new rate.
                let current = self.current_count(now);
                self.divide = value & 0xB;
                if self.initial != 0 && current != 0 {
                    let done = u64::from(self.initial - current);
                    self.start_ns = now.saturating_sub(ns_for_events(done, self.timer_hz()));
                    self.fired = 0;
                }
            }
            _ => self.ignored_writes += 1,
        }
    }
}
