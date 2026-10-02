use vmm_devices::legacy::*;

#[test]
fn claimed_ports_and_only_those() {
    let mut l = Legacy::new();
    let claimed: Vec<u16> = (0..0x1000u16).filter(|p| Legacy::owns(*p)).collect();
    let expect: Vec<u16> = (0x00..=0x0F)
        .chain(0x80..=0x8F)
        .chain([0x92])
        .chain(0xC0..=0xDF)
        .chain([0xF0, 0xF1])
        .chain(0x378..=0x37A)
        .collect();
    assert_eq!(claimed, expect);
    for p in 0..0x1000u16 {
        assert_eq!(l.read(p).is_some(), Legacy::owns(p), "{p:#x}");
        assert_eq!(l.write(p, 0), Legacy::owns(p), "{p:#x}");
    }
}

#[test]
fn a20_gate_and_fast_reset() {
    let mut l = Legacy::new();
    assert!(!l.a20_enabled());
    assert_eq!(l.read(PORT_SYSTEM_A), Some(0));
    l.write(PORT_SYSTEM_A, 0x02);
    assert!(l.a20_enabled());
    assert_eq!(l.read(PORT_SYSTEM_A), Some(0x02));
    assert!(!l.take_reset());
    l.write(PORT_SYSTEM_A, 0x03);
    assert!(l.a20_enabled(), "a reset request does not change A20");
    assert_eq!(
        l.read(PORT_SYSTEM_A),
        Some(0x02),
        "bit 0 reads back as zero"
    );
    assert!(l.take_reset());
    assert!(!l.take_reset(), "reading the request clears it");
    l.write(PORT_SYSTEM_A, 0xC0);
    assert!(!l.a20_enabled(), "bits 7:6 are not the gate");
    assert_eq!(l.read(PORT_SYSTEM_A), Some(0));
}

#[test]
fn the_post_port_remembers_the_last_code() {
    let mut l = Legacy::new();
    l.write(PORT_POST, 0xAB);
    assert_eq!(l.post_code(), 0xAB);
    assert_eq!(l.read(PORT_POST), Some(0xAB));
    for p in 0x81..=0x8F {
        assert_eq!(l.read(p), Some(0), "the other page registers are separate");
    }
    l.write(0x87, 0x12);
    assert_eq!(l.read(0x87), Some(0x12));
    assert_eq!(l.post_code(), 0xAB);
}

#[test]
fn fpu_error_ports_are_counted_and_read_as_ones() {
    let mut l = Legacy::new();
    l.write(PORT_FPU_CLEAR, 0);
    l.write(PORT_FPU_RESET, 0);
    assert_eq!(l.fpu_clears, 2);
    assert_eq!(l.read(PORT_FPU_CLEAR), Some(0xFF));
}

#[test]
fn dma_registers_read_back_and_the_two_controllers_are_separate() {
    let mut l = Legacy::new();
    for p in 0..=0x0Fu16 {
        l.write(p, p as u8 + 1);
    }
    for p in (0xC0..=0xDEu16).step_by(2) {
        l.write(p, 0x80 + (p - 0xC0) as u8);
    }
    for p in 0..=0x0Fu16 {
        assert_eq!(l.read(p), Some(p as u8 + 1), "master {p:#x}");
    }
    for p in (0xC0..=0xDEu16).step_by(2) {
        assert_eq!(l.read(p), Some(0x80 + (p - 0xC0) as u8), "slave {p:#x}");
        assert_eq!(l.read(p + 1), Some(0xFF), "odd ports float");
    }
}

#[test]
fn a_parallel_port_probe_finds_nothing() {
    let mut l = Legacy::new();
    for p in 0x378..=0x37A {
        l.write(p, 0xAA);
        assert_eq!(
            l.read(p),
            Some(0xFF),
            "no printer: the data bus floats high"
        );
    }
}
