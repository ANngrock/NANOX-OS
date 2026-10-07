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
//! * IRQ0 the PIT's channel 0 (an edge for each rising edge of its OUT), IRQ1
//!   keyboard, IRQ4 COM1, IRQ8 RTC, IRQ9 the ACPI SCI (level, and active low
//!   electrically at the I/O APIC, as the override says), and the HPET: in
//!   legacy replacement mode timer 0 is IRQ0 and timer 1 is IRQ8, and the PIT
//!   no longer reaches IRQ0 (as on ICH chipsets and in QEMU); otherwise a timer
//!   goes to the pin of its route;
//! * the I/O APIC's messages are fixed or lowest-priority interrupts for the
//!   one CPU: they land in the local APIC's IRR; an ExtINT message and the
//!   8259's INT are seen only while the local APIC is off or its LINT0 passes
//!   ExtINT (the virtual wire); other delivery modes are counted;
//! * an EOI at the local APIC tells the I/O APIC, which clears remote IRR.
//!
//! * PCI bus 0: the host bridge (slot 0), virtio-blk (slot 3), virtio-net
//!   (slot 4), virtio-gpu (slot 5), virtio-console, the agent channel (slot
//!   6), and virtio-input as a keyboard (slot 7) and a tablet (slot 8);
//!   configuration through ports 0xCF8/0xCFC and ECAM, BAR 0 of the virtio
//!   devices as memory the guest placed. Their INTA goes to I/O APIC pin 16 +
//!   slot % 4 (active low: 19, 16, 17, 18, 19 and 16; functions on one pin are
//!   wired-or) and to the 8259 line in the interrupt-line register (11, 10, 5,
//!   3, 14 and 6: lines no ISA device or the SCI uses, and none shared between
//!   the functions). Their DMA runs in `service_blk` / `service_net` /
//!   `service_gpu` / `service_console` / `service_input`.
//!
//! Not modeled: MSI, PCI bridges, and more than one CPU.

use crate::acpi_pm::AcpiPm;
use crate::hpet::{self, Hpet};
use crate::i8042::I8042;
use crate::ioapic::{self, IoApic};
use crate::lapic::{self, Lapic};
use crate::legacy::Legacy;
use crate::pci::{BarKind, Config};
use crate::pic::Pic;
use crate::pit::Pit;
use crate::rtc::Rtc;
use crate::uart::Uart;
use crate::virtio::{GuestMemory, VirtioPci};
use crate::virtio_blk::{BlockBackend, VirtioBlk};
use crate::virtio_console::{ConsoleBackend, VirtioConsole};
use crate::virtio_gpu::{self, Scanout, VirtioGpu};
use crate::virtio_input::VirtioInput;
use crate::virtio_net::{NetBackend, VirtioNet};

pub const PCI_ADDRESS: u16 = 0xCF8;
pub const PCI_DATA: u16 = 0xCFC;
/// The memory-mapped configuration window (ECAM), as the q35 chipset places it.
pub const ECAM_BASE: u64 = 0xB000_0000;
pub const ECAM_SIZE: u64 = 0x1000_0000;
/// PCI device numbers on bus 0.
pub mod slot {
    pub const HOST_BRIDGE: u8 = 0;
    pub const BLK: u8 = 3;
    pub const NET: u8 = 4;
    pub const GPU: u8 = 5;
    pub const CONSOLE: u8 = 6;
    pub const KEYBOARD: u8 = 7;
    pub const TABLET: u8 = 8;
}
/// The ISA lines the PCI devices' INTA is routed to on the 8259 (the interrupt line register tells the guest).
/// IRQ 5 is the display's and IRQ 3 the agent channel's: no ISA device of this platform uses them
/// (they have 1, 4, 8, 12 and the SCI on 9; COM2 is not modeled). The keyboard has IRQ 14 (there is
/// no IDE controller) and the tablet IRQ 6 (there is no floppy controller); 7 and 15 are left alone,
/// the 8259s report spurious interrupts there.
const BLK_PIC_LINE: u8 = 11;
const NET_PIC_LINE: u8 = 10;
const GPU_PIC_LINE: u8 = 5;
const CONSOLE_PIC_LINE: u8 = 3;
const KEYBOARD_PIC_LINE: u8 = 14;
const TABLET_PIC_LINE: u8 = 6;
/// Where a PCI device's INTx lands on the I/O APIC: pins 16..20, rotated by the slot (the q35 swizzle).
pub fn pci_pin(dev: u8) -> u8 {
    16 + dev % 4
}
const SCI_IRQ: u8 = 9;
/// The card's MAC address: locally administered, unicast.
pub const NET_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x4E, 0x58, 0x01];

/// The ISA IRQ lines the platform drives.
pub mod irq {
    pub const TIMER: u8 = 0;
    pub const KEYBOARD: u8 = 1;
    pub const COM1: u8 = 4;
    pub const RTC: u8 = 8;
    pub const SCI: u8 = 9;
    pub const MOUSE: u8 = 12;
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
    pub pit: Pit,
    pub rtc: Rtc,
    pub legacy: Legacy,
    pub ioapic: IoApic,
    pub hpet: Hpet,
    pub kbd: I8042,
    pub pm: AcpiPm,
    pub lapic: Lapic,
    /// virtio-blk at 00:03.0, virtio-net at 00:04.0, virtio-gpu at 00:05.0 and the agent channel
    /// (virtio-console) at 00:06.0.
    pub blk: VirtioBlk,
    pub net: VirtioNet,
    pub gpu: VirtioGpu,
    pub console: VirtioConsole,
    /// virtio-input at 00:07.0 (a keyboard; the PS/2 one is `kbd`) and 00:08.0 (a tablet over the
    /// display's default size).
    pub keyboard: VirtioInput,
    pub tablet: VirtioInput,
    pci_address: u32,
    /// The 8259 lines driven at the last sync (to release one a function moved away from).
    pci_lines: u16,
    /// The host bridge (00:00.0), the Intel q35 MCH as Linux expects to find it.
    pub host_bridge: Config,
    /// Port reads, port writes and memory accesses nothing claimed.
    pub unclaimed_in: u32,
    pub unclaimed_out: u32,
    pub unclaimed_mmio: u32,
    /// I/O APIC messages that are neither fixed nor lowest-priority nor ExtINT.
    pub other_messages: u32,
    /// PIT ticks merged into another's IRQ0 edge because the VMM synced after both.
    pub pit_coalesced: u64,
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
            pit: Pit::new(),
            rtc: Rtc::new(epoch_secs),
            legacy: Legacy::new(),
            ioapic: IoApic::new(),
            hpet: Hpet::new(),
            kbd: I8042::new(),
            pm: AcpiPm::new(),
            lapic: Lapic::new(bus_hz),
            blk: VirtioBlk::new(0, BLK_PIC_LINE),
            net: VirtioNet::new(NET_MAC, NET_PIC_LINE),
            gpu: VirtioGpu::new(GPU_PIC_LINE),
            console: VirtioConsole::new(CONSOLE_PIC_LINE),
            keyboard: VirtioInput::keyboard(KEYBOARD_PIC_LINE),
            tablet: VirtioInput::tablet(
                TABLET_PIC_LINE,
                virtio_gpu::DEFAULT_WIDTH,
                virtio_gpu::DEFAULT_HEIGHT,
            ),
            pci_address: 0,
            pci_lines: 0,
            host_bridge: host_bridge(),
            unclaimed_in: 0,
            unclaimed_out: 0,
            unclaimed_mmio: 0,
            other_messages: 0,
            pit_coalesced: 0,
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
            PCI_ADDRESS..=0xCFB if port + u16::from(size) <= PCI_DATA => {
                Some(self.pci_address_read(port, size))
            }
            PCI_DATA..=0xCFF => Some(self.pci_data_read(port, size)),
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
            PCI_ADDRESS..=0xCFB if port + u16::from(size) <= PCI_DATA => {
                self.pci_address_write(port, size, value);
                true
            }
            PCI_DATA..=0xCFF => {
                self.pci_data_write(port, size, value);
                true
            }
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

    // ------------------------------------------------------------------ PCI

    /// The configuration space of a function on bus 0, if one is there.
    fn config_of(&mut self, bus: u8, dev: u8, func: u8) -> Option<&mut Config> {
        if bus != 0 || func != 0 {
            return None;
        }
        match dev {
            slot::HOST_BRIDGE => Some(&mut self.host_bridge),
            slot::BLK => Some(&mut self.blk.t.cfg),
            slot::NET => Some(&mut self.net.t.cfg),
            slot::GPU => Some(&mut self.gpu.t.cfg),
            slot::CONSOLE => Some(&mut self.console.t.cfg),
            slot::KEYBOARD => Some(&mut self.keyboard.t.cfg),
            slot::TABLET => Some(&mut self.tablet.t.cfg),
            _ => None,
        }
    }

    /// Reads configuration space; an empty slot answers all ones. Offsets inside the PCIe extended range (256..4096) read as ones.
    fn config_read(&mut self, bus: u8, dev: u8, func: u8, off: usize, size: u8) -> u32 {
        match self.config_of(bus, dev, func) {
            Some(c) => c.read(off, size),
            None => ones(size),
        }
    }

    fn config_write(&mut self, bus: u8, dev: u8, func: u8, off: usize, size: u8, value: u32) {
        if let Some(c) = self.config_of(bus, dev, func) {
            c.write(off, size, value);
        }
    }

    /// The address register at 0xCF8..0xCFB, read as a whole or in parts.
    fn pci_address_read(&self, port: u16, size: u8) -> u32 {
        let shift = 8 * u32::from(port - PCI_ADDRESS);
        (self.pci_address >> shift) & (u32::MAX >> (32 - 8 * u32::from(size)))
    }

    fn pci_address_write(&mut self, port: u16, size: u8, value: u32) {
        let shift = 8 * u32::from(port - PCI_ADDRESS);
        let mask = (u32::MAX >> (32 - 8 * u32::from(size))) << shift;
        self.pci_address = (self.pci_address & !mask) | ((value << shift) & mask);
    }

    /// Port 0xCFC..0xCFF: the data window of configuration mechanism #1, addressed by the register at 0xCF8.
    fn pci_data_read(&mut self, port: u16, size: u8) -> u32 {
        let a = self.pci_address;
        if a & (1 << 31) == 0 {
            return ones(size);
        }
        let off = ((a & 0xFC) + u32::from(port - PCI_DATA)) as usize;
        self.config_read(
            (a >> 16) as u8,
            ((a >> 11) & 31) as u8,
            ((a >> 8) & 7) as u8,
            off,
            size,
        )
    }

    fn pci_data_write(&mut self, port: u16, size: u8, value: u32) {
        let a = self.pci_address;
        if a & (1 << 31) == 0 {
            return;
        }
        let off = ((a & 0xFC) + u32::from(port - PCI_DATA)) as usize;
        self.config_write(
            (a >> 16) as u8,
            ((a >> 11) & 31) as u8,
            ((a >> 8) & 7) as u8,
            off,
            size,
            value,
        );
    }

    // --------------------------------------------------------------- memory

    /// A memory read of `size` bytes at physical address `addr` in device space.
    pub fn mmio_read(&mut self, addr: u64, size: u8, now: u64) -> u64 {
        self.sync(now);
        let lapic_base = self.lapic.base();
        let found = if (ioapic::DEFAULT_BASE..ioapic::DEFAULT_BASE + 0x1000).contains(&addr)
            && size == 4
        {
            self.ioapic.read(addr - ioapic::DEFAULT_BASE).map(u64::from)
        } else if (hpet::DEFAULT_BASE..hpet::DEFAULT_BASE + hpet::SIZE).contains(&addr) {
            self.hpet.read(addr - hpet::DEFAULT_BASE, size, now)
        } else if (lapic_base..lapic_base + 0x1000).contains(&addr)
            && size == 4
            && addr.is_multiple_of(16)
        {
            Some(u64::from(self.lapic.read((addr - lapic_base) as u32, now)))
        } else if (ECAM_BASE..ECAM_BASE + ECAM_SIZE).contains(&addr) && matches!(size, 1 | 2 | 4) {
            let a = addr - ECAM_BASE;
            Some(u64::from(self.config_read(
                (a >> 20) as u8,
                ((a >> 15) & 31) as u8,
                ((a >> 12) & 7) as u8,
                (a & 0xFFF) as usize,
                size,
            )))
        } else if matches!(size, 1 | 2 | 4) && self.virtio_hit(addr).is_some() {
            let (dev, off) = self.virtio_hit(addr).unwrap_or((0, 0));
            Some(u64::from(self.virtio_function(dev).mmio_read(off, size)))
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
        let ok = if (ioapic::DEFAULT_BASE..ioapic::DEFAULT_BASE + 0x1000).contains(&addr)
            && size == 4
        {
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
        } else if (ECAM_BASE..ECAM_BASE + ECAM_SIZE).contains(&addr) && matches!(size, 1 | 2 | 4) {
            let a = addr - ECAM_BASE;
            self.config_write(
                (a >> 20) as u8,
                ((a >> 15) & 31) as u8,
                ((a >> 12) & 7) as u8,
                (a & 0xFFF) as usize,
                size,
                value as u32,
            );
            true
        } else if matches!(size, 1 | 2 | 4) && self.virtio_hit(addr).is_some() {
            let (dev, off) = self.virtio_hit(addr).unwrap_or((0, 0));
            let v = value as u32;
            // The input devices take writes to their device configuration themselves.
            match dev {
                slot::KEYBOARD => self.keyboard.mmio_write(off, size, v),
                slot::TABLET => self.tablet.mmio_write(off, size, v),
                _ => self.virtio_function(dev).mmio_write(off, size, v),
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

    /// The virtio functions and their slots.
    fn virtio_functions(&self) -> [(u8, &VirtioPci); 6] {
        [
            (slot::BLK, &self.blk.t),
            (slot::NET, &self.net.t),
            (slot::GPU, &self.gpu.t),
            (slot::CONSOLE, &self.console.t),
            (slot::KEYBOARD, &self.keyboard.t),
            (slot::TABLET, &self.tablet.t),
        ]
    }

    /// Which virtio device (its slot) decodes `addr` in its BAR 0, and the offset in it.
    pub fn virtio_hit(&self, addr: u64) -> Option<(u8, u64)> {
        self.virtio_functions()
            .into_iter()
            .find_map(|(dev, t)| match t.cfg.memory_hit(addr) {
                Some((0, off)) => Some((dev, off)),
                _ => None,
            })
    }

    /// The transport of the virtio function in slot `dev` (one `virtio_hit` found).
    fn virtio_function(&mut self, dev: u8) -> &mut VirtioPci {
        match dev {
            slot::BLK => &mut self.blk.t,
            slot::GPU => &mut self.gpu.t,
            slot::CONSOLE => &mut self.console.t,
            slot::KEYBOARD => &mut self.keyboard.t,
            slot::TABLET => &mut self.tablet.t,
            _ => &mut self.net.t,
        }
    }

    // -------------------------------------------------------------- virtio

    /// Serves the disk's virtqueue (the guest's DMA goes through `mem`), then lets the interrupt reach the chipset.
    pub fn service_blk(
        &mut self,
        mem: &mut dyn GuestMemory,
        be: &mut dyn BlockBackend,
        now: u64,
    ) -> u32 {
        let n = self.blk.service(mem, be);
        self.sync(now);
        n
    }

    /// Sends what the guest transmitted and delivers what the network has for it.
    pub fn service_net(
        &mut self,
        mem: &mut dyn GuestMemory,
        be: &mut dyn NetBackend,
        now: u64,
    ) -> (u32, u32) {
        let r = self.net.service(mem, be);
        self.sync(now);
        r
    }

    /// Serves the display's queues: the control commands change the resources and reach `scan`;
    /// returns (control commands, cursor commands) completed. A reset of the device is noticed here too.
    pub fn service_gpu(
        &mut self,
        mem: &mut dyn GuestMemory,
        scan: &mut dyn Scanout,
        now: u64,
    ) -> (u32, u32) {
        let r = self.gpu.service(mem, scan);
        self.sync(now);
        r
    }

    /// Moves the agent channel's bytes both ways; returns (chains sent, chains filled).
    pub fn service_console(
        &mut self,
        mem: &mut dyn GuestMemory,
        be: &mut dyn ConsoleBackend,
        now: u64,
    ) -> (u32, u32) {
        let r = self.console.service(mem, be);
        self.sync(now);
        r
    }

    /// Delivers the keyboard's and the tablet's queued events and takes what the guest sent them
    /// (the LEDs); returns (event buffers, status buffers) completed by the two.
    pub fn service_input(&mut self, mem: &mut dyn GuestMemory, now: u64) -> (u32, u32) {
        let (ke, ks) = self.keyboard.service(mem);
        let (te, ts) = self.tablet.service(mem);
        self.sync(now);
        (ke + te, ks + ts)
    }

    // ----------------------------------------------------------- interrupts

    /// Brings every device up to `now` and moves the interrupt lines through the chipset to the CPU.
    pub fn sync(&mut self, now: u64) {
        self.hpet.sync(now);
        self.lapic.update(now);

        // The keyboard controller's buffer was read and refilled: its lines went low before one
        // of them rises again.
        if self.kbd.take_reloaded() {
            for n in [irq::KEYBOARD, irq::MOUSE] {
                self.pic.set_irq(n, false);
                self.ioapic.set_irq(ioapic_pin(n), false);
            }
        }
        // Level lines: what the devices drive now. The 8259 takes the wired-or of an ISA device
        // and the PCI functions routed to the same line.
        let sci = self.pm.sci(now);
        let isa: [(u8, bool); 5] = [
            (irq::KEYBOARD, self.kbd.irq1()),
            (irq::MOUSE, self.kbd.irq12()),
            (irq::COM1, self.uart.irq()),
            (irq::RTC, self.rtc.irq(now)),
            (irq::SCI, sci),
        ];
        let mut levels = 0u16;
        let mut driven = 0u16;
        for (n, level) in isa {
            driven |= 1 << n;
            levels |= u16::from(level) << n;
            // The SCI is wired active low at the I/O APIC.
            self.ioapic
                .set_irq(ioapic_pin(n), if n == SCI_IRQ { !level } else { level });
        }
        // PCI INTx: level, active low at the I/O APIC, high while asserted on the 8259's line.
        // Functions on one I/O APIC pin are wired-or as well (bit n: pin 16 + n asserted).
        let mut pins = 0u8;
        for (dev, t) in self.virtio_functions() {
            let (line, level) = (t.cfg.interrupt_line(), t.irq());
            if line < 16 {
                driven |= 1 << line;
                levels |= u16::from(level) << line;
            }
            pins |= u8::from(level) << (pci_pin(dev) - 16);
        }
        for n in 0..4 {
            self.ioapic.set_irq(16 + n, pins >> n & 1 == 0);
        }
        // A line the guest moved a function away from is released.
        let released = self.pci_lines & !driven;
        self.pci_lines = driven;
        for n in 0..16u8 {
            if (driven | released) >> n & 1 != 0 {
                self.pic.set_irq(n, levels >> n & 1 != 0);
            }
        }
        // HPET: edge pulses, and level timers on the pin of their route.
        while let Some(f) = self.hpet.take_fire() {
            self.pulse(f.irq);
        }
        // PIT channel 0: an edge on IRQ0, unless the HPET in legacy replacement mode has the line.
        // Ticks a late sync finds together make one edge, as the 8259's request bit would merge them.
        let ticks = self.pit.take_edges(now);
        if ticks > 0 && !self.hpet.legacy_replacement() {
            self.pulse(irq::TIMER);
            self.pit_coalesced += ticks - 1;
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

    /// An edge on an ISA IRQ line, at the 8259 and at its I/O APIC pin.
    fn pulse(&mut self, irq: u8) {
        self.pic.set_irq(irq, true);
        self.pic.set_irq(irq, false);
        let pin = ioapic_pin(irq);
        self.ioapic.set_irq(pin, true);
        self.ioapic.set_irq(pin, false);
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
            self.pit
                .next_event(now)
                .filter(|_| !self.hpet.legacy_replacement()),
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

/// 00:00.0, the q35 memory controller hub: Intel 8086:29C0, host bridge class, QEMU's subsystem id.
fn host_bridge() -> Config {
    let mut c = Config::new(0x8086, 0x29C0, 0x06_0000, 0, (0x1AF4, 0x1100), 0);
    c.define_bar(0, BarKind::Unused);
    c
}
