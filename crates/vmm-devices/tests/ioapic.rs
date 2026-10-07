//! The I/O APIC as Linux programs it.

use vmm_devices::ioapic::*;

fn reg_w(io: &mut IoApic, idx: u8, v: u32) {
    assert!(io.write(OFF_INDEX, u32::from(idx)));
    assert!(io.write(OFF_WINDOW, v));
}

fn reg_r(io: &mut IoApic, idx: u8) -> u32 {
    io.write(OFF_INDEX, u32::from(idx));
    io.read(OFF_WINDOW).unwrap()
}

/// Programs the redirection entry of `pin`.
fn route(io: &mut IoApic, pin: u8, low: u32, dest: u8) {
    reg_w(io, REG_REDIRECTION + 2 * pin + 1, u32::from(dest) << 24);
    reg_w(io, REG_REDIRECTION + 2 * pin, low);
}

const LEVEL: u32 = 1 << 15;
const MASK: u32 = 1 << 16;
const LOW: u32 = 1 << 13;

fn msg(v: u8, pin: u8, level: bool) -> Message {
    Message {
        vector: v,
        mode: 0,
        logical: false,
        dest: 0,
        level,
        pin,
    }
}

#[test]
fn only_its_three_registers() {
    let mut io = IoApic::new();
    for o in [0x04, 0x08, 0x14, 0x20, 0x44, 0x100] {
        assert!(
            !IoApic::owns(o) && io.read(o).is_none() && !io.write(o, 0),
            "{o:#x}"
        );
    }
    for o in [OFF_INDEX, OFF_WINDOW, OFF_EOI] {
        assert!(IoApic::owns(o) && io.read(o).is_some());
    }
}

#[test]
fn id_and_version_as_linux_probes_them() {
    let mut io = IoApic::new();
    assert_eq!(
        reg_r(&mut io, REG_VERSION),
        0x0017_0020,
        "version 0x20, 24 entries"
    );
    assert_eq!(reg_r(&mut io, REG_ID), 0);
    reg_w(&mut io, REG_ID, 0x0300_0000);
    assert_eq!(reg_r(&mut io, REG_ID), 0x0300_0000);
    reg_w(&mut io, REG_ID, 0xFFFF_FFFF);
    assert_eq!(reg_r(&mut io, REG_ID), 0x0F00_0000, "only bits 27:24");
    assert_eq!(reg_r(&mut io, REG_ARBITRATION), 0x0F00_0000);
    reg_w(&mut io, REG_VERSION, 0xFFFF_FFFF);
    assert_eq!(reg_r(&mut io, REG_VERSION), 0x0017_0020, "read-only");
    assert_eq!(io.read(OFF_INDEX), Some(u32::from(REG_VERSION)));
}

#[test]
fn every_entry_starts_masked_edge_physical() {
    let mut io = IoApic::new();
    for pin in 0..PINS as u8 {
        assert_eq!(reg_r(&mut io, REG_REDIRECTION + 2 * pin), MASK, "pin {pin}");
        assert_eq!(reg_r(&mut io, REG_REDIRECTION + 2 * pin + 1), 0);
    }
    assert_eq!(
        reg_r(&mut io, REG_REDIRECTION + 2 * PINS as u8),
        0,
        "no 25th entry"
    );
    reg_w(&mut io, REG_REDIRECTION + 2 * PINS as u8, 0xFFFF_FFFF);
    assert_eq!(reg_r(&mut io, 0x3F), 0);
    assert_eq!(reg_r(&mut io, 0x03), 0, "an unknown register reads zero");
}

#[test]
fn an_edge_interrupt_is_sent_once_per_rising_edge() {
    let mut io = IoApic::new();
    route(&mut io, 4, 0x34, 1);
    io.set_irq(4, true);
    let m = io.take_message().unwrap();
    assert_eq!(
        m,
        Message {
            vector: 0x34,
            mode: 0,
            logical: false,
            dest: 1,
            level: false,
            pin: 4
        }
    );
    assert_eq!(io.take_message(), None);
    io.set_irq(4, true);
    assert_eq!(io.take_message(), None, "the line did not change");
    io.set_irq(4, false);
    assert_eq!(io.take_message(), None, "falling edges send nothing");
    io.set_irq(4, true);
    assert_eq!(io.take_message().map(|m| m.vector), Some(0x34));
}

#[test]
fn a_masked_edge_is_held_and_sent_when_unmasked() {
    let mut io = IoApic::new();
    route(&mut io, 1, 0x31 | MASK, 0);
    io.set_irq(1, true);
    assert_eq!(io.take_message(), None);
    assert_eq!(
        reg_r(&mut io, REG_REDIRECTION + 2) & (1 << 12),
        1 << 12,
        "delivery status: a request is waiting"
    );
    io.set_irq(1, false);
    route(&mut io, 1, 0x31, 0);
    assert_eq!(
        io.take_message().map(|m| m.vector),
        Some(0x31),
        "the pulse was not lost"
    );
    assert_eq!(reg_r(&mut io, REG_REDIRECTION + 2) & (1 << 12), 0);
    route(&mut io, 1, 0x31, 0);
    assert_eq!(io.take_message(), None, "and only once");
}

#[test]
fn polarity_inverts_the_line() {
    let mut io = IoApic::new();
    io.set_irq(9, true); // an idle device drives an active-low line high
    route(&mut io, 9, 0x39 | LOW, 0);
    assert_eq!(
        io.take_message(),
        None,
        "high is idle for an active-low pin"
    );
    io.set_irq(9, false);
    assert_eq!(io.take_message().map(|m| m.vector), Some(0x39));
}

#[test]
fn changing_the_polarity_flips_what_the_line_means() {
    let mut io = IoApic::new();
    route(&mut io, 6, 0x36 | LEVEL | MASK, 0);
    io.set_irq(6, false); // idle for an active-high pin
    route(&mut io, 6, 0x36 | LEVEL | LOW, 0);
    assert_eq!(
        io.take_message().map(|m| m.vector),
        Some(0x36),
        "now low is active: the line is asserted"
    );
}

#[test]
fn a_level_interrupt_is_blocked_by_remote_irr_until_eoi() {
    let mut io = IoApic::new();
    route(&mut io, 11, 0x3B | LEVEL, 0);
    io.set_irq(11, true);
    assert_eq!(io.take_message(), Some(msg(0x3B, 11, true)));
    assert_eq!(
        reg_r(&mut io, REG_REDIRECTION + 22) & (1 << 14),
        1 << 14,
        "remote IRR set"
    );
    assert_eq!(io.take_message(), None);
    io.eoi(0x3B);
    assert_eq!(
        io.take_message(),
        Some(msg(0x3B, 11, true)),
        "still asserted: sent again"
    );
    io.set_irq(11, false);
    io.eoi(0x3B);
    assert_eq!(io.take_message(), None, "released: nothing more");
    assert_eq!(reg_r(&mut io, REG_REDIRECTION + 22) & (1 << 14), 0);
}

#[test]
fn eoi_by_register_or_call_and_only_for_the_vector() {
    let mut io = IoApic::new();
    route(&mut io, 2, 0x40 | LEVEL, 0);
    route(&mut io, 3, 0x41 | LEVEL, 0);
    io.set_irq(2, true);
    io.set_irq(3, true);
    assert_eq!(io.take_message().map(|m| m.pin), Some(2));
    assert_eq!(io.take_message().map(|m| m.pin), Some(3));
    assert!(io.write(OFF_EOI, 0x41));
    assert_eq!(
        io.take_message().map(|m| m.pin),
        Some(3),
        "only pin 3 had that vector"
    );
    assert_eq!(io.take_message(), None);
    assert_eq!(io.eois, 1);
    io.eoi(0x40);
    assert_eq!(io.take_message().map(|m| m.pin), Some(2));
    assert_eq!(io.read(OFF_EOI), Some(0));
}

#[test]
fn a_level_line_that_drops_before_eoi_is_not_sent_again() {
    let mut io = IoApic::new();
    route(&mut io, 5, 0x35 | LEVEL, 0);
    io.set_irq(5, true);
    io.take_message();
    io.set_irq(5, false);
    io.eoi(0x35);
    assert_eq!(io.take_message(), None);
}

#[test]
fn a_masked_level_line_waits_and_sends_on_unmask() {
    let mut io = IoApic::new();
    route(&mut io, 7, 0x37 | LEVEL | MASK, 0);
    io.set_irq(7, true);
    assert_eq!(io.take_message(), None);
    assert_eq!(reg_r(&mut io, REG_REDIRECTION + 14) & (1 << 12), 1 << 12);
    route(&mut io, 7, 0x37 | LEVEL, 0);
    assert_eq!(io.take_message().map(|m| m.vector), Some(0x37));
    assert_eq!(reg_r(&mut io, REG_REDIRECTION + 14) & (1 << 12), 0);
}

#[test]
fn switching_a_level_entry_to_edge_clears_remote_irr() {
    let mut io = IoApic::new();
    route(&mut io, 8, 0x38 | LEVEL, 0);
    io.set_irq(8, true);
    io.take_message();
    assert_ne!(reg_r(&mut io, REG_REDIRECTION + 16) & (1 << 14), 0);
    route(&mut io, 8, 0x38, 0);
    assert_eq!(reg_r(&mut io, REG_REDIRECTION + 16) & (1 << 14), 0);
    io.set_irq(8, false);
    io.set_irq(8, true);
    assert_eq!(
        io.take_message().map(|m| m.vector),
        Some(0x38),
        "edge delivery works again"
    );
}

#[test]
fn read_only_bits_and_reserved_bits_ignore_writes() {
    let mut io = IoApic::new();
    route(&mut io, 0, 0x30 | LEVEL, 0);
    io.set_irq(0, true);
    io.take_message();
    // try to clear remote IRR and set delivery status
    reg_w(&mut io, REG_REDIRECTION, 0x30 | LEVEL | (1 << 12));
    let v = reg_r(&mut io, REG_REDIRECTION);
    assert_eq!(
        v & (1 << 14),
        1 << 14,
        "remote IRR cannot be cleared by writing"
    );
    assert_eq!(v & (1 << 12), 0, "delivery status cannot be set by writing");
    reg_w(&mut io, REG_REDIRECTION + 1, 0xFFFF_FFFF);
    assert_eq!(
        reg_r(&mut io, REG_REDIRECTION + 1),
        0xFF00_0001 & 0xFF00_0000 | (reg_r(&mut io, REG_REDIRECTION + 1) & 0x0001_FFFF)
    );
    assert_eq!(
        reg_r(&mut io, REG_REDIRECTION + 1) & 0x00FF_FFFF,
        0,
        "bits 55:32 are reserved"
    );
}

#[test]
fn delivery_modes_destination_modes_and_destinations_travel_in_the_message() {
    let mut io = IoApic::new();
    route(&mut io, 12, 0x3C | (1 << 8) | (1 << 11), 0xF0);
    io.set_irq(12, true);
    assert_eq!(
        io.take_message(),
        Some(Message {
            vector: 0x3C,
            mode: 1,
            logical: true,
            dest: 0xF0,
            level: false,
            pin: 12
        })
    );
    route(&mut io, 13, 4 << 8, 3); // NMI, vector ignored
    io.set_irq(13, true);
    assert_eq!(io.take_message().map(|m| (m.mode, m.dest)), Some((4, 3)));
}

#[test]
fn out_of_range_pins_are_ignored() {
    let mut io = IoApic::new();
    io.set_irq(24, true);
    io.set_irq(255, true);
    assert_eq!(io.take_message(), None);
}

#[test]
fn messages_come_out_in_order_and_overflow_is_counted() {
    let mut io = IoApic::new();
    for pin in 0..8u8 {
        route(&mut io, pin, 0x50 + pin as u32, 0);
    }
    for pin in [5u8, 2, 7] {
        io.set_irq(pin, true);
    }
    let got: Vec<u8> = std::iter::from_fn(|| io.take_message())
        .map(|m| m.pin)
        .collect();
    assert_eq!(got, [5, 2, 7]);
    // flood: 100 level interrupts on a pin cannot queue more than the buffer holds
    for round in 0..40 {
        route(&mut io, 10, 0x60 | LEVEL, 0);
        io.set_irq(10, true);
        io.set_irq(10, false);
        route(&mut io, 10, 0x60, 0);
        let _ = round;
        io.set_irq(10, true);
        io.set_irq(10, false);
    }
    assert!(io.dropped > 0);
}

#[test]
fn isa_irq_routing_as_linux_sets_it_up() {
    // pins 0..15 edge active-high ISA, PCI pins 16..23 level active-low
    let mut io = IoApic::new();
    for pin in 0..16u8 {
        route(&mut io, pin, 0x20 + u32::from(pin), 0);
    }
    for pin in 16..24u8 {
        io.set_irq(pin, true); // the PCI lines idle high
        route(&mut io, pin, (0x30 + u32::from(pin)) | LEVEL | LOW, 0);
    }
    assert_eq!(io.take_message(), None, "idle is not an interrupt");
    io.set_irq(4, true);
    io.set_irq(20, false);
    let got: Vec<(u8, u8)> = std::iter::from_fn(|| io.take_message())
        .map(|m| (m.pin, m.vector))
        .collect();
    assert_eq!(got, [(4, 0x24), (20, 0x44)]);
}

#[test]
fn a_held_level_line_sends_nothing_more_while_remote_irr_is_set() {
    let mut io = IoApic::new();
    route(&mut io, 11, 0x3B | LEVEL, 0);
    io.set_irq(11, true);
    assert!(io.take_message().is_some());
    io.set_irq(11, true);
    assert_eq!(
        io.take_message(),
        None,
        "the line has not changed and the interrupt is awaiting EOI"
    );
}

#[test]
fn delivery_status_is_clear_for_a_masked_level_pin_that_is_not_asserted() {
    let mut io = IoApic::new();
    route(&mut io, 7, 0x37 | LEVEL | MASK, 0);
    assert_eq!(
        reg_r(&mut io, REG_REDIRECTION + 14) & (1 << 12),
        0,
        "nothing is waiting"
    );
}

#[test]
fn rewriting_an_entry_does_not_lose_or_repeat_what_is_in_service() {
    let mut io = IoApic::new();
    route(&mut io, 0, 0x30 | LEVEL, 0);
    io.set_irq(0, true);
    io.take_message();
    route(&mut io, 0, 0x31 | LEVEL, 0); // a new vector while in service
    assert_eq!(
        io.take_message(),
        None,
        "remote IRR still blocks: nothing is sent again"
    );
    assert_ne!(reg_r(&mut io, REG_REDIRECTION) & (1 << 14), 0);
    io.eoi(0x31);
    assert_eq!(io.take_message().map(|m| m.vector), Some(0x31));
}

#[test]
fn an_edge_pin_whose_polarity_flips_into_active_sends() {
    let mut io = IoApic::new();
    route(&mut io, 5, 0x35 | MASK, 0);
    io.set_irq(5, false);
    route(&mut io, 5, 0x35 | LOW, 0); // low is now the active level, and the line is low
    assert_eq!(
        io.take_message().map(|m| m.vector),
        Some(0x35),
        "becoming active is an edge"
    );
}

#[test]
fn the_id_register_keeps_four_bits() {
    let mut io = IoApic::new();
    reg_w(&mut io, REG_ID, 0xFF00_0000);
    assert_eq!(reg_r(&mut io, REG_ID), 0x0F00_0000);
}
