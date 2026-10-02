//! The HPET as Linux uses it: clocksource, one-shot and periodic comparators.

use vmm_devices::hpet::*;

const TN_LEVEL: u64 = 1 << 1;
const TN_ENABLE: u64 = 1 << 2;
const TN_PERIODIC: u64 = 1 << 3;
const TN_SETVAL: u64 = 1 << 6;
const TN_32BIT: u64 = 1 << 8;

fn r8(h: &mut Hpet, off: u64, now: u64) -> u64 {
    h.read(off, 8, now).unwrap()
}

fn w8(h: &mut Hpet, off: u64, v: u64, now: u64) {
    assert!(h.write(off, 8, v, now));
}

fn cfg(t: u64) -> u64 {
    REG_TIMER0 + TIMER_STRIDE * t
}

fn cmp(t: u64) -> u64 {
    cfg(t) + 8
}

fn start(h: &mut Hpet, now: u64) {
    w8(h, REG_CONFIG, 1, now);
}

fn fires(h: &mut Hpet) -> Vec<Fire> {
    std::iter::from_fn(|| h.take_fire()).collect()
}

#[test]
fn the_id_register_as_q35_reports_it() {
    let mut h = Hpet::new();
    let id = r8(&mut h, REG_ID, 0);
    assert_eq!(id & 0xFF, 1, "revision");
    assert_eq!((id >> 8) & 0x1F, 2, "three timers");
    assert_eq!((id >> 13) & 1, 1, "64-bit counter");
    assert_eq!((id >> 15) & 1, 1, "legacy replacement capable");
    assert_eq!((id >> 16) & 0xFFFF, 0x8086);
    assert_eq!(id >> 32, 100_000_000, "100 ns in femtoseconds");
    assert_eq!(h.read(REG_ID, 4, 0), Some(id & 0xFFFF_FFFF));
    assert_eq!(h.read(REG_ID + 4, 4, 0), Some(id >> 32));
    w8(&mut h, REG_ID, 0, 0);
    assert_eq!(r8(&mut h, REG_ID, 0), id, "read-only");
}

#[test]
fn only_aligned_32_and_64_bit_accesses_inside_the_block() {
    let mut h = Hpet::new();
    assert!(Hpet::owns(0) && Hpet::owns(SIZE - 1) && !Hpet::owns(SIZE));
    assert_eq!(h.read(0, 2, 0), None);
    assert_eq!(h.read(0, 1, 0), None);
    assert_eq!(h.read(SIZE, 4, 0), None);
    assert!(!h.write(0x10, 2, 1, 0));
    assert!(!h.write(SIZE, 8, 1, 0));
}

#[test]
fn the_counter_is_frozen_until_enabled_then_counts_at_10_mhz() {
    let mut h = Hpet::new();
    assert_eq!(r8(&mut h, REG_COUNTER, 5_000), 0);
    start(&mut h, 1_000);
    assert_eq!(r8(&mut h, REG_COUNTER, 1_000), 0);
    assert_eq!(r8(&mut h, REG_COUNTER, 1_099), 0);
    assert_eq!(r8(&mut h, REG_COUNTER, 1_100), 1);
    assert_eq!(r8(&mut h, REG_COUNTER, 1_000 + 1_000_000_000), 10_000_000);
    w8(&mut h, REG_CONFIG, 0, 2_000);
    let frozen = r8(&mut h, REG_COUNTER, 2_000);
    assert_eq!(frozen, 10);
    assert_eq!(r8(&mut h, REG_COUNTER, 9_999_999), frozen, "stopped");
    start(&mut h, 20_000);
    assert_eq!(
        r8(&mut h, REG_COUNTER, 20_100),
        frozen + 1,
        "resumes where it stopped"
    );
}

#[test]
fn the_counter_can_be_set_only_while_stopped() {
    let mut h = Hpet::new();
    w8(&mut h, REG_COUNTER, 1000, 0);
    assert_eq!(r8(&mut h, REG_COUNTER, 0), 1000);
    start(&mut h, 0);
    w8(&mut h, REG_COUNTER, 5, 100);
    assert_eq!(r8(&mut h, REG_COUNTER, 100), 1001, "ignored while running");
}

#[test]
fn counter_reads_in_halves_agree_with_the_whole() {
    let mut h = Hpet::new();
    w8(&mut h, REG_COUNTER, 0x1_2345_6789, 0);
    assert_eq!(h.read(REG_COUNTER, 4, 0), Some(0x2345_6789));
    assert_eq!(h.read(REG_COUNTER + 4, 4, 0), Some(1));
    assert!(h.write(REG_COUNTER, 4, 0xAAAA_BBBB, 0));
    assert!(h.write(REG_COUNTER + 4, 4, 0x7, 0));
    assert_eq!(r8(&mut h, REG_COUNTER, 0), 0x7_AAAA_BBBB);
}

#[test]
fn a_one_shot_edge_timer_fires_once_when_the_counter_reaches_the_comparator() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | (2 << 9), 0); // IRQ 2
    start(&mut h, 0);
    w8(&mut h, cmp(0), 1000, 0);
    assert_eq!(h.next_event(0), Some(100_000), "1000 ticks of 100 ns");
    h.sync(99_999);
    assert_eq!(h.take_fire(), None);
    h.sync(100_000);
    assert_eq!(fires(&mut h), [Fire { timer: 0, irq: 2 }]);
    h.sync(10_000_000);
    assert_eq!(h.take_fire(), None, "a one-shot does not repeat");
    assert_eq!(
        h.next_event(10_000_000),
        None,
        "the comparator is behind the counter now: no deadline"
    );
}

#[test]
fn a_disabled_timer_does_not_interrupt_but_still_counts_the_match() {
    let mut h = Hpet::new();
    start(&mut h, 0);
    w8(&mut h, cmp(1), 10, 0);
    h.sync(1_000_000);
    assert_eq!(h.take_fire(), None);
    assert_eq!(h.next_event(0), None, "no enabled comparator, no deadline");
}

#[test]
fn a_periodic_timer_follows_the_setval_protocol_and_repeats() {
    let mut h = Hpet::new();
    w8(
        &mut h,
        cfg(0),
        TN_ENABLE | TN_PERIODIC | TN_SETVAL | (2 << 9),
        0,
    );
    start(&mut h, 0);
    w8(&mut h, cmp(0), 500, 0); // first expiry at 500
    w8(&mut h, cmp(0), 1000, 0); // then every 1000
    assert_eq!(
        r8(&mut h, cfg(0), 0) & TN_SETVAL,
        0,
        "VAL_SET clears itself"
    );
    h.sync(500 * 100);
    assert_eq!(fires(&mut h).len(), 1);
    h.sync(1500 * 100);
    assert_eq!(fires(&mut h).len(), 1);
    h.sync(2500 * 100);
    assert_eq!(fires(&mut h).len(), 1);
    assert_eq!(h.next_event(2500 * 100), Some(3500 * 100));
}

#[test]
fn missed_periods_are_all_counted_up_to_the_queue_size() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | TN_PERIODIC | TN_SETVAL, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 100, 0);
    w8(&mut h, cmp(0), 100, 0);
    h.sync(5 * 100 * 100); // five periods at once
    assert_eq!(fires(&mut h).len(), 5);
    h.sync(100 * 100 * 100 * 100); // many more
    assert_eq!(fires(&mut h).len(), 16, "the queue holds sixteen");
    assert!(
        h.dropped == 0,
        "and the excess in one burst was never queued, only capped"
    );
}

#[test]
fn a_level_timer_sets_its_status_bit_and_holds_the_line_until_cleared() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(2), TN_ENABLE | TN_LEVEL | (11 << 9), 0);
    start(&mut h, 0);
    w8(&mut h, cmp(2), 50, 0);
    assert!(!h.line(2, 4_999));
    assert!(h.line(2, 5_000));
    assert_eq!(r8(&mut h, REG_STATUS, 5_000), 1 << 2);
    assert_eq!(
        h.take_fire(),
        None,
        "level interrupts are lines, not pulses"
    );
    assert!(h.line(2, 9_000), "stays asserted");
    w8(&mut h, REG_STATUS, 1 << 2, 9_000); // write 1 to clear
    assert!(!h.line(2, 9_000));
    assert_eq!(r8(&mut h, REG_STATUS, 9_000), 0);
    // writing 1 to a bit of another timer or 0 anywhere changes nothing
    w8(&mut h, cmp(2), 200, 9_000);
    h.sync(30_000);
    w8(&mut h, REG_STATUS, 0b011, 30_000);
    assert!(h.line(2, 30_000));
    w8(&mut h, REG_STATUS, 0, 30_000);
    assert!(h.line(2, 30_000));
}

#[test]
fn a_masked_level_timer_has_status_but_no_line() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_LEVEL, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 10, 0);
    h.sync(10_000);
    assert!(!h.line(0, 10_000), "INT_ENB is off");
    assert_eq!(
        r8(&mut h, REG_STATUS, 10_000),
        0,
        "and the status bit is not set either"
    );
}

#[test]
fn legacy_replacement_routes_timers_0_and_1_to_irq0_and_irq8() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | (11 << 9), 0);
    w8(&mut h, cfg(1), TN_ENABLE | (11 << 9), 0);
    w8(&mut h, cfg(2), TN_ENABLE | (11 << 9), 0);
    w8(&mut h, REG_CONFIG, 3, 0);
    for t in 0..3 {
        w8(&mut h, cmp(t), 10, 0);
    }
    h.sync(1_000);
    assert_eq!(
        fires(&mut h),
        [
            Fire { timer: 0, irq: 0 },
            Fire { timer: 1, irq: 8 },
            Fire { timer: 2, irq: 11 }
        ]
    );
}

#[test]
fn a_32_bit_timer_compares_modulo_2_to_the_32() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | TN_32BIT, 0);
    w8(&mut h, REG_COUNTER, 0xFFFF_FF00, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 0x0000_0100, 0); // 0x200 ticks ahead across the 32-bit wrap
    assert_eq!(h.next_event(0), Some(0x200 * 100));
    h.sync(0x1FF * 100);
    assert_eq!(h.take_fire(), None);
    h.sync(0x200 * 100);
    assert_eq!(fires(&mut h).len(), 1);
}

#[test]
fn a_comparator_already_passed_does_not_fire_until_the_counter_wraps() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE, 0);
    start(&mut h, 0);
    h.sync(100_000); // counter 1000
    w8(&mut h, cmp(0), 500, 100_000);
    h.sync(10_000_000_000);
    assert_eq!(
        h.take_fire(),
        None,
        "the 64-bit counter will not get there in this life"
    );
}

#[test]
fn timer_configuration_has_read_only_capability_bits() {
    let mut h = Hpet::new();
    let c = r8(&mut h, cfg(0), 0);
    assert_eq!(c & 0x30, 0x30, "periodic capable and 64-bit capable");
    assert_eq!(c >> 32, 0x00F0_0904, "interrupt route capabilities");
    w8(&mut h, cfg(0), 0, 0);
    assert_eq!(r8(&mut h, cfg(0), 0) & 0x30, 0x30);
    w8(&mut h, cfg(0), u64::MAX, 0);
    let c = r8(&mut h, cfg(0), 0);
    assert_eq!(c >> 32, 0x00F0_0904, "capability half is read-only");
    assert_eq!(c & 0x30, 0x30);
    assert_eq!((c >> 9) & 0x1F, 0x1F, "the route field is writable");
    assert_eq!(c & (1 << 7), 0, "reserved bits stay zero");
    assert_eq!(c & (1 << 15), 0);
}

#[test]
fn fsb_registers_are_plain_storage_and_unknown_offsets_read_zero() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(1) + 16, 0xFEE0_1234_0000_0042, 0);
    assert_eq!(r8(&mut h, cfg(1) + 16, 0), 0xFEE0_1234_0000_0042);
    assert_eq!(r8(&mut h, 0x300, 0), 0);
    assert_eq!(r8(&mut h, 0x08, 0), 0);
    w8(&mut h, 0x300, 7, 0);
    assert_eq!(r8(&mut h, 0x300, 0), 0);
}

#[test]
fn the_config_register_keeps_only_enable_and_legacy() {
    let mut h = Hpet::new();
    w8(&mut h, REG_CONFIG, u64::MAX, 0);
    assert_eq!(r8(&mut h, REG_CONFIG, 0), 3);
}

#[test]
fn reading_the_counter_is_enough_for_a_clocksource_and_it_never_goes_back() {
    let mut h = Hpet::new();
    start(&mut h, 0);
    let mut last = 0;
    for t in (0..10_000u64).step_by(37) {
        let c = r8(&mut h, REG_COUNTER, t * 1000);
        assert!(c >= last);
        last = c;
    }
}

#[test]
fn stopping_the_counter_stops_the_timers() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 1000, 0);
    w8(&mut h, REG_CONFIG, 0, 50_000);
    h.sync(10_000_000);
    assert_eq!(h.take_fire(), None);
    assert_eq!(h.next_event(10_000_000), None);
    start(&mut h, 10_000_000);
    assert_eq!(
        h.next_event(10_000_000),
        Some(10_000_000 + 500 * 100),
        "500 ticks were left"
    );
}

#[test]
fn switching_a_level_timer_to_edge_forgets_its_status() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | TN_LEVEL, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 10, 0);
    h.sync(10_000);
    assert_eq!(r8(&mut h, REG_STATUS, 10_000), 1);
    w8(&mut h, cfg(0), TN_ENABLE, 10_000);
    assert_eq!(r8(&mut h, REG_STATUS, 10_000), 0);
}

#[test]
fn undrained_interrupts_across_syncs_overflow_the_queue_and_are_counted() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | TN_PERIODIC | TN_SETVAL, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 100, 0);
    w8(&mut h, cmp(0), 100, 0);
    for k in 1..=3u64 {
        h.sync(k * 10 * 100 * 100); // ten periods per sync
    }
    assert_eq!(fires(&mut h).len(), 16);
    assert_eq!(h.dropped, 14, "30 expiries, 16 kept");
}

#[test]
fn a_level_line_drops_when_the_timer_interrupt_is_disabled() {
    let mut h = Hpet::new();
    w8(&mut h, cfg(0), TN_ENABLE | TN_LEVEL, 0);
    start(&mut h, 0);
    w8(&mut h, cmp(0), 10, 0);
    assert!(h.line(0, 10_000));
    w8(&mut h, cfg(0), TN_LEVEL, 10_000);
    assert!(
        !h.line(0, 10_000),
        "status is set but the enable bit is off"
    );
    assert!(!h.line(5, 10_000), "no such timer");
}
