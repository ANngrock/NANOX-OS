//! The ACPI power-management registers as the Linux ACPI driver uses them.

use vmm_devices::acpi_pm::*;

const NS: u64 = 1_000_000_000;

fn r(a: &mut AcpiPm, port: u16, size: u8, now: u64) -> u32 {
    a.read(port, size, now).unwrap()
}

fn w(a: &mut AcpiPm, port: u16, size: u8, v: u32, now: u64) {
    assert!(a.write(port, size, v, now), "{port:#x}");
}

fn enable_acpi(a: &mut AcpiPm) {
    w(a, SMI_CMD, 1, u32::from(ACPI_ENABLE), 0);
}

#[test]
fn claimed_ports() {
    let mut a = AcpiPm::new();
    for p in [
        0x600, 0x601, 0x602, 0x603, 0x604, 0x605, 0x608, 0x60B, 0x620, 0x62F, 0xB2, 0xB3,
    ] {
        assert!(AcpiPm::owns(p), "{p:#x}");
    }
    for p in [0x5FF, 0x606, 0x607, 0x60C, 0x61F, 0x630, 0xB1, 0xB4, 0x92] {
        assert!(!AcpiPm::owns(p), "{p:#x}");
        assert!(a.read(p, 1, 0).is_none() && !a.write(p, 1, 0, 0), "{p:#x}");
    }
}

#[test]
fn acpi_mode_is_switched_by_the_smi_command() {
    let mut a = AcpiPm::new();
    assert!(!a.acpi_enabled());
    assert_eq!(r(&mut a, PM1_CONTROL, 2, 0) & 1, 0);
    enable_acpi(&mut a);
    assert!(a.acpi_enabled());
    assert_eq!(r(&mut a, PM1_CONTROL, 2, 0) & 1, 1, "SCI_EN reads back");
    w(&mut a, SMI_CMD, 1, u32::from(ACPI_DISABLE), 0);
    assert!(!a.acpi_enabled());
    w(&mut a, SMI_CMD, 1, 0x77, 0);
    assert_eq!(a.unsupported, 1);
    assert_eq!(r(&mut a, SMI_CMD, 1, 0), 0);
    w(&mut a, APM_STATUS, 1, 0xAB, 0);
    assert_eq!(r(&mut a, APM_STATUS, 1, 0), 0xAB);
}

#[test]
fn the_timer_counts_at_3_579_545_hz_in_24_bits() {
    let mut a = AcpiPm::new();
    assert_eq!(r(&mut a, PM_TIMER, 4, 0), 0);
    assert_eq!(r(&mut a, PM_TIMER, 4, NS), 3_579_545);
    assert_eq!(r(&mut a, PM_TIMER, 4, 10 * NS), 35_795_450 & 0xFF_FFFF);
    let wrap = 0x100_0000u64 * NS / 3_579_545;
    assert!(
        r(&mut a, PM_TIMER, 4, wrap + 1000) < 10_000,
        "it wraps at 2^24 ticks"
    );
    assert_eq!(
        r(&mut a, PM_TIMER, 4, NS),
        3_579_545,
        "reads are a function of virtual time"
    );
    w(&mut a, PM_TIMER, 4, 0x1234, NS);
    assert_eq!(r(&mut a, PM_TIMER, 4, NS), 3_579_545, "read-only");
    assert_eq!(
        r(&mut a, PM_TIMER, 2, NS),
        3_579_545 & 0xFFFF,
        "16-bit read"
    );
    assert_eq!(r(&mut a, PM_TIMER + 2, 2, NS), 3_579_545 >> 16);
    assert_eq!(
        r(&mut a, PM_TIMER + 3, 1, 10 * NS),
        0,
        "the top byte of a 24-bit timer is zero"
    );
    assert_eq!(r(&mut a, PM_TIMER + 1, 1, NS), (3_579_545 >> 8) & 0xFF);
}

#[test]
fn accesses_that_straddle_registers_are_refused() {
    let mut a = AcpiPm::new();
    assert!(
        a.read(0x601, 2, 0).is_none(),
        "0x601 and 0x602 are different registers"
    );
    assert!(a.read(0x600, 4, 0).is_none());
    assert!(!a.write(0x603, 2, 0, 0));
    assert!(a.read(0x608, 3, 0).is_none(), "odd sizes do not exist");
    assert!(
        a.read(0x62E, 4, 0).is_none(),
        "the enable register ends at 0x62F"
    );
    assert!(a.read(0x62C, 4, 0).is_some());
}

#[test]
fn the_timer_overflow_is_an_event_with_its_enable_and_sci() {
    let mut a = AcpiPm::new();
    enable_acpi(&mut a);
    let toggle = 0x80_0000u64 * NS / 3_579_545 + 1_000;
    assert!(!a.sci(toggle), "not enabled");
    assert_eq!(
        r(&mut a, PM1_STATUS, 2, toggle) & 1,
        1,
        "but the status bit is set"
    );
    w(&mut a, PM1_STATUS, 2, 1, toggle);
    assert_eq!(r(&mut a, PM1_STATUS, 2, toggle), 0, "write 1 to clear");
    w(&mut a, PM1_ENABLE, 2, 1, toggle);
    assert_eq!(
        a.next_event(toggle),
        Some(2 * 0x80_0000u64 * NS / 3_579_545 + 1),
        "the next toggle, rounded up"
    );
    assert!(!a.sci(toggle));
    let next = 2 * 0x80_0000u64 * NS / 3_579_545 + 1_000;
    assert!(a.sci(next));
    w(&mut a, PM1_STATUS, 2, 1, next);
    assert!(!a.sci(next));
    w(&mut a, PM1_ENABLE, 2, 0, next);
    assert_eq!(a.next_event(next), None, "nothing enabled: no deadline");
}

#[test]
fn the_sci_needs_sci_en() {
    let mut a = AcpiPm::new();
    w(&mut a, PM1_ENABLE, 2, 0x100, 0);
    a.press_power_button();
    assert!(!a.sci(0), "ACPI mode is off");
    enable_acpi(&mut a);
    assert!(a.sci(0));
}

#[test]
fn the_power_button_reaches_the_guest_as_an_event() {
    let mut a = AcpiPm::new();
    enable_acpi(&mut a);
    w(&mut a, PM1_ENABLE, 2, u32::from(STS_PWRBTN), 0);
    assert!(!a.sci(0));
    a.press_power_button();
    assert!(a.sci(0));
    assert_eq!(r(&mut a, PM1_STATUS, 2, 0), u32::from(STS_PWRBTN));
    w(&mut a, PM1_STATUS, 1, 0xFF, 0);
    assert!(a.sci(0), "a write to the low byte does not clear bit 8");
    w(&mut a, PM1_STATUS + 1, 1, 0x01, 0);
    assert!(!a.sci(0), "a write of 1 to bit 8 does");
}

#[test]
fn only_modeled_status_bits_exist() {
    let mut a = AcpiPm::new();
    w(&mut a, PM1_ENABLE, 2, 0xFFFF, 0);
    assert_eq!(
        r(&mut a, PM1_ENABLE, 2, 0),
        u32::from(STS_TMR | STS_GBL | STS_PWRBTN | STS_SLPBTN | STS_RTC | STS_WAK)
    );
    w(&mut a, PM1_ENABLE, 1, 0, 0);
    assert_eq!(
        r(&mut a, PM1_ENABLE, 2, 0),
        u32::from(STS_PWRBTN | STS_SLPBTN | STS_RTC | STS_WAK),
        "a byte write changes that byte only"
    );
}

#[test]
fn gpe_events_and_their_enables() {
    let mut a = AcpiPm::new();
    enable_acpi(&mut a);
    a.raise_gpe(3);
    a.raise_gpe(64);
    assert!(!a.sci(0), "not enabled");
    assert_eq!(r(&mut a, GPE0_STATUS, 4, 0), 1 << 3);
    w(&mut a, GPE0_ENABLE, 1, 1 << 3, 0);
    assert_eq!(r(&mut a, GPE0_ENABLE, 1, 0), 8);
    assert!(a.sci(0));
    w(&mut a, GPE0_STATUS, 1, 1 << 3, 0);
    assert!(!a.sci(0));
    a.raise_gpe(63);
    w(&mut a, GPE0_ENABLE + 7, 1, 0x80, 0);
    assert!(a.sci(0), "the last bit of the 64");
    assert_eq!(r(&mut a, GPE0_STATUS + 4, 4, 0), 0x8000_0000);
    w(&mut a, GPE0_STATUS + 4, 4, 0x8000_0000, 0);
    assert!(!a.sci(0));
}

#[test]
fn control_register_keeps_sci_en_and_the_sleep_type_and_not_slp_en() {
    let mut a = AcpiPm::new();
    w(&mut a, PM1_CONTROL, 2, 0xFFFF & !(1 << 13), 0);
    assert_eq!(
        r(&mut a, PM1_CONTROL, 2, 0),
        0x1C07,
        "SCI_EN, BM_RLD, GBL_RLS and SLP_TYP"
    );
    assert_eq!(a.take_sleep(), None);
}

#[test]
fn writing_slp_en_with_s5_is_the_guest_powering_off() {
    let mut a = AcpiPm::new();
    enable_acpi(&mut a);
    w(
        &mut a,
        PM1_CONTROL,
        2,
        1 | (u32::from(SLEEP_S5) << 10) | (1 << 13),
        0,
    );
    assert_eq!(a.take_sleep(), Some(SLEEP_S5));
    assert_eq!(a.take_sleep(), None, "taking it clears it");
    assert_eq!(
        r(&mut a, PM1_CONTROL, 2, 0) & (1 << 13),
        0,
        "SLP_EN does not stay set"
    );
    assert!(a.acpi_enabled());
    // another sleep type is reported as it is, for the VMM to decide
    w(&mut a, PM1_CONTROL, 2, 1 | (3 << 10) | (1 << 13), 0);
    assert_eq!(a.take_sleep(), Some(3));
    // writing the type without SLP_EN is only a preparation
    w(&mut a, PM1_CONTROL, 2, 1 | (5 << 10), 0);
    assert_eq!(a.take_sleep(), None);
}

#[test]
fn a_byte_write_of_slp_en_in_the_high_byte_counts() {
    let mut a = AcpiPm::new();
    w(&mut a, PM1_CONTROL, 1, 1, 0);
    w(&mut a, PM1_CONTROL + 1, 1, (5 << 2) | (1 << 5), 0);
    assert_eq!(a.take_sleep(), Some(5));
}

#[test]
fn partial_status_writes_clear_only_their_bytes() {
    let mut a = AcpiPm::new();
    a.raise_gpe(1);
    a.raise_gpe(9);
    w(&mut a, GPE0_STATUS, 1, 0xFF, 0);
    assert_eq!(
        r(&mut a, GPE0_STATUS, 2, 0),
        1 << 9,
        "the low byte cleared, the next byte kept"
    );
}
