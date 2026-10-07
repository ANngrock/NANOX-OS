//! The RTC/CMOS as a kernel uses it.

use vmm_devices::rtc::*;

const NS: u64 = 1_000_000_000;
/// 2026-10-02 12:34:56 UTC.
const T0: i64 = 1_790_944_496;

fn rd(r: &mut Rtc, reg: u8, now: u64) -> u8 {
    r.write(PORT_INDEX, reg, now);
    r.read(PORT_DATA, now).unwrap()
}

fn wr(r: &mut Rtc, reg: u8, v: u8, now: u64) {
    r.write(PORT_INDEX, reg, now);
    r.write(PORT_DATA, v, now);
}

#[test]
fn civil_conversions_round_trip_over_four_centuries() {
    for secs in (-2_000_000_000i64..4_000_000_000).step_by(86_400 * 7 + 3601) {
        assert_eq!(DateTime::from_unix(secs).to_unix(), secs, "{secs}");
    }
    let d = DateTime::from_unix(T0);
    assert_eq!(
        (d.year, d.month, d.day, d.hour, d.minute, d.second),
        (2026, 10, 2, 12, 34, 56)
    );
    assert_eq!(
        DateTime::from_unix(0).weekday(),
        5,
        "1970-01-01 was a Thursday (Sunday = 1)"
    );
    assert_eq!(d.weekday(), 6, "2026-10-02 is a Friday");
    let leap = DateTime::from_unix(
        DateTime {
            year: 2000,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
        }
        .to_unix(),
    );
    assert_eq!((leap.month, leap.day), (2, 29));
    let y2100 = DateTime {
        year: 2100,
        month: 3,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    }
    .to_unix()
        - DateTime {
            year: 2100,
            month: 2,
            day: 28,
            hour: 0,
            minute: 0,
            second: 0,
        }
        .to_unix();
    assert_eq!(y2100, 86_400, "2100 is not a leap year");
}

#[test]
fn the_time_registers_in_bcd_24h() {
    let mut r = Rtc::new(T0);
    assert_eq!(rd(&mut r, REG_SECONDS, 0), 0x56);
    assert_eq!(rd(&mut r, REG_MINUTES, 0), 0x34);
    assert_eq!(rd(&mut r, REG_HOURS, 0), 0x12);
    assert_eq!(rd(&mut r, REG_WEEKDAY, 0), 6);
    assert_eq!(rd(&mut r, REG_DAY, 0), 0x02);
    assert_eq!(rd(&mut r, REG_MONTH, 0), 0x10);
    assert_eq!(rd(&mut r, REG_YEAR, 0), 0x26);
    assert_eq!(rd(&mut r, REG_CENTURY, 0), 0x20);
}

#[test]
fn time_advances_with_virtual_time_and_rolls_over() {
    let mut r = Rtc::new(T0);
    assert_eq!(rd(&mut r, REG_SECONDS, 3 * NS + 5), 0x59);
    assert_eq!(rd(&mut r, REG_MINUTES, 3 * NS + 5), 0x34);
    assert_eq!(rd(&mut r, REG_SECONDS, 4 * NS), 0x00);
    assert_eq!(rd(&mut r, REG_MINUTES, 4 * NS), 0x35);
    // across midnight and a month
    let mut r = Rtc::new(
        DateTime {
            year: 2026,
            month: 10,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
        }
        .to_unix(),
    );
    assert_eq!(rd(&mut r, REG_DAY, NS), 0x01);
    assert_eq!(rd(&mut r, REG_MONTH, NS), 0x11);
    assert_eq!(rd(&mut r, REG_HOURS, NS), 0x00);
}

#[test]
fn binary_mode_and_the_12_hour_clock() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x06, 0); // binary, 24h
    assert_eq!(rd(&mut r, REG_SECONDS, 0), 56);
    assert_eq!(rd(&mut r, REG_YEAR, 0), 26);
    assert_eq!(rd(&mut r, REG_CENTURY, 0), 20);
    wr(&mut r, REG_B, 0x04, 0); // binary, 12h
    assert_eq!(
        rd(&mut r, REG_HOURS, 0),
        12 | 0x80,
        "noon is 12 PM: the PM flag is bit 7 of the hour"
    );
    let mut r = Rtc::new(
        DateTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 15,
            minute: 0,
            second: 0,
        }
        .to_unix(),
    );
    wr(&mut r, REG_B, 0x00, 0); // BCD, 12h
    assert_eq!(rd(&mut r, REG_HOURS, 0), 0x03 | 0x80);
    let mut r = Rtc::new(
        DateTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
        .to_unix(),
    );
    wr(&mut r, REG_B, 0x00, 0);
    assert_eq!(rd(&mut r, REG_HOURS, 0), 0x12, "midnight is 12 AM");
}

#[test]
fn setting_the_clock_with_set_freezes_and_restarts_it() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x82, 10 * NS); // SET
    assert_eq!(rd(&mut r, REG_SECONDS, 10 * NS), 0x06, "frozen at 12:35:06");
    assert_eq!(rd(&mut r, REG_SECONDS, 99 * NS), 0x06);
    wr(&mut r, REG_HOURS, 0x08, 99 * NS);
    wr(&mut r, REG_MINUTES, 0x30, 99 * NS);
    wr(&mut r, REG_SECONDS, 0x00, 99 * NS);
    wr(&mut r, REG_DAY, 0x15, 99 * NS);
    wr(&mut r, REG_MONTH, 0x03, 99 * NS);
    wr(&mut r, REG_YEAR, 0x31, 99 * NS);
    wr(&mut r, REG_CENTURY, 0x21, 99 * NS);
    assert_eq!(rd(&mut r, REG_YEAR, 150 * NS), 0x31, "still frozen");
    wr(&mut r, REG_B, 0x02, 150 * NS); // clear SET at t = 150 s
    assert_eq!(rd(&mut r, REG_SECONDS, 150 * NS), 0x00);
    assert_eq!(
        rd(&mut r, REG_SECONDS, 152 * NS),
        0x02,
        "and running from then"
    );
    assert_eq!(rd(&mut r, REG_HOURS, 152 * NS), 0x08);
    assert_eq!(rd(&mut r, REG_DAY, 152 * NS), 0x15);
    assert_eq!(rd(&mut r, REG_MONTH, 152 * NS), 0x03);
    assert_eq!(rd(&mut r, REG_CENTURY, 152 * NS), 0x21);
    assert_eq!(r.datetime(152 * NS).year, 2131);
    assert_eq!(
        r.datetime(152 * NS).weekday(),
        DateTime {
            year: 2131,
            month: 3,
            day: 15,
            hour: 0,
            minute: 0,
            second: 0
        }
        .weekday()
    );
}

#[test]
fn writing_a_time_register_without_set_changes_the_time_at_once() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_MINUTES, 0x10, 2 * NS);
    assert_eq!(rd(&mut r, REG_MINUTES, 2 * NS), 0x10);
    assert_eq!(
        rd(&mut r, REG_SECONDS, 3 * NS),
        0x59,
        "seconds kept counting from the write (58 at 2 s)"
    );
    wr(&mut r, REG_WEEKDAY, 7, 3 * NS);
    assert_eq!(
        rd(&mut r, REG_WEEKDAY, 3 * NS),
        6,
        "the weekday is derived and cannot be set"
    );
}

#[test]
fn out_of_range_values_are_clamped_not_stored() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_SECONDS, 0x99, 0);
    assert_eq!(rd(&mut r, REG_SECONDS, 0), 0x59);
    wr(&mut r, REG_MONTH, 0x00, 0);
    assert_eq!(rd(&mut r, REG_MONTH, 0), 0x01);
    wr(&mut r, REG_DAY, 0x00, 0);
    assert_eq!(rd(&mut r, REG_DAY, 0), 0x01);
}

#[test]
fn status_registers_reset_values_and_read_only_bits() {
    let mut r = Rtc::new(T0);
    assert_eq!(rd(&mut r, REG_A, 0) & 0x7F, 0x26);
    assert_eq!(rd(&mut r, REG_B, 0), 0x02);
    assert_eq!(rd(&mut r, REG_C, 0), 0);
    assert_eq!(rd(&mut r, REG_D, 0), 0x80, "the battery is good");
    wr(&mut r, REG_D, 0, 0);
    wr(&mut r, REG_C, 0xFF, 0);
    assert_eq!(rd(&mut r, REG_D, 0), 0x80);
    assert_eq!(rd(&mut r, REG_C, 0), 0);
    assert_eq!(
        r.read(PORT_INDEX, 0),
        Some(0xFF),
        "the index port is write-only"
    );
}

#[test]
fn uip_is_high_in_the_last_244_microseconds_before_a_second() {
    let mut r = Rtc::new(T0);
    assert_eq!(rd(&mut r, REG_A, 500_000_000) & 0x80, 0);
    assert_eq!(rd(&mut r, REG_A, NS - 244_000) & 0x80, 0x80);
    assert_eq!(rd(&mut r, REG_A, NS - 244_001) & 0x80, 0);
    assert_eq!(rd(&mut r, REG_A, NS) & 0x80, 0, "the update finished");
    wr(&mut r, REG_B, 0x82, 0);
    assert_eq!(rd(&mut r, REG_A, NS - 100) & 0x80, 0, "no update while SET");
}

#[test]
fn the_periodic_interrupt_at_1024_hz() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x42, 0); // PIE
    assert!(!r.irq(0));
    let period = NS / 1024;
    assert!(!r.irq(period - 1));
    assert!(r.irq(period + 1));
    assert_eq!(
        r.next_event(period + 1),
        Some(2 * period + 1),
        "the next tick (rounded up to a nanosecond)"
    );
    assert_eq!(
        rd(&mut r, REG_C, period + 1),
        0xC0,
        "IRQF and PF, and reading clears them"
    );
    assert!(!r.irq(period + 2));
    assert_eq!(rd(&mut r, REG_C, period + 2), 0);
    // many periods missed: one flag, not a count
    assert!(r.irq(50 * period));
    assert_eq!(rd(&mut r, REG_C, 50 * period), 0xC0);
}

#[test]
fn the_periodic_flag_is_raised_even_when_the_interrupt_is_disabled() {
    let mut r = Rtc::new(T0);
    let period = NS / 1024;
    assert!(!r.irq(2 * period), "no PIE, no IRQ");
    assert_eq!(rd(&mut r, REG_C, 2 * period), 0x40, "PF alone, IRQF clear");
    assert_eq!(r.next_event(0), None, "nothing enabled: no deadline");
}

#[test]
fn periodic_rates() {
    for (rate, hz) in [(1u8, 256u64), (2, 128), (3, 8192), (6, 1024), (15, 2)] {
        let mut r = Rtc::new(T0);
        wr(&mut r, REG_A, 0x20 | rate, 0);
        wr(&mut r, REG_B, 0x42, 0);
        let period = NS / hz;
        assert!(!r.irq(period - 2), "rate {rate}");
        assert!(r.irq(period + 1), "rate {rate}");
    }
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_A, 0x20, 0); // rate 0: off
    wr(&mut r, REG_B, 0x42, 0);
    assert!(!r.irq(10 * NS));
    wr(&mut r, REG_A, 0x06, NS); // oscillator off
    assert!(!r.irq(20 * NS));
    assert_eq!(r.unsupported, 1);
}

#[test]
fn update_ended_interrupt_once_per_second() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_A, 0x20, 0); // periodic rate off, so only the second counts
    wr(&mut r, REG_B, 0x12, 0); // UIE
    assert!(!r.irq(NS - 1));
    assert!(r.irq(NS));
    assert_eq!(r.next_event(NS), Some(2 * NS));
    assert_eq!(rd(&mut r, REG_C, NS), 0x90);
    assert!(!r.irq(NS + 1));
    assert!(r.irq(3 * NS + 5));
}

#[test]
fn no_update_interrupt_while_the_clock_is_frozen() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x92, 0);
    assert!(!r.irq(5 * NS));
    assert_eq!(r.next_event(5 * NS), None);
}

#[test]
fn the_alarm_fires_when_the_time_matches() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_SECONDS_ALARM, 0x58, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0x34, 0);
    wr(&mut r, REG_HOURS_ALARM, 0x12, 0);
    wr(&mut r, REG_A, 0x20, 0); // periodic rate off
    wr(&mut r, REG_B, 0x22, 0); // AIE
    assert!(!r.irq(NS), "12:34:57");
    assert!(r.irq(2 * NS), "12:34:58");
    assert_eq!(
        rd(&mut r, REG_C, 2 * NS),
        0xB0,
        "IRQF, AF and UF (UF is raised every second whether enabled or not)"
    );
    assert!(!r.irq(3 * NS));
}

#[test]
fn alarm_dont_care_bytes_match_anything() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_SECONDS_ALARM, 0xC0, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0xFF, 0);
    wr(&mut r, REG_HOURS_ALARM, 0xC5, 0);
    wr(&mut r, REG_B, 0x22, 0);
    assert!(r.irq(NS), "every second matches");
    // a fixed second only
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_SECONDS_ALARM, 0x00, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0xC0, 0);
    wr(&mut r, REG_HOURS_ALARM, 0xC0, 0);
    wr(&mut r, REG_B, 0x22, 0);
    assert!(!r.irq(3 * NS), "12:34:59");
    assert!(r.irq(4 * NS), "12:35:00");
}

#[test]
fn an_alarm_inside_a_missed_stretch_is_not_lost() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_SECONDS_ALARM, 0x00, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0x35, 0);
    wr(&mut r, REG_HOURS_ALARM, 0x12, 0);
    wr(&mut r, REG_B, 0x22, 0);
    assert!(
        r.irq(600 * NS),
        "the guest was not looking for ten minutes: 12:35:00 passed"
    );
}

#[test]
fn alarm_comparison_uses_the_clock_format() {
    let mut r = Rtc::new(
        DateTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 15,
            minute: 0,
            second: 0,
        }
        .to_unix(),
    );
    wr(&mut r, REG_B, 0x20, 0); // BCD, 12h, AIE
    wr(&mut r, REG_SECONDS_ALARM, 0x01, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0x00, 0);
    wr(&mut r, REG_HOURS_ALARM, 0x03 | 0x80, 0);
    assert!(r.irq(NS), "3 PM in the chip's 12-hour format");
}

#[test]
fn nvram_is_plain_storage_and_the_firmware_can_prefill_it() {
    let mut r = Rtc::new(T0);
    r.set_nvram(0x34, 0xAB);
    r.set_nvram(0x00, 0xFF); // not NVRAM: ignored
    assert_eq!(rd(&mut r, 0x34, 0), 0xAB);
    wr(&mut r, 0x40, 0x5A, 0);
    assert_eq!(r.nvram(0x40), 0x5A);
    assert_eq!(rd(&mut r, 0x40, 0), 0x5A);
    assert_eq!(rd(&mut r, 0x7F, 0), 0);
    assert_eq!(
        rd(&mut r, 0x80 | 0x34, 0),
        0xAB,
        "bit 7 of the index is the NMI mask, not an address bit"
    );
    assert!(r.nmi_masked());
    r.write(PORT_INDEX, 0x34, 0);
    assert!(!r.nmi_masked());
}

#[test]
fn square_wave_is_counted_and_other_ports_are_not_claimed() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x0A, 0);
    assert_eq!(r.unsupported, 1);
    for p in [0x72, 0x73, 0x6F, 0x74] {
        assert!(!Rtc::owns(p) && r.read(p, 0).is_none() && !r.write(p, 0, 0));
    }
}

#[test]
fn a_deadline_exists_only_for_enabled_interrupts() {
    let mut r = Rtc::new(T0);
    assert_eq!(r.next_event(0), None);
    wr(&mut r, REG_B, 0x42, 0);
    assert!(r.next_event(0).is_some());
    wr(&mut r, REG_B, 0x02, 0);
    wr(&mut r, REG_B, 0x22, 0);
    assert_eq!(r.next_event(NS / 2), Some(NS));
}

#[test]
fn a_12_hour_alarm_with_pm_does_not_match_the_morning() {
    let mut r = Rtc::new(
        DateTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 3,
            minute: 0,
            second: 0,
        }
        .to_unix(),
    );
    wr(&mut r, REG_A, 0x20, 0);
    wr(&mut r, REG_B, 0x20, 0); // BCD, 12h, AIE
    wr(&mut r, REG_SECONDS_ALARM, 0x01, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0x00, 0);
    wr(&mut r, REG_HOURS_ALARM, 0x03 | 0x80, 0); // 3 PM
    assert!(!r.irq(NS), "it is 3 AM");
}

#[test]
fn the_alarm_flag_is_raised_with_the_interrupt_disabled() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_A, 0x20, 0);
    wr(&mut r, REG_SECONDS_ALARM, 0x57, 0);
    wr(&mut r, REG_MINUTES_ALARM, 0xC0, 0);
    wr(&mut r, REG_HOURS_ALARM, 0xC0, 0);
    assert!(!r.irq(NS), "AIE is off");
    assert_eq!(
        rd(&mut r, REG_C, NS),
        0x30,
        "AF and UF are flags even so; IRQF is clear"
    );
}

#[test]
fn the_next_second_is_counted_from_when_the_clock_was_set() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x82, 0);
    wr(&mut r, REG_B, 0x12, 10 * NS + NS / 2); // clear SET at 10.5 s, UIE
    assert_eq!(r.next_event(10 * NS + NS / 2), Some(11 * NS + NS / 2));
    assert!(!r.irq(11 * NS));
    assert!(r.irq(11 * NS + NS / 2));
}

#[test]
fn twelve_am_and_twelve_pm_in_the_12_hour_format() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x00, 0); // BCD, 12h
    wr(&mut r, REG_HOURS, 0x12, 0); // 12 AM
    assert_eq!(rd(&mut r, REG_HOURS, 0), 0x12);
    assert_eq!(r.datetime(0).hour, 0);
    wr(&mut r, REG_HOURS, 0x12 | 0x80, 0); // 12 PM
    assert_eq!(r.datetime(0).hour, 12);
    wr(&mut r, REG_HOURS, 0x01 | 0x80, 0); // 1 PM
    assert_eq!(r.datetime(0).hour, 13);
}

#[test]
fn setting_set_twice_keeps_the_frozen_time() {
    let mut r = Rtc::new(T0);
    wr(&mut r, REG_B, 0x82, 5 * NS);
    wr(&mut r, REG_B, 0x82, 50 * NS);
    assert_eq!(
        rd(&mut r, REG_SECONDS, 90 * NS),
        0x01,
        "frozen at the first SET: 12:34:56 + 5 s"
    );
}
