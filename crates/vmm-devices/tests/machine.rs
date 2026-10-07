//! The platform bus: ports and memory dispatch, interrupt wiring through the
//! 8259, the I/O APIC and the local APIC, and what Linux does through them.

use vmm_devices::acpi_pm::{
    ACPI_ENABLE, PM1_CONTROL, PM1_ENABLE, PM1_STATUS, SLEEP_S5, SMI_CMD, STS_PWRBTN,
};
use vmm_devices::hpet;
use vmm_devices::ioapic;
use vmm_devices::lapic::{self, reg};
use vmm_devices::machine::*;
use vmm_devices::map::{Status, MEASURED};

const NS: u64 = 1_000_000_000;
const T0: i64 = 1_790_944_496;
const LAPIC: u64 = lapic::DEFAULT_BASE;
const IOAPIC: u64 = ioapic::DEFAULT_BASE;
const HPET: u64 = hpet::DEFAULT_BASE;

fn machine() -> Machine {
    Machine::new(T0, 100_000_000)
}

fn out(m: &mut Machine, port: u16, v: u8, now: u64) {
    m.io_out(port, 1, u32::from(v), now);
}

fn inb(m: &mut Machine, port: u16, now: u64) -> u8 {
    m.io_in(port, 1, now) as u8
}

/// Programs an I/O APIC redirection entry through memory-mapped registers.
fn route(m: &mut Machine, pin: u8, low: u32, now: u64) {
    m.mmio_write(IOAPIC, 4, u64::from(0x10 + 2 * pin + 1), now);
    m.mmio_write(IOAPIC + 0x10, 4, 0, now);
    m.mmio_write(IOAPIC, 4, u64::from(0x10 + 2 * pin), now);
    m.mmio_write(IOAPIC + 0x10, 4, u64::from(low), now);
}

/// Software-enables the local APIC with spurious vector 0xFF and takes LINT0 off the 8259.
fn enable_lapic(m: &mut Machine) {
    m.mmio_write(LAPIC + u64::from(reg::SVR), 4, 0x1FF, 0);
    m.mmio_write(LAPIC + u64::from(reg::LVT_LINT0), 4, 1 << 16, 0);
}

fn eoi(m: &mut Machine, now: u64) {
    m.mmio_write(LAPIC + u64::from(reg::EOI), 4, 0, now);
}

const LEVEL: u32 = 1 << 15;
const LOW: u32 = 1 << 13;

// ------------------------------------------------------------------ dispatch

#[test]
fn an_empty_bus_reads_all_ones_and_swallows_writes_and_counts_both() {
    let mut m = machine();
    assert_eq!(m.io_in(0x1234, 1, 0), 0xFF);
    assert_eq!(m.io_in(0x1234, 2, 0), 0xFFFF);
    assert_eq!(m.io_in(0x1234, 4, 0), 0xFFFF_FFFF);
    assert_eq!(
        m.unclaimed_in,
        1 + 2 + 4,
        "wide reads count each empty byte"
    );
    m.io_out(0x1234, 1, 0x55, 0);
    m.io_out(0x1234, 4, 0x1122_3344, 0);
    assert_eq!(m.unclaimed_out, 1 + 4);
    assert_eq!(m.mmio_read(0xE000_0000, 4, 0), 0xFFFF_FFFF);
    m.mmio_write(0xE000_0000, 4, 1, 0);
    assert_eq!(m.unclaimed_mmio, 2);
}

#[test]
fn every_done_region_of_the_measurement_table_is_served_by_the_bus() {
    for r in MEASURED {
        let Status::Done(module) = r.status else {
            continue;
        };
        for &(lo, hi) in r.ranges {
            for addr in [lo, hi] {
                let mut m = machine();
                if addr < 0x1_0000 {
                    // A port: a byte read must find a device (the ACPI block answers wider reads too).
                    m.io_in(addr as u16, 1, 0);
                    assert_eq!(
                        m.unclaimed_in, 0,
                        "{} port {addr:#x} ({module}) is not served",
                        r.name
                    );
                } else {
                    let size = if module == "hpet" { 8 } else { 4 };
                    let addr = addr & !0xF;
                    m.mmio_read(addr, size, 0);
                    assert_eq!(
                        m.unclaimed_mmio, 0,
                        "{} at {addr:#x} ({module}) is not served",
                        r.name
                    );
                }
            }
        }
    }
}

#[test]
fn byte_wide_devices_answer_wide_accesses_as_consecutive_bytes() {
    let mut m = machine();
    // COM1's last two ports (modem status, scratch): a 16-bit write lands as two byte writes.
    m.io_out(0x3FE, 2, 0xAB00 | 0x55, 0);
    assert_eq!(inb(&mut m, 0x3FF, 0), 0xAB);
    assert_eq!(m.unclaimed_out, 0);
    assert_eq!(
        m.io_in(0x3FF, 2, 0),
        0xFF00 | 0xAB,
        "the byte after COM1's last port is empty"
    );
    assert_eq!(m.unclaimed_in, 1);
}

#[test]
fn pci_configuration_ports_answer_for_present_functions_and_empty_slots() {
    let mut m = machine();
    m.io_out(PCI_ADDRESS, 4, 0x8000_0000, 0);
    assert_eq!(
        m.io_in(PCI_ADDRESS, 4, 0),
        0x8000_0000,
        "the address register reads back"
    );
    assert_eq!(m.io_in(PCI_DATA, 4, 0), 0x29C0_8086, "the host bridge");
    m.io_out(PCI_ADDRESS, 4, 0x8000_0000 | (31 << 11), 0);
    assert_eq!(m.io_in(PCI_DATA, 4, 0), 0xFFFF_FFFF, "nobody at 00:1f.0");
    assert_eq!(m.io_in(PCI_DATA, 2, 0), 0xFFFF);
    assert_eq!(m.io_in(PCI_DATA + 3, 1, 0), 0xFF);
    m.io_out(PCI_DATA, 4, 0x1234, 0);
    assert_eq!((m.unclaimed_in, m.unclaimed_out), (0, 0));
}

// ------------------------------------------------------------ interrupt paths

#[test]
fn com1_through_the_io_apic_to_the_local_apic() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 4, 0x34, 0);
    out(&mut m, 0x3F9, 1, 0); // IER: received data
    assert_eq!(m.pending(0), None);
    m.uart.push_rx(b'x');
    assert_eq!(m.pending(0), Some(0x34));
    assert_eq!(m.acknowledge(0), Some(0x34));
    assert_eq!(m.pending(0), None, "in service now");
    assert_eq!(inb(&mut m, 0x3F8, 0), b'x');
    eoi(&mut m, 0);
    assert_eq!(
        m.pending(0),
        None,
        "the line dropped when the byte was read"
    );
}

#[test]
fn the_8259_virtual_wire_when_the_local_apic_is_off() {
    let mut m = machine();
    // Linux-style init of the master: vectors at 0x20, IRQ4 unmasked.
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, !(1u8 << 4) & !(1 << 2)),
    ] {
        out(&mut m, p, v, 0);
    }
    out(&mut m, 0x3F9, 1, 0);
    m.uart.push_rx(1);
    assert_eq!(m.pending(0), Some(0x24));
    assert_eq!(m.acknowledge(0), Some(0x24));
    assert_eq!(m.acknowledge(0), None, "nothing else");
    out(&mut m, 0x20, 0x20, 0); // EOI to the 8259
}

#[test]
fn a_masked_lint0_hides_the_8259_from_an_enabled_local_apic() {
    let mut m = machine();
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, 0x00),
    ] {
        out(&mut m, p, v, 0);
    }
    out(&mut m, 0x3F9, 1, 0);
    m.uart.push_rx(1);
    assert!(
        m.pending(0).is_some(),
        "with the local APIC off the 8259 is visible"
    );
    enable_lapic(&mut m);
    assert_eq!(
        m.pending(0),
        None,
        "LINT0 masked: the 8259's INT goes nowhere"
    );
    // ExtINT delivery through LINT0 puts it back (the virtual-wire setup of the BSP)
    m.mmio_write(LAPIC + u64::from(reg::LVT_LINT0), 4, 7 << 8, 0);
    assert_eq!(m.pending(0), Some(0x24));
}

#[test]
fn the_keyboard_on_irq1() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 1, 0x31, 0);
    m.key(0x1C);
    assert_eq!(m.acknowledge(0), Some(0x31));
    eoi(&mut m, 0);
    assert_eq!(inb(&mut m, 0x60, 0), 0x1E);
    assert_eq!(m.pending(0), None);
    m.key(0xF0);
    m.key(0x1C);
    assert_eq!(
        m.acknowledge(0),
        Some(0x31),
        "the release is a new byte, a new interrupt"
    );
}

#[test]
fn every_byte_of_a_keyboard_reply_interrupts() {
    // Through the I/O APIC (edge-triggered pin 1)
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 1, 0x31, 0);
    out(&mut m, 0x60, 0xF2, 0); // identify
    for (i, want) in [0xFA, 0xAB, 0x83].into_iter().enumerate() {
        assert_eq!(m.acknowledge(0), Some(0x31), "byte {i}");
        eoi(&mut m, 0);
        assert_eq!(inb(&mut m, 0x60, 0), want);
    }
    assert_eq!(m.pending(0), None, "three bytes, three interrupts");
    // and through the 8259 (IRQ1 edge-triggered, the local APIC off)
    let mut m = machine();
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, 0xFD),
    ] {
        out(&mut m, p, v, 0);
    }
    out(&mut m, 0x60, 0xF2, 0);
    for want in [0xFA, 0xAB, 0x83] {
        assert_eq!(m.acknowledge(0), Some(0x21));
        out(&mut m, 0x20, 0x20, 0);
        assert_eq!(inb(&mut m, 0x60, 0), want);
    }
    assert_eq!(m.pending(0), None);
}

fn icr(m: &mut Machine, high: u32, low: u32) {
    m.mmio_write(LAPIC + 0x310, 4, u64::from(high), 0);
    m.mmio_write(LAPIC + 0x300, 4, u64::from(low), 0);
}

#[test]
fn an_ipi_to_itself_is_delivered() {
    let mut m = machine();
    enable_lapic(&mut m);
    // irq_work as Linux raises it: shorthand self, fixed, vector 0xF6
    icr(&mut m, 0, 0x4_00F6);
    assert_eq!(m.acknowledge(0), Some(0xF6));
    eoi(&mut m, 0);
    // the ICR reads back what was written, delivery status idle
    assert_eq!(m.mmio_read(LAPIC + 0x300, 4, 0), 0x4_00F6);
    // all including self: delivered; all excluding self: no one to deliver to
    icr(&mut m, 0, 0x8_0051);
    assert_eq!(m.acknowledge(0), Some(0x51));
    eoi(&mut m, 0);
    icr(&mut m, 0, 0xC_0052);
    assert_eq!(m.pending(0), None);
    // physical destination: its ID 0 and the broadcast, not another ID;
    // lowest priority as fixed
    for (dest, delivered) in [(0u32, true), (0xFF, true), (1, false)] {
        icr(&mut m, dest << 24, 0x153);
        assert_eq!(m.acknowledge(0), delivered.then_some(0x53), "dest {dest}");
        if delivered {
            eoi(&mut m, 0);
        }
    }
    assert_eq!(m.lapic.ignored_writes, 0, "all of them handled");
}

#[test]
fn logical_ipis_and_what_is_not_modeled() {
    let mut m = machine();
    enable_lapic(&mut m);
    // flat model, logical ID 1
    m.mmio_write(LAPIC + 0xE0, 4, 0xFFFF_FFFF, 0);
    m.mmio_write(LAPIC + 0xD0, 4, 1 << 24, 0);
    icr(&mut m, 0x03 << 24, 0x854);
    assert_eq!(m.acknowledge(0), Some(0x54));
    eoi(&mut m, 0);
    icr(&mut m, 0x02 << 24, 0x854);
    assert_eq!(m.pending(0), None);
    assert_eq!(m.lapic.ignored_writes, 0);
    // NMI, INIT and SIPI are counted, not delivered; so is the cluster model
    for low in [0x4_0400, 0x4_0500, 0x4_0600] {
        icr(&mut m, 0, low);
    }
    m.mmio_write(LAPIC + 0xE0, 4, 0x0FFF_FFFF, 0);
    icr(&mut m, 0x01 << 24, 0x855);
    assert_eq!(m.pending(0), None);
    assert_eq!(m.lapic.ignored_writes, 4);
}

#[test]
fn the_mouse_on_irq12() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 12, 0x3C, 0);
    out(&mut m, 0x64, 0x60, 0);
    out(&mut m, 0x60, 0x47, 0); // keyboard and mouse interrupts on
    out(&mut m, 0x64, 0xD4, 0);
    out(&mut m, 0x60, 0xF4, 0); // reporting on: the ACK interrupts
    assert_eq!(m.acknowledge(0), Some(0x3C));
    eoi(&mut m, 0);
    assert_eq!(inb(&mut m, 0x60, 0), 0xFA);
    m.kbd.push_mouse(4, 4, 0, 0);
    for want in [0x08, 4, 4] {
        assert_eq!(m.acknowledge(0), Some(0x3C), "every byte of the packet");
        eoi(&mut m, 0);
        assert_eq!(inb(&mut m, 0x60, 0), want);
    }
    assert_eq!(m.pending(0), None);
}

#[test]
fn the_rtc_periodic_interrupt_on_irq8_until_register_c_is_read() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 8, 0x38, 0);
    out(&mut m, 0x70, 0x0B, 0);
    out(&mut m, 0x71, 0x42, 0); // PIE, 24h BCD
    let t = NS / 1024 + 10;
    assert_eq!(m.acknowledge(t), Some(0x38));
    eoi(&mut m, t);
    assert_eq!(
        m.pending(t + 1),
        None,
        "the edge was taken; the line is still up, so no second one"
    );
    out(&mut m, 0x70, 0x0C, t + 1);
    assert_eq!(inb(&mut m, 0x71, t + 1) & 0xC0, 0xC0, "IRQF and PF");
    let t2 = t + NS / 1024 + 10;
    assert_eq!(
        m.acknowledge(t2),
        Some(0x38),
        "after C was read the next period interrupts again"
    );
}

#[test]
fn hpet_legacy_replacement_drives_irq0_which_is_io_apic_pin_2() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 2, 0x30, 0);
    // timer 0: periodic 1000 ticks, interrupts enabled; legacy replacement on; counter on.
    m.mmio_write(
        HPET + hpet::REG_TIMER0,
        8,
        (1 << 2) | (1 << 3) | (1 << 6),
        0,
    );
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 3, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + 8, 8, 1000, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + 8, 8, 1000, 0);
    assert_eq!(m.next_event(0), Some(100_000), "1000 ticks of 100 ns");
    assert_eq!(m.pending(99_999), None);
    assert_eq!(m.acknowledge(100_000), Some(0x30));
    eoi(&mut m, 100_000);
    assert_eq!(m.acknowledge(200_000), Some(0x30), "every period");
}

#[test]
fn hpet_timer_1_in_legacy_mode_is_the_rtc_interrupt() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 8, 0x38, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + hpet::TIMER_STRIDE, 8, 1 << 2, 0);
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 3, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + hpet::TIMER_STRIDE + 8, 8, 50, 0);
    assert_eq!(m.acknowledge(5_000), Some(0x38));
}

#[test]
fn a_level_hpet_timer_stays_asserted_until_its_status_is_cleared() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 11, 0x3B | LEVEL, 0);
    // timer 2, level, route 11
    m.mmio_write(
        HPET + hpet::REG_TIMER0 + 2 * hpet::TIMER_STRIDE,
        8,
        (1 << 1) | (1 << 2) | (11 << 9),
        0,
    );
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 1, 0);
    m.mmio_write(
        HPET + hpet::REG_TIMER0 + 2 * hpet::TIMER_STRIDE + 8,
        8,
        10,
        0,
    );
    assert_eq!(m.acknowledge(10_000), Some(0x3B));
    eoi(&mut m, 10_000);
    assert_eq!(
        m.acknowledge(10_001),
        Some(0x3B),
        "status still set: the I/O APIC sends again after EOI"
    );
    // the driver clears the status bit, then EOIs
    m.mmio_write(HPET + hpet::REG_STATUS, 8, 1 << 2, 10_002);
    eoi(&mut m, 10_002);
    assert_eq!(m.pending(10_003), None);
}

#[test]
fn the_sci_is_a_level_active_low_line_on_irq9() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 9, 0x39 | LEVEL | LOW, 0);
    m.io_out(SMI_CMD, 1, u32::from(ACPI_ENABLE), 0);
    m.io_out(PM1_ENABLE, 2, u32::from(STS_PWRBTN), 0);
    assert_eq!(m.pending(0), None, "idle: the line is high");
    m.press_power_button();
    assert_eq!(m.acknowledge(0), Some(0x39));
    eoi(&mut m, 0);
    assert_eq!(
        m.acknowledge(1),
        Some(0x39),
        "the event is still pending: level, sent again"
    );
    m.io_out(PM1_STATUS, 2, u32::from(STS_PWRBTN), 2);
    eoi(&mut m, 2);
    assert_eq!(m.pending(3), None, "cleared by the driver");
}

#[test]
fn non_fixed_ioapic_messages_are_counted_not_delivered() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 4, 4 << 8, 0); // NMI
    out(&mut m, 0x3F9, 1, 0);
    m.uart.push_rx(1);
    assert_eq!(m.pending(0), None);
    assert_eq!(m.other_messages, 1);
}

#[test]
fn an_extint_message_reaches_the_cpu_through_the_8259() {
    let mut m = machine();
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, 0x00),
    ] {
        out(&mut m, p, v, 0);
    }
    enable_lapic(&mut m);
    m.mmio_write(LAPIC + u64::from(reg::LVT_LINT0), 4, 7 << 8, 0); // LINT0 passes ExtINT
    route(&mut m, 1, 7 << 8, 0); // the keyboard pin sends ExtINT
    m.key(0x1C);
    assert_eq!(
        m.pending(0),
        Some(0x21),
        "the 8259 gives its vector for IRQ1"
    );
    assert_eq!(m.acknowledge(0), Some(0x21));
    assert_eq!(m.other_messages, 0);
    out(&mut m, 0x20, 0x20, 0);
    assert_eq!(m.acknowledge(1), None);
}

// -------------------------------------------------------------- the outputs

#[test]
fn power_off_reset_and_a20() {
    let mut m = machine();
    assert!(
        m.a20_enabled(),
        "the keyboard controller starts with A20 on"
    );
    m.io_out(0x64, 1, 0xD1, 0);
    m.io_out(0x60, 1, 0x01, 0); // output port: A20 off, but this also pulses reset (bit 0 clear is reset; here bit 0 set)
    assert!(!m.a20_enabled());
    m.io_out(0x92, 1, 0x02, 0);
    assert!(m.a20_enabled(), "port 0x92 enables it too");
    assert!(!m.take_reset());
    m.io_out(0x92, 1, 0x03, 0);
    assert!(m.take_reset());
    assert!(!m.take_reset());
    m.io_out(0x64, 1, 0xFE, 0);
    assert!(m.take_reset(), "the keyboard controller's reset pulse");
    m.io_out(SMI_CMD, 1, u32::from(ACPI_ENABLE), 0);
    m.io_out(
        PM1_CONTROL,
        2,
        1 | (u32::from(SLEEP_S5) << 10) | (1 << 13),
        0,
    );
    assert_eq!(m.take_sleep(), Some(SLEEP_S5));
}

#[test]
fn next_event_is_the_earliest_device_deadline() {
    let mut m = machine();
    assert_eq!(m.next_event(0), None, "nothing armed");
    // RTC update-ended interrupt every second
    out(&mut m, 0x70, 0x0B, 0);
    out(&mut m, 0x71, 0x12, 0);
    assert_eq!(m.next_event(0), Some(NS));
    // an HPET comparator sooner than that
    m.mmio_write(HPET + hpet::REG_TIMER0, 8, 1 << 2, 0);
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 1, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + 8, 8, 5000, 0);
    assert_eq!(m.next_event(0), Some(500_000));
    // and the local APIC timer sooner still
    m.mmio_write(LAPIC + u64::from(reg::SVR), 4, 0x1FF, 0);
    m.mmio_write(LAPIC + u64::from(reg::LVT_TIMER), 4, 0x40, 0);
    m.mmio_write(LAPIC + u64::from(reg::TIMER_DIVIDE), 4, 0b1011, 0); // divide by 1
    m.mmio_write(LAPIC + u64::from(reg::TIMER_INITIAL), 4, 1000, 0); // 10 us at 100 MHz
    assert_eq!(m.next_event(0), Some(10_000));
}

#[test]
fn the_local_apic_timer_interrupt_arrives_through_the_bus() {
    let mut m = machine();
    m.mmio_write(LAPIC + u64::from(reg::SVR), 4, 0x1FF, 0);
    m.mmio_write(LAPIC + u64::from(reg::LVT_TIMER), 4, 0x40, 0);
    m.mmio_write(LAPIC + u64::from(reg::TIMER_DIVIDE), 4, 0b1011, 0);
    m.mmio_write(LAPIC + u64::from(reg::TIMER_INITIAL), 4, 1000, 0);
    assert_eq!(m.pending(9_999), None);
    assert_eq!(m.acknowledge(10_000), Some(0x40));
    eoi(&mut m, 10_000);
}

// --------------------------------------------------------------- a boot script

#[test]
fn what_a_linux_kernel_does_in_its_first_moments_works_through_the_bus() {
    let mut m = machine();
    // serial: scratch-register probe, FIFO detection, loopback test
    for v in [0x55u8, 0xAA] {
        out(&mut m, 0x3FF, v, 0);
        assert_eq!(inb(&mut m, 0x3FF, 0), v);
    }
    out(&mut m, 0x3FA, 0x01, 0);
    assert_eq!(inb(&mut m, 0x3FA, 0) & 0xC0, 0xC0);
    // console output
    for b in b"Linux\n" {
        out(&mut m, 0x3F8, *b, 0);
    }
    let mut buf = [0u8; 16];
    assert_eq!(m.uart.take_tx(&mut buf), 6);
    assert_eq!(&buf[..6], b"Linux\n");
    // RTC: the date
    out(&mut m, 0x70, 0x09, 0);
    assert_eq!(inb(&mut m, 0x71, 0), 0x26);
    // 8042: self test and enabling the port
    out(&mut m, 0x64, 0xAA, 0);
    assert_eq!(inb(&mut m, 0x60, 0), 0x55);
    out(&mut m, 0x64, 0xAE, 0);
    // PIT channel 2 calibration through port 0x61 (the existing model)
    out(&mut m, 0x61, 0x01, 0);
    out(&mut m, 0x43, 0xB0, 0);
    out(&mut m, 0x42, 0xFF, 0);
    out(&mut m, 0x42, 0xFF, 0);
    assert_eq!(inb(&mut m, 0x61, 0) & 0x20, 0, "OUT2 low while counting");
    assert_eq!(
        inb(&mut m, 0x61, 60_000_000) & 0x20,
        0x20,
        "and high after 65536 PIT clocks"
    );
    // I/O APIC and HPET identification through memory
    m.mmio_write(IOAPIC, 4, 1, 0);
    assert_eq!(m.mmio_read(IOAPIC + 0x10, 4, 0), 0x0017_0020);
    let id = m.mmio_read(HPET, 8, 0);
    assert_eq!((id >> 16) & 0xFFFF, 0x8086);
    // the local APIC version
    assert_eq!(
        m.mmio_read(LAPIC + u64::from(reg::VERSION), 4, 0) & 0xFF,
        0x14
    );
    // ACPI: enable, read the timer
    m.io_out(SMI_CMD, 1, u32::from(ACPI_ENABLE), 0);
    let t = m.io_in(0x608, 4, NS);
    assert_eq!(t, 3_579_545);
    // PCI: the host bridge answers, the next slot is empty
    m.io_out(PCI_ADDRESS, 4, 0x8000_0000, 0);
    assert_eq!(m.io_in(PCI_DATA, 4, 0), 0x29C0_8086);
    m.io_out(PCI_ADDRESS, 4, 0x8000_0800, 0);
    assert_eq!(m.io_in(PCI_DATA, 4, 0), 0xFFFF_FFFF);
    assert_eq!(
        m.unclaimed_in + m.unclaimed_out + m.unclaimed_mmio,
        0,
        "everything was served"
    );
}

#[test]
fn a_lowest_priority_message_is_delivered_like_a_fixed_one() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 4, 0x34 | (1 << 8), 0);
    out(&mut m, 0x3F9, 1, 0);
    m.uart.push_rx(1);
    assert_eq!(m.acknowledge(0), Some(0x34));
}

#[test]
fn illegal_vectors_from_the_io_apic_are_dropped_by_the_local_apic() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 4, 0x05, 0);
    out(&mut m, 0x3F9, 1, 0);
    m.uart.push_rx(1);
    assert_eq!(m.pending(0), None);
    assert_eq!(
        m.mmio_read(LAPIC + 0x200, 4, 0),
        0,
        "and it never reaches IRR"
    );
}

#[test]
fn the_8259_reaches_the_cpu_through_the_hpet_legacy_timer_too() {
    let mut m = machine();
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, 0x00),
    ] {
        out(&mut m, p, v, 0);
    }
    m.mmio_write(HPET + hpet::REG_TIMER0, 8, 1 << 2, 0);
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 3, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + 8, 8, 100, 0);
    assert_eq!(m.acknowledge(10_000), Some(0x20), "IRQ0 on the 8259");
    out(&mut m, 0x20, 0x20, 10_000);
    m.mmio_write(HPET + hpet::REG_TIMER0 + 8, 8, 1000, 10_000); // a second expiry
    assert_eq!(
        m.acknowledge(110_000),
        Some(0x20),
        "each pulse is a fresh edge on the 8259"
    );
}

// ------------------------------------------------------------- PIT on IRQ0

/// Linux's periodic tick: channel 0, mode 2, LSB then MSB, LATCH = 1193 (HZ = 1000).
/// Its edges come 1194, 2387, 5966, 7159 clocks after the count: ceil(k * 1e9 / 1193182) ns.
fn pit_tick(m: &mut Machine, now: u64) {
    out(m, 0x43, 0x34, now);
    out(m, 0x40, 0xA9, now);
    out(m, 0x40, 0x04, now);
}

fn pic_at_0x20(m: &mut Machine) {
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, 0x00),
    ] {
        out(m, p, v, 0);
    }
}

#[test]
fn pit_channel_0_drives_irq0_on_the_8259() {
    let mut m = machine();
    pic_at_0x20(&mut m);
    pit_tick(&mut m, 0);
    assert_eq!(
        m.acknowledge(0),
        Some(0x20),
        "the control word ended the power-on mode 0's low OUT: an edge"
    );
    out(&mut m, 0x20, 0x20, 0);
    assert_eq!(m.next_event(0), Some(1_000_686));
    assert_eq!(m.pending(1_000_685), None);
    assert_eq!(m.acknowledge(1_000_686), Some(0x20));
    out(&mut m, 0x20, 0x20, 1_000_686);
    assert_eq!(m.next_event(1_000_686), Some(2_000_534));
    assert_eq!(m.pending(2_000_533), None);
    assert_eq!(
        m.acknowledge(2_000_534),
        Some(0x20),
        "every period, a fresh edge"
    );
    assert_eq!(m.pit_coalesced, 0);
    assert_eq!(m.unclaimed_in + m.unclaimed_out, 0);
}

#[test]
fn pit_channel_0_is_io_apic_pin_2() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 2, 0x30, 0);
    route(&mut m, 0, 0x3F, 0);
    pit_tick(&mut m, 0);
    assert_eq!(m.acknowledge(0), Some(0x30), "the control word's edge");
    eoi(&mut m, 0);
    assert_eq!(m.pending(1_000_685), None);
    assert_eq!(m.acknowledge(1_000_686), Some(0x30), "pin 2, not pin 0");
    eoi(&mut m, 1_000_686);
    assert_eq!(m.pending(1_000_687), None, "an edge, once");
    assert_eq!(m.acknowledge(2_000_534), Some(0x30));
}

#[test]
fn a_late_sync_merges_pit_ticks_into_one_irq0_and_counts_the_rest() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 2, 0x30, 0);
    pit_tick(&mut m, 0);
    assert_eq!(m.acknowledge(0), Some(0x30));
    eoi(&mut m, 0);
    // the VMM comes back after five ticks
    assert_eq!(m.acknowledge(5_000_076), Some(0x30));
    assert_eq!(m.pit_coalesced, 4);
    eoi(&mut m, 5_000_076);
    assert_eq!(m.pending(5_000_076), None);
    assert_eq!(m.next_event(5_000_076), Some(5_999_923));
    assert_eq!(m.acknowledge(5_999_923), Some(0x30));
    assert_eq!(m.pit_coalesced, 4, "on time again: nothing merged");
}

#[test]
fn hpet_legacy_replacement_takes_irq0_from_the_pit() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 2, 0x30, 0);
    // legacy replacement on, the counter running, timer 0 one-shot at 5 ms
    m.mmio_write(HPET + hpet::REG_TIMER0, 8, 1 << 2, 0);
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 3, 0);
    m.mmio_write(HPET + hpet::REG_TIMER0 + 8, 8, 50_000, 0);
    pit_tick(&mut m, 0);
    assert_eq!(
        m.next_event(0),
        Some(5_000_000),
        "the HPET's deadline; the PIT's ticks lead nowhere"
    );
    assert_eq!(m.pending(1_000_686), None, "no PIT tick on IRQ0");
    assert_eq!(m.pending(4_999_999), None);
    assert_eq!(m.acknowledge(5_000_000), Some(0x30), "the HPET's");
    eoi(&mut m, 5_000_000);
    assert_eq!(m.pit_coalesced, 0, "dropped ticks are not merged ones");
    // legacy replacement off: IRQ0 is the PIT's again
    m.mmio_write(HPET + hpet::REG_CONFIG, 8, 1, 5_000_001);
    assert_eq!(
        m.pending(5_000_001),
        None,
        "the ticks it missed stay missed"
    );
    assert_eq!(m.next_event(5_000_001), Some(5_000_076));
    assert_eq!(m.acknowledge(5_000_076), Some(0x30));
    assert_eq!(m.pit_coalesced, 0);
}

#[test]
fn an_edge_hpet_timer_does_not_touch_a_pin_another_device_uses() {
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, 4, 0x34, 0);
    out(&mut m, 0x3F9, 1, 0);
    // timer 2 is an edge timer whose route field names pin 4
    m.mmio_write(
        HPET + hpet::REG_TIMER0 + 2 * hpet::TIMER_STRIDE,
        8,
        4 << 9,
        0,
    );
    m.uart.push_rx(1);
    assert_eq!(
        m.acknowledge(0),
        Some(0x34),
        "the UART's line was not pulled down by the HPET"
    );
    eoi(&mut m, 0);
    assert_eq!(
        m.pending(1),
        None,
        "the still-high line is not a new edge each time the bus syncs"
    );
}

#[test]
fn a_fixed_mode_lint0_does_not_pass_the_8259() {
    let mut m = machine();
    for (p, v) in [
        (0x20, 0x11),
        (0x21, 0x20),
        (0x21, 0x04),
        (0x21, 0x01),
        (0x21, 0x00),
    ] {
        out(&mut m, p, v, 0);
    }
    enable_lapic(&mut m);
    m.mmio_write(LAPIC + u64::from(reg::LVT_LINT0), 4, 0x30, 0); // unmasked, fixed vector 0x30
    out(&mut m, 0x3F9, 1, 0);
    m.uart.push_rx(1);
    assert_eq!(
        m.pending(0),
        None,
        "only an ExtINT-mode LINT0 is the virtual wire"
    );
}

#[test]
fn the_power_management_timer_event_is_a_deadline() {
    let mut m = machine();
    m.io_out(PM1_ENABLE, 2, 1, 0); // TMR_EN
    let t = m.next_event(0).unwrap();
    assert_eq!(
        t,
        (2 * 0x80_0000u64 * NS).div_ceil(2 * 3_579_545),
        "the first time bit 23 toggles"
    );
}

#[test]
fn local_apic_registers_are_only_reachable_aligned_and_32_bit() {
    let mut m = machine();
    m.mmio_read(LAPIC + 0x31, 4, 0);
    m.mmio_write(LAPIC + 0x81, 4, 1, 0);
    m.mmio_read(LAPIC + 0x30, 8, 0);
    m.mmio_write(LAPIC + 0x80, 2, 1, 0);
    assert_eq!(m.unclaimed_mmio, 4);
    m.mmio_read(LAPIC + 0x30, 4, 0);
    m.mmio_write(LAPIC + 0x80, 4, 1, 0);
    assert_eq!(m.unclaimed_mmio, 4);
}
