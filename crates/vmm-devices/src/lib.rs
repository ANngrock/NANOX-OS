//! Emulated devices for the M10 VMM (docs/specs/M10-VMM.md, item 2): what
//! a NANOX M1+ kernel needs from the platform to calibrate and run its
//! local APIC timer as a guest.
//!
//! * [`decode`] — the MOV forms a guest uses for MMIO, so a nested page
//!   fault on the APIC page can be emulated (no decode assists or AVIC in
//!   QEMU TCG);
//! * [`lapic`] — an xAPIC register model with the timer (one-shot and
//!   periodic, divide configuration), IRR/ISR, TPR and EOI;
//! * [`pit`] — the 8254 PIT, its three channels and port 0x61 (channel 2's
//!   gate and OUT2): the calibration reference, and the IRQ0 tick;
//! * for a Linux guest (docs/specs/M11-WINDOW.md §5, step 2): [`uart`] (16550A
//!   console), [`pic`] (the two 8259A and the ELCR), [`rtc`] (MC146818 and
//!   CMOS), [`legacy`] (port 0x92, POST, DMA registers, parallel probe),
//!   [`ioapic`], [`hpet`], [`i8042`] (PS/2 controller and keyboard),
//!   [`acpi_pm`] (PM1 events, timer, GPE0, SMI command, soft-off),
//!   [`machine`] (all of them on one port and memory bus, with the chipset's
//!   interrupt wiring: 8259, I/O APIC, local APIC) and
//!   [`map`], the table of the 37 regions a Linux kernel was measured to touch
//!   and which of them these modules provide.
//!
//! Devices are pure state machines over a virtual time in nanoseconds that
//! the VMM passes in; they never read a clock, so a run is reproducible.
//! No `unsafe`, no allocation, no dependencies.

#![no_std]
#![forbid(unsafe_code)]

pub mod acpi_pm;
pub mod decode;
pub mod hpet;
pub mod i8042;
pub mod ioapic;
pub mod lapic;
pub mod legacy;
pub mod machine;
pub mod map;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod rtc;
pub mod uart;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_console;
pub mod virtio_gpu;
pub mod virtio_input;
pub mod virtio_net;

pub const NS_PER_SEC: u64 = 1_000_000_000;

/// `count` events of `hz` since `ns` nanoseconds: floor(ns * hz / 1e9).
pub(crate) fn events_in(ns: u64, hz: u64) -> u64 {
    (u128::from(ns) * u128::from(hz) / u128::from(NS_PER_SEC)) as u64
}

/// First time (ns, rounded up) at which `events` events of `hz` happened.
pub(crate) fn ns_for_events(events: u64, hz: u64) -> u64 {
    let n = u128::from(events) * u128::from(NS_PER_SEC);
    n.div_ceil(u128::from(hz)).min(u128::from(u64::MAX)) as u64
}
