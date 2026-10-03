//! The platform: every device of this crate on one I/O and memory bus, and
//! the wiring between them that a PC chipset provides. The VMM hands it the
//! guest's port accesses ([`Machine::io_in`], [`Machine::io_out`]) and its
//! accesses to device memory ([`Machine::mmio_read`], [`Machine::mmio_write`]);
//! it hands the guest an interrupt when [`Machine::acknowledge`] says there is
//! one, and asks [`Machine::next_event`] when to call back.
//!
//! Wiring (as the ACPI tables of a q35 machine describe it):
//!
//! * ISA IRQ lines go to both the 8259 pair and the I/O APIC, pin = IRQ, except
//!   IRQ0 which is pin 2 (the usual interrupt-source override);
//! * IRQ1 keyboard, IRQ4 COM1, IRQ8 RTC, IRQ9 the ACPI SCI (level, and active
//!   low electrically at the I/O APIC, as the override says), and the HPET: in
//!   legacy replacement mode timer 0 is IRQ0 and timer 1 is IRQ8, otherwise a
//!   timer goes to the pin of its route;
//! * the I/O APIC's messages are fixed or lowest-priority interrupts for the
//!   one CPU: they land in the local APIC's IRR; an ExtINT message and the
//!   8259's INT are seen only while the local APIC is off or its LINT0 passes
//!   ExtINT (the virtual wire); other delivery modes are counted;
//! * an EOI at the local APIC tells the I/O APIC, which clears remote IRR.
//!
//! Not modeled: the PIT's channel 0 (nothing drives IRQ0 but the HPET), the
//! PCI bus (the configuration ports answer "no device" until it exists), and
//! more than one CPU.

use crate::acpi_pm::AcpiPm;
use crate::hpet::{self, Hpet};
use crate::i8042::I8042;
use crate::ioapic::{self, IoApic};
use crate::lapic::{self, Lapic};
use crate::legacy::Legacy;
use crate::pic::Pic;
use crate::pit::Pit2;
use crate::rtc::Rtc;
use crate::uart::Uart;

pub const PCI_ADDRESS: u16 = 0xCF8;
pub const PCI_DATA: u16 = 0xCFC;
const SCI_IRQ: u8 = 9;

/// The ISA IRQ lines the platform drives.
pub mod irq {
    pub const TIMER: u8 = 0;
    pub const KEYBOARD: u8 = 1;
    pub const COM1: u8 = 4;
    pub const RTC: u8 = 8;
    pub const SCI: u8 = 9;
}

/// Where an ISA IRQ is wired on the I/O APIC.
pub fn ioapic_pin(irq: u8) -> u8 {
    if irq == irq::TIMER {
        2
    } else {
        irq
    }
}

#[derive(Clone, Debug)]
pub struct Machine {
    pub uart: Uart,
    pub pic: Pic,
    pub pit: Pit2,
    pub rtc: Rtc,
    pub legacy: Legacy,
    pub ioapic: IoApic,
    pub hpet: Hpet,
    pub kbd: I8042,
    pub pm: AcpiPm,
    pub lapic: Lapic,
    pci_address: u32,
    /// Port reads, port writes and memory accesses nothing claimed.
    pub unclaimed_in: u32,
    pub unclaimed_out: u32,
    pub unclaimed_mmio: u32,
    /// I/O APIC messages that are neither fixed nor lowest-priority nor ExtINT.
    pub other_messages: u32,
}

/// All-ones of an access size: what an empty bus returns.
fn ones(size: u8) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

impl Machine {
    /// `epoch_secs`: the unix time the RTC shows at virtual time 0; `bus_hz`: the local APIC timer's input clock.
    pub fn new(epoch_secs: i64, bus_hz: u64) -> Self {
        Self {
            uart: Uart::new(),
            pic: Pic::new(),
            pit: Pit2::new(),
            rtc: Rtc::new(epoch_secs),
            legacy: Legacy::new(),
            ioapic: IoApic::new(),
            hpet: Hpet::new(),
            kbd: I8042::new(),
            pm: AcpiPm::new(),
            lapic: Lapic::new(bus_hz),
            pci_address: 0,
            unclaimed_in: 0,
            unclaimed_out: 0,
            unclaimed_mmio: 0,
            other_messages: 0,
        }
    }

    // ---------------------------------------------------------------- ports

    /// A port read of `size` bytes (1, 2 or 4). A byte-wide device read with a wider access gets the next ports' bytes too.
    pub fn io_in(&mut self, port: u16, size: u8, now: u64) -> u32 {
        self.sync(now);
        let v = match self.read_whole(port, size, now) {
            Some(v) => v,
            // A wide access to byte-wide devices is several byte accesses at consecutive ports.
            None if size > 1 => {
                let mut v = 0u32;
                for i in 0..size {
                    let b = match self.read_whole(port.wrapping_add(u16::from(i)), 1, now) {
                        Some(b) => b,
                        None => {
                            self.unclaimed_in += 1;
                            0xFF
                        }
                    };
                    v |= b << (8 * u32::from(i));
                }
                v
            }
            None => {
                self.unclaimed_in += 1;
                ones(size)
            }
        };
        // Reading can change a line (reading a status register drops its interrupt).
        self.sync(now);
        v
    }

    /// A port write of `size` bytes (1, 2 or 4).
    pub fn io_out(&mut self, port: u16, size: u8, value: u32, now: u64) {
        self.sync(now);
        if !self.write_whole(port, size, value, now) {
            if size > 1 {
                for i in 0..size {
                    let b = (value >> (8 * u32::from(i))) & 0xFF;
                    if !self.write_whole(port.wrapping_add(u16::from(i)), 1, b, now) {
                        self.unclaimed_out += 1;
                    }
                }
            } else {
                self.unclaimed_out += 1;
            }
        }
        self.sync(now);
    }

    /// The device that takes an access of exactly this size at this port, if any.
    fn read_whole(&mut self, port: u16, size: u8, now: u64) -> Option<u32> {
        match port {
            PCI_ADDRESS if size == 4 => Some(self.pci_address),
            PCI_DATA..=0xCFF => Some(ones(size)), // no device answers
            p if AcpiPm::owns(p) => self.pm.read(p, size, now),
            p if size == 1 => self.read_byte(p, now).map(u32::from),
            _ => None,
        }
    }

    fn read_byte(&mut self, p: u16, now: u64) -> Option<u8> {
        if Uart::owns(p) {
            self.uart.read(p)
        } else if Pic::owns(p) {
            self.pic.read(p)
        } else if Rtc::owns(p) {
            self.rtc.read(p, now)
        } else if I8042::owns(p) {
            self.kbd.read(p)
        } else if Legacy::owns(p) {
            self.legacy.read(p)
        } else {
            self.pit.read(p, now)
        }
    }

    fn write_whole(&mut self, port: u16, size: u8, value: u32, now: u64) -> bool {
        match port {
            PCI_ADDRESS if size == 4 => {
                self.pci_address = value;
                true
            }
            PCI_DATA..=0xCFF => true,
            p if AcpiPm::owns(p) => self.pm.write(p, size, value, now),
            p if size == 1 => self.write_byte(p, value as u8, now),
            _ => false,
        }
    }

    fn write_byte(&mut self, p: u16, v: u8, now: u64) -> bool {
        if Uart::owns(p) {
            self.uart.write(p, v)
        } else if Pic::owns(p) {
            self.pic.write(p, v)
        } else if Rtc::owns(p) {
            self.rtc.write(p, v, now)
        } else if I8042::owns(p) {
            self.kbd.write(p, v)
        } else if Legacy::owns(p) {
            self.legacy.write(p, v)
        } else {
            self.pit.write(p, v, now)
        }
    }

    // --------------------------------------------------------------- memory

    /// A memory read of `size` bytes at physical address `addr` in device space.
    pub fn mmio_read(&mut self, addr: u64, size: u8, now: u64) -> u64 {
        self.sync(now);
        let lapic_base = self.lapic.base();
        let found =
            if (ioapic::DEFAULT_BASE..ioapic::DEFAULT_BASE + 0x1000).contains(&addr) && size == 4 {
                self.ioapic.read(addr - ioapic::DEFAULT_BASE).map(u64::from)
            } else if (hpet::DEFAULT_BASE..hpet::DEFAULT_BASE + hpet::SIZE).contains(&addr) {
                self.hpet.read(addr - hpet::DEFAULT_BASE, size, now)
            } else if (lapic_base..lapic_base + 0x1000).contains(&addr)
                && size == 4
                && addr.is_multiple_of(16)
            {
                Some(u64::from(self.lapic.read((addr - lapic_base) as u32, now)))
            } else {
                None
            };
        let v = found.unwrap_or_else(|| {
            self.unclaimed_mmio += 1;
            u64::from(ones(size.min(4))) | if size == 8 { u64::MAX << 32 } else { 0 }
        });
        self.sync(now);
        v
    }

    /// A memory write of `size` bytes at physical address `addr` in device space.
    pub fn mmio_write(&mut self, addr: u64, size: u8, value: u64, now: u64) {
        self.sync(now);
        let lapic_base = self.lapic.base();
        let ok =
            if (ioapic::DEFAULT_BASE..ioapic::DEFAULT_BASE + 0x1000).contains(&addr) && size == 4 {
                self.ioapic.write(addr - ioapic::DEFAULT_BASE, value as u32)
            } else if (hpet::DEFAULT_BASE..hpet::DEFAULT_BASE + hpet::SIZE).contains(&addr) {
                self.hpet.write(addr - hpet::DEFAULT_BASE, size, value, now)
            } else if (lapic_base..lapic_base + 0x1000).contains(&addr)
                && size == 4
                && addr.is_multiple_of(16)
            {
                let off = (addr - lapic_base) as u32;
                // An EOI completes the highest vector in service; level interrupts of the I/O APIC hear of it.
                let done = (off == lapic::reg::EOI)
                    .then(|| self.lapic.in_service())
                    .flatten();
                self.lapic.write(off, value as u32, now);
                if let Some(v) = done {
                    self.ioapic.eoi(v);
                }
                true
            } else {
                false
            };
        if !ok {
            self.unclaimed_mmio += 1;
        }
        self.sync(now);
    }

    // ----------------------------------------------------------- interrupts

    /// Brings every device up to `now` and moves the interrupt lines through the chipset to the CPU.
    pub fn sync(&mut self, now: u64) {
        self.hpet.sync(now);
        self.lapic.update(now);

        // Level lines: what the devices drive now.
        let sci = self.pm.sci(now);
        let lines: [(u8, bool); 4] = [
            (irq::KEYBOARD, self.kbd.irq1()),
            (irq::COM1, self.uart.irq()),
            (irq::RTC, self.rtc.irq(now)),
            (irq::SCI, sci),
        ];
        for (n, level) in lines {
            self.pic.set_irq(n, level);
            // The SCI is wired active low at the I/O APIC.
            self.ioapic
                .set_irq(ioapic_pin(n), if n == SCI_IRQ { !level } else { level });
        }
        // HPET: edge pulses, and level timers on the pin of their route.
        while let Some(f) = self.hpet.take_fire() {
            self.pic.set_irq(f.irq, true);
            self.pic.set_irq(f.irq, false);
            let pin = ioapic_pin(f.irq);
            self.ioapic.set_irq(pin, true);
            self.ioapic.set_irq(pin, false);
        }
        for t in 0..hpet::TIMERS {
            let route = self.hpet_level_pin(t);
            let level = self.hpet.line(t, now);
            if let Some(pin) = route {
                self.ioapic.set_irq(pin, level);
            }
        }
        // I/O APIC messages to the CPU.
        while let Some(m) = self.ioapic.take_message() {
            match m.mode {
                0 | 1 => self.lapic.raise(m.vector),
                // ExtINT: the 8259 sees the same line itself, so there is nothing to carry.
                7 => {}
                _ => self.other_messages += 1,
            }
        }
    }

    /// The I/O APIC pin of a level-triggered HPET timer (its route; legacy replacement is edge-only).
    fn hpet_level_pin(&self, t: usize) -> Option<u8> {
        let cfg = self.hpet.timer_config(t);
        if cfg & 2 == 0 {
            return None; // edge
        }
        Some(((cfg >> 9) & 0x1F) as u8)
    }

    fn pic_visible(&self) -> bool {
        !self.lapic.is_enabled() || self.lapic.lint0_extint()
    }

    /// The vector the CPU should take now, if it can: the local APIC's, else the 8259's through the virtual wire.
    pub fn pending(&mut self, now: u64) -> Option<u8> {
        self.sync(now);
        if let Some(v) = self.lapic.pending() {
            return Some(v);
        }
        // The vector is only fixed at the acknowledge cycle; report the one the 8259 would give.
        (self.pic_visible() && self.pic.int_pending()).then(|| self.pic.clone().acknowledge())
    }

    /// The CPU takes the interrupt `pending` reported: the local APIC moves it to in-service, or the 8259 runs its acknowledge cycle.
    pub fn acknowledge(&mut self, now: u64) -> Option<u8> {
        self.sync(now);
        if let Some(v) = self.lapic.pending() {
            self.lapic.accept(v);
            return Some(v);
        }
        if self.pic_visible() && self.pic.int_pending() {
            return Some(self.pic.acknowledge());
        }
        None
    }

    /// When the VMM must call `sync` next: the earliest deadline of any device.
    pub fn next_event(&self, now: u64) -> Option<u64> {
        [
            self.rtc.next_event(now),
            self.hpet.next_event(now),
            self.pm.next_event(now),
            self.lapic.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    // --------------------------------------------------------------- outputs

    /// The A20 gate: enabled by either of its two controls (port 0x92 and the keyboard controller).
    pub fn a20_enabled(&self) -> bool {
        self.legacy.a20_enabled() || self.kbd.a20_enabled()
    }

    /// The guest asked for a reset (port 0x92 or the keyboard controller); reading clears the request.
    pub fn take_reset(&mut self) -> bool {
        let a = self.legacy.take_reset();
        let b = self.kbd.take_reset();
        a || b
    }

    /// The guest asked for a sleep state; `acpi_pm::SLEEP_S5` is power off.
    pub fn take_sleep(&mut self) -> Option<u8> {
        self.pm.take_sleep()
    }

    /// The host presses the power button (an ACPI event the guest sees on IRQ9).
    pub fn press_power_button(&mut self) {
        self.pm.press_power_button();
    }

    /// The host types a key: `byte` is a set-2 scancode byte.
    pub fn key(&mut self, byte: u8) {
        self.kbd.push_scancode(byte);
    }
}
