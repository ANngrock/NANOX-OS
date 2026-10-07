//! The 8259A pair as Linux and a PC BIOS program it.

use vmm_devices::pic::*;

/// What Linux's `init_8259A` does: mask all, ICW1..4 to both chips, then the masks.
fn init_linux(p: &mut Pic) {
    p.write(MASTER_DATA, 0xFF);
    p.write(SLAVE_DATA, 0xFF);
    p.write(MASTER_COMMAND, 0x11);
    p.write(MASTER_DATA, 0x20);
    p.write(MASTER_DATA, 0x04);
    p.write(MASTER_DATA, 0x01);
    p.write(SLAVE_COMMAND, 0x11);
    p.write(SLAVE_DATA, 0x28);
    p.write(SLAVE_DATA, 0x02);
    p.write(SLAVE_DATA, 0x01);
    p.write(MASTER_DATA, 0xFB); // everything but the cascade masked
    p.write(SLAVE_DATA, 0xFF);
}

fn unmask(p: &mut Pic, irq: u8) {
    if irq < 8 {
        let m = p.read(MASTER_DATA).unwrap();
        p.write(MASTER_DATA, m & !(1 << irq));
    } else {
        let s = p.read(SLAVE_DATA).unwrap();
        p.write(SLAVE_DATA, s & !(1 << (irq - 8)));
    }
}

fn eoi(p: &mut Pic, irq: u8) {
    if irq >= 8 {
        p.write(SLAVE_COMMAND, 0x20);
    }
    p.write(MASTER_COMMAND, 0x20);
}

#[test]
fn only_its_own_ports() {
    let mut p = Pic::new();
    for port in [0x22, 0x23, 0xA2, 0x4D2, 0x1F, 0x40] {
        assert!(
            !Pic::owns(port) && p.read(port).is_none() && !p.write(port, 0),
            "{port:#x}"
        );
    }
    for port in [0x20, 0x21, 0xA0, 0xA1, 0x4D0, 0x4D1] {
        assert!(Pic::owns(port) && p.read(port).is_some() && p.write(port, 0));
    }
}

#[test]
fn nothing_pending_and_all_masked_after_reset() {
    let mut p = Pic::new();
    assert_eq!(p.read(MASTER_DATA), Some(0xFF));
    p.set_irq(1, true);
    assert!(!p.int_pending(), "masked");
}

#[test]
fn the_linux_sequence_sets_vectors_and_masks() {
    let mut p = Pic::new();
    init_linux(&mut p);
    assert_eq!(p.read(MASTER_DATA), Some(0xFB));
    assert_eq!(p.read(SLAVE_DATA), Some(0xFF));
    assert_eq!(p.unsupported, 0);
    unmask(&mut p, 1);
    p.set_irq(1, true);
    assert!(p.int_pending());
    assert_eq!(p.acknowledge(), 0x21);
    assert!(!p.int_pending());
    eoi(&mut p, 1);
    unmask(&mut p, 12);
    p.set_irq(12, true);
    assert!(p.int_pending(), "through the cascade");
    assert_eq!(p.acknowledge(), 0x2C);
}

#[test]
fn icw1_clears_the_mask_and_pending_requests() {
    let mut p = Pic::new();
    init_linux(&mut p);
    unmask(&mut p, 3);
    p.set_irq(3, true);
    p.write(MASTER_COMMAND, 0x11);
    assert_eq!(
        p.read(MASTER_DATA),
        Some(0),
        "the datasheet: ICW1 clears IMR"
    );
    p.write(MASTER_DATA, 0x08);
    p.write(MASTER_DATA, 0x04);
    p.write(MASTER_DATA, 0x01);
    p.write(MASTER_COMMAND, 0x0A);
    assert_eq!(p.read(MASTER_COMMAND), Some(0), "IRR was cleared");
}

#[test]
fn priority_is_fixed_lowest_number_first() {
    let mut p = Pic::new();
    init_linux(&mut p);
    for i in [5, 3, 6] {
        unmask(&mut p, i);
    }
    p.set_irq(6, true);
    p.set_irq(3, true);
    p.set_irq(5, true);
    assert_eq!(p.acknowledge(), 0x23);
    assert!(!p.int_pending(), "5 and 6 wait: 3 is in service");
    eoi(&mut p, 3);
    assert_eq!(p.acknowledge(), 0x25);
    eoi(&mut p, 5);
    assert_eq!(p.acknowledge(), 0x26);
}

#[test]
fn a_higher_priority_request_preempts_one_in_service() {
    let mut p = Pic::new();
    init_linux(&mut p);
    for i in [1, 4] {
        unmask(&mut p, i);
    }
    p.set_irq(4, true);
    assert_eq!(p.acknowledge(), 0x24);
    p.set_irq(1, true);
    assert!(p.int_pending());
    assert_eq!(p.acknowledge(), 0x21);
    p.write(MASTER_COMMAND, 0x20);
    assert_eq!(
        p.read(MASTER_COMMAND),
        Some(0),
        "OCW3 default is IRR; nothing pending"
    );
    p.write(MASTER_COMMAND, 0x0B);
    assert_eq!(
        p.read(MASTER_COMMAND),
        Some(0x10),
        "the non-specific EOI took 1, 4 is still in service"
    );
}

#[test]
fn specific_eoi_clears_the_named_bit() {
    let mut p = Pic::new();
    init_linux(&mut p);
    for i in [2, 5] {
        unmask(&mut p, i);
    }
    unmask(&mut p, 5);
    p.set_irq(5, true);
    p.acknowledge();
    p.write(MASTER_COMMAND, 0x0B);
    assert_eq!(p.read(MASTER_COMMAND), Some(0x20));
    p.write(MASTER_COMMAND, 0x60 | 5);
    assert_eq!(p.read(MASTER_COMMAND), Some(0));
    p.write(MASTER_COMMAND, 0x60 | 4);
    assert_eq!(
        p.read(MASTER_COMMAND),
        Some(0),
        "an EOI for something not in service changes nothing"
    );
}

#[test]
fn edge_triggered_lines_latch_once_per_rising_edge() {
    let mut p = Pic::new();
    init_linux(&mut p);
    unmask(&mut p, 4);
    p.set_irq(4, true);
    assert_eq!(p.acknowledge(), 0x24);
    eoi(&mut p, 4);
    assert!(
        !p.int_pending(),
        "the line is still high: no new edge, no new request"
    );
    p.set_irq(4, false);
    p.set_irq(4, true);
    assert!(p.int_pending());
    assert_eq!(p.acknowledge(), 0x24);
}

#[test]
fn a_pulse_while_masked_is_remembered_in_irr() {
    let mut p = Pic::new();
    init_linux(&mut p);
    p.set_irq(4, true);
    p.set_irq(4, false);
    p.write(MASTER_COMMAND, 0x0A);
    assert_eq!(p.read(MASTER_COMMAND), Some(0x10));
    unmask(&mut p, 4);
    assert!(p.int_pending());
    assert_eq!(p.acknowledge(), 0x24);
}

#[test]
fn level_triggered_lines_follow_the_line_and_redeliver() {
    let mut p = Pic::new();
    init_linux(&mut p);
    p.write(ELCR_MASTER, 0x20); // IRQ5 level
    unmask(&mut p, 5);
    p.set_irq(5, true);
    assert_eq!(p.acknowledge(), 0x25);
    assert!(!p.int_pending(), "in service");
    eoi(&mut p, 5);
    assert!(p.int_pending(), "still asserted: delivered again");
    p.set_irq(5, false);
    assert!(!p.int_pending(), "dropping the line withdraws the request");
    assert_eq!(p.acknowledge(), 0x27, "so an acknowledge now is spurious");
}

#[test]
fn elcr_keeps_the_fixed_edge_bits_clear() {
    let mut p = Pic::new();
    p.write(ELCR_MASTER, 0xFF);
    p.write(ELCR_SLAVE, 0xFF);
    assert_eq!(
        p.read(ELCR_MASTER),
        Some(0xF8),
        "IRQ0, 1, 2 are always edge"
    );
    assert_eq!(
        p.read(ELCR_SLAVE),
        Some(0xDE),
        "IRQ8 and IRQ13 are always edge"
    );
}

#[test]
fn spurious_interrupts_use_irq7_of_the_chip_with_nothing() {
    let mut p = Pic::new();
    init_linux(&mut p);
    assert_eq!(p.acknowledge(), 0x27);
    assert_eq!(p.spurious, 1);
    // a slave request that vanished between INT and INTA: master 2 taken, slave spurious
    unmask(&mut p, 10);
    p.set_irq(10, true);
    p.write(SLAVE_DATA, 0xFF); // mask it before the acknowledge
    assert_eq!(p.acknowledge(), 0x2F);
    assert_eq!(p.spurious, 2);
}

#[test]
fn the_cascade_blocks_and_releases_with_the_slave_eoi() {
    let mut p = Pic::new();
    init_linux(&mut p);
    for i in [9, 14] {
        unmask(&mut p, i);
    }
    p.set_irq(14, true);
    p.set_irq(9, true);
    assert_eq!(p.acknowledge(), 0x29, "IRQ9 outranks IRQ14");
    assert!(!p.int_pending(), "the slave is serving");
    p.write(SLAVE_COMMAND, 0x20);
    assert!(
        !p.int_pending(),
        "the master still has the cascade in service"
    );
    p.write(MASTER_COMMAND, 0x20);
    assert!(p.int_pending(), "IRQ14 is released");
    assert_eq!(p.acknowledge(), 0x2E);
    eoi(&mut p, 14);
    assert!(!p.int_pending());
}

#[test]
fn a_second_slave_request_while_the_first_waits_is_not_lost() {
    let mut p = Pic::new();
    init_linux(&mut p);
    for i in [8, 15] {
        unmask(&mut p, i);
    }
    p.set_irq(15, true);
    assert_eq!(p.acknowledge(), 0x2F);
    p.set_irq(8, true);
    assert!(
        !p.int_pending(),
        "the slave is serving 15, 8 outranks it but the master is serving the cascade"
    );
    eoi(&mut p, 15);
    assert!(p.int_pending());
    assert_eq!(p.acknowledge(), 0x28);
}

#[test]
fn ocw3_selects_irr_or_isr() {
    let mut p = Pic::new();
    init_linux(&mut p);
    unmask(&mut p, 6);
    p.set_irq(6, true);
    p.write(MASTER_COMMAND, 0x0A);
    assert_eq!(p.read(MASTER_COMMAND), Some(0x40));
    p.acknowledge();
    p.write(MASTER_COMMAND, 0x0B);
    assert_eq!(p.read(MASTER_COMMAND), Some(0x40));
    p.write(MASTER_COMMAND, 0x0A);
    assert_eq!(
        p.read(MASTER_COMMAND),
        Some(0),
        "the request left IRR when it went in service"
    );
    assert_eq!(p.unsupported, 0);
}

#[test]
fn poll_mode_acknowledges_and_reports() {
    let mut p = Pic::new();
    init_linux(&mut p);
    p.write(MASTER_COMMAND, 0x0C);
    assert_eq!(p.read(MASTER_COMMAND), Some(0), "nothing to poll");
    unmask(&mut p, 3);
    p.set_irq(3, true);
    p.write(MASTER_COMMAND, 0x0C);
    assert_eq!(p.read(MASTER_COMMAND), Some(0x83));
    p.write(MASTER_COMMAND, 0x0B);
    assert_eq!(
        p.read(MASTER_COMMAND),
        Some(0x08),
        "the polled request is in service"
    );
    p.write(MASTER_COMMAND, 0x0A);
    assert_eq!(p.read(MASTER_COMMAND), Some(0), "poll was one read only");
}

#[test]
fn automatic_eoi_never_blocks() {
    let mut p = Pic::new();
    p.write(MASTER_COMMAND, 0x11);
    p.write(MASTER_DATA, 0x20);
    p.write(MASTER_DATA, 0x04);
    p.write(MASTER_DATA, 0x03); // 8086 + AEOI
    p.write(MASTER_DATA, 0x00);
    p.set_irq(3, true);
    assert_eq!(p.acknowledge(), 0x23);
    p.write(MASTER_COMMAND, 0x0B);
    assert_eq!(p.read(MASTER_COMMAND), Some(0), "never in service");
    p.set_irq(3, false);
    p.set_irq(1, true);
    p.set_irq(3, true);
    assert_eq!(p.acknowledge(), 0x21);
    assert!(p.int_pending(), "3 is not held back by 1");
}

#[test]
fn what_is_not_modeled_is_counted() {
    let mut p = Pic::new();
    p.write(MASTER_COMMAND, 0xA0); // rotate on non-specific EOI
    p.write(MASTER_COMMAND, 0x68); // special mask mode
    p.write(MASTER_COMMAND, 0x13); // single mode
    p.write(MASTER_DATA, 0x20);
    p.write(MASTER_DATA, 0x00); // ICW3 (a real single-mode chip would skip it)
    p.write(MASTER_DATA, 0x00); // ICW4 follows (ICW1 bit 0): not 8086
    assert_eq!(p.unsupported, 4);
}

#[test]
fn the_vector_base_ignores_the_low_three_bits() {
    let mut p = Pic::new();
    p.write(MASTER_COMMAND, 0x11);
    p.write(MASTER_DATA, 0x27);
    p.write(MASTER_DATA, 0x04);
    p.write(MASTER_DATA, 0x01);
    p.write(MASTER_DATA, 0x00);
    p.set_irq(0, true);
    assert_eq!(p.acknowledge(), 0x20);
}

#[test]
fn switching_a_high_line_to_level_mode_requests_again() {
    let mut p = Pic::new();
    init_linux(&mut p);
    unmask(&mut p, 4);
    p.set_irq(4, true);
    assert_eq!(p.acknowledge(), 0x24);
    eoi(&mut p, 4);
    assert!(!p.int_pending(), "edge mode: one request per edge");
    p.write(ELCR_MASTER, 0x10);
    p.set_irq(4, true);
    assert!(
        p.int_pending(),
        "level mode: the asserted line is a request"
    );
}

#[test]
fn irq2_without_a_slave_is_an_ordinary_line() {
    let mut p = Pic::new();
    p.write(MASTER_COMMAND, 0x13);
    p.write(MASTER_DATA, 0x20);
    p.write(MASTER_DATA, 0x00); // no slave on IRQ2
    p.write(MASTER_DATA, 0x01);
    p.write(MASTER_DATA, 0x00);
    p.set_irq(2, true);
    assert_eq!(p.acknowledge(), 0x22);
}

#[test]
fn level_sense_in_icw1_is_counted() {
    let mut p = Pic::new();
    p.write(MASTER_COMMAND, 0x19); // LTIM set
    assert_eq!(p.unsupported, 1);
    p.write(MASTER_COMMAND, 0x11);
    assert_eq!(p.unsupported, 1);
}

#[test]
fn without_icw4_the_next_data_write_is_the_mask() {
    let mut p = Pic::new();
    p.write(MASTER_COMMAND, 0x10); // ICW1: no ICW4
    p.write(MASTER_DATA, 0x30);
    p.write(MASTER_DATA, 0x04);
    p.write(MASTER_DATA, 0xAA);
    assert_eq!(
        p.read(MASTER_DATA),
        Some(0xAA),
        "the write after ICW3 is OCW1"
    );
    p.set_irq(0, true);
    assert!(p.int_pending());
    assert_eq!(p.acknowledge(), 0x30);
}
