//! What a Linux guest touched on a q35 machine (docs/research/linux-guest-surface.md,
//! measured by `tools/hostguest/linux_surface.py`) and which of it this crate
//! provides. The table is the work list of the VMM for a Linux guest: each of
//! the 37 measured regions is `Done` by a named module, `Partial` (with what is
//! missing), `Pending` for a planned step (docs/specs/M11-WINDOW.md §5) or
//! `HostOnly`: something only QEMU's own firmware and hardware need, which a
//! VMM with `virtio` devices and its own firmware does not.
//!
//! The tests tie the table to the measurement (every region of the report is
//! here, no other) and to the devices (every `Done` port is claimed by its module).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Provided by the named module of this crate.
    Done(&'static str),
    /// Provided in part; the text says what is not.
    Partial(&'static str, &'static str),
    /// Planned: the step it belongs to.
    Pending(&'static str),
    /// Needed only by QEMU's own devices or firmware.
    HostOnly(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// The name QEMU gives the region in its trace.
    pub name: &'static str,
    /// Inclusive address ranges whose end points the guest used (ports below 0x10000, otherwise
    /// physical addresses); a region the guest touched sparsely lists its lowest and highest only.
    pub ranges: &'static [(u64, u64)],
    pub status: Status,
}

use Status::{Done, HostOnly, Partial, Pending};

pub const MEASURED: &[Region] = &[
    Region { name: "vga-lowmem", ranges: &[(0xA0000, 0xBFFFF)], status: Pending("display") },
    Region { name: "pcspk", ranges: &[(0x61, 0x61)], status: Done("pit") },
    Region { name: "pci-conf-idx", ranges: &[(0xCF8, 0xCFB)], status: Done("pci") },
    Region { name: "pci-conf-data", ranges: &[(0xCFC, 0xCFF)], status: Done("pci") },
    Region { name: "serial", ranges: &[(0x3F8, 0x3FE)], status: Done("uart") },
    Region { name: "apic-msi", ranges: &[(0xFEE0_0000, 0xFEE0_03E0)], status: Done("lapic") },
    Region { name: "vga", ranges: &[(0x3C0, 0x3DA)], status: Pending("display") },
    Region { name: "io", ranges: &[(0xF1, 0xF1), (0x402, 0x402)], status: HostOnly("0xF1 FPU reset (legacy), 0x402 QEMU debug console") },
    Region { name: "hpet", ranges: &[(0xFED0_0000, 0xFED0_0148)], status: Done("hpet") },
    Region { name: "ahci", ranges: &[(0xFEBD_5000, 0xFEBD_53A8)], status: HostOnly("replaced by virtio-blk") },
    Region { name: "acpi-tmr", ranges: &[(0x608, 0x608)], status: Done("acpi-pm") },
    Region { name: "ioapic", ranges: &[(0xFEC0_0000, 0xFEC0_0010)], status: Done("ioapic") },
    Region { name: "rtc-index", ranges: &[(0x70, 0x70)], status: Done("rtc") },
    Region { name: "port92", ranges: &[(0x92, 0x92)], status: Done("legacy") },
    Region { name: "pcie-mmcfg-mmio", ranges: &[(0xB000_0002, 0xB00F_B040)], status: Done("pci") },
    Region { name: "edid", ranges: &[(0xFEBD_4000, 0xFEBD_40FF)], status: HostOnly("QEMU VGA's EDID; a virtio-gpu display has its own") },
    Region { name: "pit", ranges: &[(0x40, 0x43)], status: Partial("pit", "only channel 2 (ports 0x42, 0x43); channel 0, the system tick Linux calibrates against, is not modeled") },
    Region { name: "ioport80", ranges: &[(0x80, 0x80)], status: Done("legacy") },
    Region { name: "i8042-cmd", ranges: &[(0x64, 0x64)], status: Done("i8042") },
    Region { name: "rtc", ranges: &[(0x71, 0x71)], status: Done("rtc") },
    Region { name: "pic", ranges: &[(0x20, 0x21), (0xA0, 0xA1)], status: Done("pic") },
    Region { name: "i8042-data", ranges: &[(0x60, 0x60)], status: Done("i8042") },
    Region { name: "acpi-gpe0", ranges: &[(0x620, 0x62F)], status: Done("acpi-pm") },
    Region { name: "fwcfg.dma", ranges: &[(0x518, 0x518)], status: HostOnly("QEMU firmware configuration") },
    Region { name: "apm-io", ranges: &[(0xB2, 0xB3)], status: Done("acpi-pm") },
    Region { name: "acpi-evt", ranges: &[(0x600, 0x602)], status: Done("acpi-pm") },
    Region { name: "vbe", ranges: &[(0x1CE, 0x1CF)], status: HostOnly("QEMU VGA's Bochs interface") },
    Region { name: "fwcfg", ranges: &[(0x510, 0x511)], status: HostOnly("QEMU firmware configuration") },
    Region { name: "acpi-cpu-hotplug", ranges: &[(0xCD8, 0xCDC)], status: HostOnly("QEMU CPU hotplug; one fixed CPU set") },
    Region { name: "acpi-cnt", ranges: &[(0x604, 0x604)], status: Done("acpi-pm") },
    Region { name: "dma-cont", ranges: &[(0x0D, 0x0D), (0xDA, 0xDA)], status: Done("legacy") },
    Region { name: "parallel", ranges: &[(0x378, 0x37A)], status: Done("legacy") },
    Region { name: "elcr", ranges: &[(0x4D0, 0x4D1)], status: Done("pic") },
    Region { name: "acpi-smi", ranges: &[(0x630, 0x630)], status: HostOnly("QEMU's SMI control of the ICH9") },
    Region { name: "kvmvapic", ranges: &[(0x7E, 0x7E)], status: HostOnly("QEMU's TPR optimisation for Windows guests") },
    Region { name: "ioportF0", ranges: &[(0xF0, 0xF0)], status: Done("legacy") },
    Region { name: "dma-page", ranges: &[(0x87, 0x87)], status: Done("legacy") },
];

/// How many regions are provided, partly provided, planned, and not needed.
pub fn tally() -> (usize, usize, usize, usize) {
    let count = |f: fn(&Status) -> bool| MEASURED.iter().filter(|r| f(&r.status)).count();
    (
        count(|s| matches!(s, Done(_))),
        count(|s| matches!(s, Partial(..))),
        count(|s| matches!(s, Pending(_))),
        count(|s| matches!(s, HostOnly(_))),
    )
}
