//! The 8254: every mode's OUT and count at exact virtual times, the access
//! modes and flip-flops, latches and read-back, counts written mid-run, the
//! gate of channel 2, BCD, and channel 0's edges for IRQ0.
//!
//! Expected times come from `clk`: the k-th PIT clock after a start at 0
//! falls at ceil(k * 1e9 / 1_193_182) ns, the rounding the crate uses; the
//! first test pins it to hand-computed values.

use vmm_devices::pit::{
    Pit, HZ, PORT_CHANNEL0 as CH0, PORT_CHANNEL1 as CH1, PORT_CHANNEL2 as CH2, PORT_CONTROL as CTL,
    PORT_SPEAKER as SPK,
};

/// When the k-th clock of a run started at 0 has come.
fn clk(k: u64) -> u64 {
    (u128::from(k) * 1_000_000_000).div_ceil(u128::from(HZ)) as u64
}

/// Clocks in `ns`: floor(ns * HZ / 1e9).
fn clocks_in(ns: u64) -> u64 {
    (u128::from(ns) * u128::from(HZ) / 1_000_000_000) as u64
}

fn ctl(p: &mut Pit, v: u8, now: u64) {
    assert!(p.write(CTL, v, now));
}

/// A count to a channel programmed for LSB then MSB.
fn count16(p: &mut Pit, port: u16, n: u16, now: u64) {
    assert!(p.write(port, n as u8, now));
    assert!(p.write(port, (n >> 8) as u8, now));
}

/// The counter latch command, then the two bytes (LSB then MSB access).
fn latched(p: &mut Pit, ch: u8, now: u64) -> u16 {
    ctl(p, ch << 6, now);
    let port = CH0 + u16::from(ch);
    let lo = p.read(port, now).unwrap();
    let hi = p.read(port, now).unwrap();
    u16::from(lo) | u16::from(hi) << 8
}

/// The read-back command for the status of one channel, and its read.
fn status(p: &mut Pit, ch: u8, now: u64) -> u8 {
    ctl(p, 0xE0 | 2 << ch, now);
    p.read(CH0 + u16::from(ch), now).unwrap()
}

#[test]
fn the_clock_times_are_the_hand_computed_ones() {
    assert_eq!(clk(1), 839, "838.095 ns rounded up");
    assert_eq!(clk(2), 1_677);
    assert_eq!(clk(101), 84_648);
    assert_eq!(clk(1_194), 1_000_686);
    assert_eq!(clk(65_537), 54_926_240);
    assert_eq!(clocks_in(1_000_000), 1_193);
    assert_eq!(clocks_in(clk(20)), 20);
}

#[test]
fn the_chip_owns_its_four_ports_and_port_0x61() {
    for port in [0x40, 0x41, 0x42, 0x43, 0x61] {
        assert!(Pit::owns(port), "{port:#x}");
    }
    let mut p = Pit::new();
    for port in [0x3F, 0x44, 0x60, 0x62] {
        assert!(!Pit::owns(port), "{port:#x}");
        assert_eq!(p.read(port, 0), None);
        assert!(!p.write(port, 0, 0));
    }
    assert_eq!(p.unsupported, 0);
    assert_eq!(
        p.take_edges(1_000_000_000),
        0,
        "nothing programmed, nothing ticks"
    );
    assert_eq!(p.next_event(0), None);
}

// ------------------------------------------------------------------ modes

#[test]
fn mode_0_raises_out_once_after_n_plus_1_clocks() {
    let mut p = Pit::new();
    let t0 = 10_000;
    assert!(!p.out(0, t0), "power-on: no count, OUT low");
    ctl(&mut p, 0x30, t0);
    count16(&mut p, CH0, 1000, t0);
    assert_eq!(
        status(&mut p, 0, t0),
        0x70,
        "OUT low, null count: not loaded yet"
    );
    assert_eq!(latched(&mut p, 0, t0), 1000, "before the loading clock");
    assert_eq!(status(&mut p, 0, t0 + clk(1)), 0x30, "loaded");
    assert_eq!(latched(&mut p, 0, t0 + clk(1)), 1000);
    assert_eq!(latched(&mut p, 0, t0 + clk(2)), 999);
    assert_eq!(latched(&mut p, 0, t0 + clk(1000)), 1);
    assert_eq!(p.next_event(t0), Some(t0 + clk(1001)));
    assert!(!p.out(0, t0 + clk(1001) - 1));
    assert_eq!(p.take_edges(t0 + clk(1001) - 1), 0);
    assert!(p.out(0, t0 + clk(1001)));
    assert_eq!(p.take_edges(t0 + clk(1001)), 1);
    assert_eq!(latched(&mut p, 0, t0 + clk(1001)), 0);
    assert_eq!(status(&mut p, 0, t0 + clk(1001)), 0xB0);
    assert_eq!(
        latched(&mut p, 0, t0 + clk(1002)),
        0xFFFF,
        "the counter goes on, wrapping"
    );
    assert_eq!(p.next_event(t0 + clk(1001)), None, "once");
    assert_eq!(p.take_edges(t0 + clk(200_000)), 0);
    assert!(p.out(0, t0 + clk(200_000)), "OUT stays high");
    assert_eq!(p.unsupported, 0);
}

#[test]
fn mode_0_first_byte_stops_the_count_and_drops_out_the_second_restarts() {
    let mut p = Pit::new();
    ctl(&mut p, 0x30, 0);
    count16(&mut p, CH0, 10, 0);
    let t1 = clk(20);
    assert_eq!(p.take_edges(t1), 1);
    p.write(CH0, 5, t1);
    assert!(!p.out(0, t1), "the first byte drops OUT");
    assert_eq!(
        latched(&mut p, 0, t1 + 5_000),
        0xFFF7,
        "and stops the count where it was: 10 - 19"
    );
    assert_eq!(p.next_event(t1 + 5_000), None, "not counting");
    let t2 = t1 + 10_000;
    p.write(CH0, 0, t2);
    assert!(!p.out(0, t2 + clk(6) - 1));
    assert!(p.out(0, t2 + clk(6)));
    assert_eq!(p.take_edges(t2 + clk(6)), 1);
}

#[test]
fn mode_2_pulses_out_low_for_one_clock_every_n_clocks() {
    const N: u64 = 1193; // Linux's LATCH for HZ = 1000
    let mut p = Pit::new();
    let t0 = 7;
    ctl(&mut p, 0x34, t0);
    count16(&mut p, CH0, N as u16, t0);
    p.take_edges(t0); // the control word's own edge (see control_words_that_raise_out0_are_edges)
    for j in 1..=3 {
        let low = t0 + clk(j * N); // the clock its counter is 1
        let high = t0 + clk(j * N + 1); // reloaded: OUT rises, IRQ0's edge
        assert!(p.out(0, low - 1), "{j}");
        assert!(!p.out(0, low), "{j}");
        assert!(!p.out(0, high - 1), "{j}");
        assert!(p.out(0, high), "{j}");
    }
    assert_eq!(p.next_event(t0 + clk(N)), Some(t0 + clk(N + 1)));
    assert_eq!(p.next_event(t0 + clk(N + 1) - 1), Some(t0 + clk(N + 1)));
    assert_eq!(p.next_event(t0 + clk(N + 1)), Some(t0 + clk(2 * N + 1)));
    assert_eq!(latched(&mut p, 0, t0 + clk(1)), N as u16);
    assert_eq!(latched(&mut p, 0, t0 + clk(2)), N as u16 - 1);
    assert_eq!(latched(&mut p, 0, t0 + clk(N)), 1);
    assert_eq!(p.take_edges(t0 + clk(N + 1) - 1), 0);
    assert_eq!(p.take_edges(t0 + clk(N + 1)), 1);
    assert_eq!(latched(&mut p, 0, t0 + clk(N + 1)), N as u16, "reloaded");
    assert_eq!(latched(&mut p, 0, t0 + clk(N + 2)), N as u16 - 1);
    assert_eq!(p.take_edges(t0 + clk(3 * N + 1) - 1), 1);
    assert_eq!(p.take_edges(t0 + clk(3 * N + 1)), 1);
    assert_eq!(
        p.take_edges(t0 + clk(1000 * N + 1)),
        997,
        "a late sync gets them all"
    );
    // A huge gap is one division, not a loop.
    let far = 1_000_000_000_000_000; // 11.6 days
    let total = (clocks_in(far - t0) - (N + 1)) / N + 1;
    assert_eq!(p.take_edges(far), total - 1000);
    assert_eq!(p.next_event(far), Some(t0 + clk(total * N + N + 1)));
    assert_eq!(p.take_edges(far), 0);
}

#[test]
fn mode_3_square_wave_even_and_odd_counts() {
    // even: high for N/2 clocks, low for N/2; the counter goes down by two in each half
    let mut p = Pit::new();
    ctl(&mut p, 0x16, 0); // channel 0, LSB only, mode 3
    p.write(CH0, 10, 0);
    p.take_edges(0);
    for (c, v) in [(1, 10), (2, 8), (5, 2), (6, 10), (7, 8), (10, 2), (11, 10)] {
        assert_eq!(p.read(CH0, clk(c)), Some(v), "clock {c}");
    }
    for (t, high) in [
        (clk(6) - 1, true),
        (clk(6), false),
        (clk(11) - 1, false),
        (clk(11), true),
        (clk(16), false),
        (clk(21), true),
    ] {
        assert_eq!(p.out(0, t), high, "{t}");
    }
    assert_eq!(p.next_event(0), Some(clk(11)));
    assert_eq!(p.take_edges(clk(21)), 2);
    // odd: high for (N + 1)/2 clocks, low for (N - 1)/2; N - 1 is loaded, and the high half
    // has the extra clock at 0
    let mut p = Pit::new();
    ctl(&mut p, 0x16, 0);
    p.write(CH0, 11, 0);
    p.take_edges(0);
    for (c, v) in [
        (1, 10),
        (2, 8),
        (5, 2),
        (6, 0),
        (7, 10),
        (8, 8),
        (11, 2),
        (12, 10),
    ] {
        assert_eq!(p.read(CH0, clk(c)), Some(v), "clock {c}");
    }
    for (t, high) in [
        (clk(7) - 1, true),
        (clk(7), false),
        (clk(12) - 1, false),
        (clk(12), true),
        (clk(18), false),
        (clk(23), true),
    ] {
        assert_eq!(p.out(0, t), high, "{t}");
    }
    assert_eq!(p.next_event(clk(12)), Some(clk(23)));
    assert_eq!(p.take_edges(clk(23)), 2);
}

#[test]
fn mode_4_strobes_once_and_a_new_count_retriggers() {
    // Linux's one-shot clock event: 0x38, then each event's count.
    let mut p = Pit::new();
    ctl(&mut p, 0x38, 0);
    assert!(p.out(0, 0), "OUT high after the control word");
    p.take_edges(0);
    count16(&mut p, CH0, 100, 0);
    assert_eq!(p.next_event(0), Some(clk(102)));
    let t = clk(60);
    p.write(CH0, 50, t);
    assert_eq!(
        latched(&mut p, 0, t),
        41,
        "the first byte does not disturb the count"
    );
    p.write(CH0, 0, t); // count 50: retriggered
    assert!(p.out(0, clk(101)), "no strobe at the old count's end");
    assert_eq!(p.next_event(t), Some(t + clk(52)));
    assert!(p.out(0, t + clk(51) - 1));
    assert!(
        !p.out(0, t + clk(51)),
        "low for one clock at the terminal count"
    );
    assert!(!p.out(0, t + clk(52) - 1));
    assert!(p.out(0, t + clk(52)));
    assert_eq!(p.take_edges(t + clk(52)), 1);
    assert_eq!(
        latched(&mut p, 0, t + clk(52)),
        0xFFFF,
        "the counter wraps on"
    );
    assert_eq!(p.next_event(t + clk(52)), None);
    assert!(
        p.out(0, t + clk(51 + 0x1_0000)),
        "and does not strobe again"
    );
    assert_eq!(p.take_edges(t + clk(200_000)), 0);
}

#[test]
fn modes_1_and_5_wait_for_the_gate_of_channel_2() {
    let mut p = Pit::new();
    ctl(&mut p, 0x92, 0); // channel 2, LSB only, mode 1
    p.write(CH2, 5, 0);
    assert!(p.out2(1_000_000), "armed, not triggered: OUT high");
    assert_eq!(
        status(&mut p, 2, 1_000_000),
        0xD2,
        "OUT high, null count, LSB, mode 1"
    );
    assert_eq!(p.out2_deadline(), None);
    let t = 2_000_000;
    p.write(SPK, 1, t); // the gate's rising edge triggers
    assert!(p.out2(t + clk(1) - 1));
    assert!(!p.out2(t + clk(1)), "low from the clock after the trigger");
    assert_eq!(p.out2_deadline(), Some(t + clk(6)));
    assert_eq!(status(&mut p, 2, t + clk(1)), 0x12, "loaded, OUT low");
    assert!(!p.out2(t + clk(6) - 1));
    assert!(p.out2(t + clk(6)), "for N clocks");
    // Retriggering during the pulse restarts it; the gate going low does nothing in mode 1.
    let t2 = t + clk(3);
    p.write(SPK, 0, t2);
    assert!(!p.out2(t2 + 1), "gate low: the pulse goes on");
    let t3 = t2 + 100;
    p.write(SPK, 1, t3);
    assert!(
        !p.out2(t + clk(6)),
        "retriggered: still low past the first pulse's end"
    );
    assert!(!p.out2(t3 + clk(6) - 1));
    assert!(p.out2(t3 + clk(6)));
    // A count written during the pulse waits for the next trigger.
    p.write(CH2, 2, t3 + clk(2));
    assert_eq!(
        status(&mut p, 2, t3 + clk(2)),
        0x52,
        "null count until the trigger"
    );
    assert!(p.out2(t3 + clk(6)), "the running pulse keeps its count");
    let t4 = t3 + clk(10);
    p.write(SPK, 0, t4);
    p.write(SPK, 1, t4);
    assert!(!p.out2(t4 + clk(3) - 1));
    assert!(p.out2(t4 + clk(3)), "the new count, 2");
    // Written before the loading clock of a trigger, a count is the one loaded.
    let t5 = t4 + clk(10);
    p.write(SPK, 0, t5);
    p.write(SPK, 1, t5);
    p.write(CH2, 3, t5 + 1);
    assert!(!p.out2(t5 + clk(4) - 1));
    assert!(p.out2(t5 + clk(4)));
    // Mode 5: a strobe N + 1 clocks after the trigger.
    ctl(&mut p, 0x9A, t5 + clk(10));
    p.write(SPK, 0, t5 + clk(10));
    p.write(CH2, 5, t5 + clk(10));
    let t6 = t5 + clk(20);
    assert!(p.out2(t6), "not triggered");
    p.write(SPK, 1, t6);
    assert!(p.out2(t6 + clk(6) - 1));
    assert!(!p.out2(t6 + clk(6)));
    assert!(p.out2(t6 + clk(7)));
    assert_eq!(p.out2_deadline(), Some(t6 + clk(7)));
    assert_eq!(p.unsupported, 0);
}

#[test]
fn modes_1_and_5_never_start_on_channel_0_whose_gate_has_no_edge() {
    for ctrl in [0x12, 0x1A] {
        let mut p = Pit::new();
        ctl(&mut p, ctrl, 0);
        p.write(CH0, 5, 0);
        assert_eq!(p.take_edges(0), 1, "the control word raised OUT0");
        assert!(p.out(0, 1_000_000_000));
        assert_eq!(p.next_event(0), None);
        assert_eq!(p.take_edges(1_000_000_000), 0);
        assert_eq!(
            status(&mut p, 0, 1_000_000_000),
            0xC0 | ctrl,
            "never loaded"
        );
    }
}

#[test]
fn the_gate_stops_modes_2_and_3_and_pauses_mode_4() {
    // mode 2 on channel 2: the gate's rising edge starts it
    let mut p = Pit::new();
    ctl(&mut p, 0xB4, 0);
    count16(&mut p, CH2, 10, 0);
    assert!(p.out2(clk(10)), "gate closed: stopped, OUT high");
    assert_eq!(p.out2_deadline(), None);
    p.write(SPK, 1, 0);
    assert_eq!(p.out2_deadline(), Some(clk(11)));
    assert!(!p.out2(clk(10)));
    // gate low during the pulse: OUT high at once, the count holds
    p.write(SPK, 0, clk(10));
    assert!(p.out2(clk(10)));
    assert_eq!(latched(&mut p, 2, clk(10) + 5_000), 1);
    // a count written while stopped is taken by the next trigger
    count16(&mut p, CH2, 4, clk(10) + 6_000);
    assert_eq!(
        status(&mut p, 2, clk(10) + 7_000),
        0xF4,
        "OUT high, null count"
    );
    let t = clk(10) + 10_000;
    p.write(SPK, 1, t);
    assert_eq!(
        latched(&mut p, 2, t + clk(1)),
        4,
        "reloaded with the new count"
    );
    assert!(p.out2(t + clk(4) - 1));
    assert!(!p.out2(t + clk(4)));
    assert!(p.out2(t + clk(5)));
    assert_eq!(p.out2_deadline(), Some(t + clk(5)));
    // mode 3 too
    ctl(&mut p, 0xB6, t);
    count16(&mut p, CH2, 4, t);
    assert!(!p.out2(t + clk(3)), "the gate is open: running");
    p.write(SPK, 0, t + clk(3));
    assert!(p.out2(t + clk(3)), "gate low: OUT high");
    // mode 4: the gate pauses the count
    let mut p = Pit::new();
    p.write(SPK, 1, 0);
    ctl(&mut p, 0x98, 0); // channel 2, LSB only, mode 4
    p.write(CH2, 10, 0);
    p.write(SPK, 0, clk(5));
    assert_eq!(p.read(CH2, clk(5) + 100_000), Some(6), "paused at 10 - 4");
    let t = 1_000_000;
    p.write(SPK, 1, t);
    assert!(p.out2(t + clk(6) - 1));
    assert!(
        !p.out2(t + clk(6)),
        "the strobe, 6 clocks of counting later"
    );
    assert!(p.out2(t + clk(7)));
}

// ---------------------------------------------------------- access and latches

#[test]
fn lsb_only_msb_only_and_interleaved_two_byte_access() {
    let mut p = Pit::new();
    ctl(&mut p, 0x54, 0); // channel 1, LSB only, mode 2
    p.write(CH1, 0x34, 0);
    assert_eq!(p.read(CH1, clk(1)), Some(0x34));
    assert_eq!(p.read(CH1, clk(2)), Some(0x33), "every read is the LSB");
    let t = clk(2);
    ctl(&mut p, 0x64, t); // MSB only
    p.write(CH1, 0x12, t); // count 0x1200
    assert_eq!(p.read(CH1, t + clk(1)), Some(0x12));
    assert_eq!(
        p.read(CH1, t + clk(2)),
        Some(0x11),
        "every read is the MSB: 0x11FF"
    );
    // LSB then MSB, reads and writes interleaved (the data sheet's example)
    let t = clk(100);
    ctl(&mut p, 0x74, t);
    count16(&mut p, CH1, 0x1234, t);
    let r = t + clk(1);
    assert_eq!(p.read(CH1, r), Some(0x34));
    p.write(CH1, 0x00, r); // a new count's LSB
    assert_eq!(
        p.read(CH1, r),
        Some(0x12),
        "the read flip-flop is not the write one"
    );
    p.write(CH1, 0x01, r); // its MSB: 0x0100, taken at the end of the period
    assert_eq!(p.read(CH1, r), Some(0x34));
    assert_eq!(p.read(CH1, r), Some(0x12));
    assert_eq!(latched(&mut p, 1, t + clk(0x1234)), 1);
    assert_eq!(latched(&mut p, 1, t + clk(0x1235)), 0x0100);
    assert_eq!(p.take_edges(t + clk(0x2000)), 0, "channel 1 is not IRQ0");
}

#[test]
fn the_counter_latch_holds_one_value_until_it_is_read() {
    let mut p = Pit::new();
    ctl(&mut p, 0x30, 0);
    count16(&mut p, CH0, 0x0100, 0);
    // unlatched, the two bytes come from different moments
    assert_eq!(p.read(CH0, clk(1)), Some(0x00), "LSB of 0x0100");
    assert_eq!(p.read(CH0, clk(2)), Some(0x00), "MSB of 0x00FF: torn");
    ctl(&mut p, 0x00, clk(3)); // latch: 0x00FE
    ctl(&mut p, 0x00, clk(5)); // a second latch before the read is ignored
    assert_eq!(p.read(CH0, clk(6)), Some(0xFE));
    assert_eq!(p.read(CH0, clk(7)), Some(0x00));
    assert_eq!(p.read(CH0, clk(8)), Some(0xF9), "read out: live again");
    assert_eq!(p.read(CH0, clk(8)), Some(0x00));
    ctl(&mut p, 0x00, clk(9));
    assert_eq!(p.read(CH0, clk(20)), Some(0xF8), "a new latch takes");
    assert_eq!(p.read(CH0, clk(20)), Some(0x00));
    // one-byte access: one read empties the latch
    let t = clk(100);
    ctl(&mut p, 0x54, t); // channel 1, LSB
    p.write(CH1, 0x34, t);
    ctl(&mut p, 0x40, t + clk(1));
    assert_eq!(p.read(CH1, t + clk(5)), Some(0x34));
    assert_eq!(p.read(CH1, t + clk(5)), Some(0x30), "52 - 4, live");
    let t = t + clk(5);
    ctl(&mut p, 0x64, t); // channel 1, MSB: 0x1200
    p.write(CH1, 0x12, t);
    ctl(&mut p, 0x40, t + clk(1));
    assert_eq!(p.read(CH1, t + clk(2)), Some(0x12));
    assert_eq!(p.read(CH1, t + clk(2)), Some(0x11), "0x11FF, live");
}

#[test]
fn read_back_latches_counts_and_statuses_of_several_channels() {
    let mut p = Pit::new();
    ctl(&mut p, 0x34, 0);
    count16(&mut p, CH0, 1000, 0);
    ctl(&mut p, 0x74, 0);
    count16(&mut p, CH1, 500, 0);
    let t = clk(11);
    ctl(&mut p, 0xC2, t); // count and status of channel 0
    assert_eq!(
        p.read(CH0, t + 5_000),
        Some(0xB4),
        "status first: OUT high, loaded, LSB/MSB, mode 2, binary"
    );
    assert_eq!(p.read(CH0, t + 6_000), Some(0xDE), "then the count: 990");
    assert_eq!(p.read(CH0, t + 7_000), Some(0x03));
    assert_eq!(p.read(CH0, clk(20)), Some(0xD5), "then live: 981");
    assert_eq!(p.read(CH0, clk(20)), Some(0x03));
    // all three, twice: the first latch stays
    ctl(&mut p, 0xCE, clk(30));
    ctl(&mut p, 0xCE, clk(40));
    let read3 = |p: &mut Pit, port| [0, 1, 2].map(|_| p.read(port, clk(45)).unwrap());
    assert_eq!(read3(&mut p, CH0), [0xB4, 0xCB, 0x03], "971");
    assert_eq!(read3(&mut p, CH1), [0xB4, 0xD7, 0x01], "471");
    assert_eq!(
        read3(&mut p, CH2),
        [0x70, 0x00, 0x00],
        "power-on: mode 0, no count, OUT low"
    );
    // count only, status only
    ctl(&mut p, 0xD2, clk(50));
    assert_eq!(latched_bytes(&mut p, CH0, clk(55)), 951);
    ctl(&mut p, 0xE4, clk(60));
    assert_eq!(p.read(CH1, clk(60)), Some(0xB4));
    assert_eq!(
        latched_bytes(&mut p, CH1, clk(60)),
        441,
        "no count latched: live"
    );
    // a control word drops what was latched
    ctl(&mut p, 0xC2, clk(70));
    ctl(&mut p, 0x34, clk(70));
    assert_eq!(
        latched_bytes(&mut p, CH0, clk(70)),
        0,
        "no status, no old count"
    );
}

/// Two reads of a channel, LSB then MSB.
fn latched_bytes(p: &mut Pit, port: u16, now: u64) -> u16 {
    let lo = p.read(port, now).unwrap();
    let hi = p.read(port, now).unwrap();
    u16::from(lo) | u16::from(hi) << 8
}

// ------------------------------------------------------ counts written mid-run

#[test]
fn mode_2_takes_a_new_count_at_the_end_of_the_period() {
    let mut p = Pit::new();
    ctl(&mut p, 0x34, 0);
    count16(&mut p, CH0, 100, 0);
    p.take_edges(0);
    assert_eq!(p.take_edges(clk(150)), 1);
    count16(&mut p, CH0, 40, clk(150));
    assert_eq!(
        status(&mut p, 0, clk(150)),
        0xF4,
        "null count: not taken yet"
    );
    assert_eq!(latched(&mut p, 0, clk(150)), 51, "the period runs on");
    assert_eq!(p.next_event(clk(150)), Some(clk(201)));
    assert!(!p.out(0, clk(200)));
    assert!(p.out(0, clk(201)));
    assert_eq!(latched(&mut p, 0, clk(200)), 1);
    assert_eq!(latched(&mut p, 0, clk(201)), 40, "taken at the reload");
    assert_eq!(status(&mut p, 0, clk(201)), 0xB4);
    assert!(p.out(0, clk(240) - 1));
    assert!(!p.out(0, clk(240)));
    assert!(p.out(0, clk(241)));
    assert_eq!(p.next_event(clk(201)), Some(clk(241)));
    assert_eq!(p.take_edges(clk(241)), 2);
    // once more, now that the first one was taken
    count16(&mut p, CH0, 60, clk(250));
    assert_eq!(p.next_event(clk(250)), Some(clk(281)));
    assert_eq!(latched(&mut p, 0, clk(280)), 1);
    assert_eq!(latched(&mut p, 0, clk(281)), 60);
    assert_eq!(p.take_edges(clk(281)), 1);
    assert_eq!(p.next_event(clk(281)), Some(clk(341)));
    assert_eq!(p.take_edges(clk(401)), 2);
    // a second count before the first is taken replaces it
    let mut p = Pit::new();
    ctl(&mut p, 0x34, 0);
    count16(&mut p, CH0, 100, 0);
    count16(&mut p, CH0, 40, clk(150));
    count16(&mut p, CH0, 30, clk(170));
    assert_eq!(p.next_event(clk(170)), Some(clk(201)));
    assert_eq!(p.next_event(clk(201)), Some(clk(231)));
}

#[test]
fn mode_3_takes_a_new_count_at_the_end_of_the_half_period() {
    // written in the high half: taken where OUT falls, into the new count's low half
    let mut p = Pit::new();
    ctl(&mut p, 0x36, 0);
    count16(&mut p, CH0, 10, 0);
    p.take_edges(0);
    count16(&mut p, CH0, 20, clk(3));
    assert_eq!(latched(&mut p, 0, clk(5)), 2);
    assert!(p.out(0, clk(6) - 1));
    assert!(!p.out(0, clk(6)));
    assert_eq!(latched(&mut p, 0, clk(6)), 20);
    assert_eq!(latched(&mut p, 0, clk(7)), 18);
    assert_eq!(p.next_event(clk(6)), Some(clk(16)));
    assert!(
        !p.out(0, clk(16) - 1),
        "low for the new count's half: 10 clocks"
    );
    assert!(p.out(0, clk(16)));
    assert!(!p.out(0, clk(26)));
    assert!(p.out(0, clk(36)));
    assert_eq!(p.take_edges(clk(36)), 2);
    // written in the low half: taken where OUT rises
    let mut p = Pit::new();
    ctl(&mut p, 0x36, 0);
    count16(&mut p, CH0, 10, 0);
    p.take_edges(0);
    count16(&mut p, CH0, 6, clk(7));
    assert_eq!(p.next_event(clk(7)), Some(clk(11)));
    assert_eq!(latched(&mut p, 0, clk(11)), 6);
    assert_eq!(latched(&mut p, 0, clk(12)), 4);
    assert!(p.out(0, clk(14) - 1));
    assert!(!p.out(0, clk(14)));
    assert!(p.out(0, clk(17)));
    assert_eq!(p.take_edges(clk(17)), 2);
}

// --------------------------------------------------------- counts and BCD

/// A fresh chip with channel 0 programmed at time 0.
fn channel0(ctrl: u8, n: u16) -> Pit {
    let mut p = Pit::new();
    ctl(&mut p, ctrl, 0);
    count16(&mut p, CH0, n, 0);
    p
}

#[test]
fn a_count_of_0_is_65536() {
    let mut p = channel0(0x30, 0);
    assert_eq!(latched(&mut p, 0, clk(1)), 0);
    assert_eq!(latched(&mut p, 0, clk(2)), 0xFFFF);
    assert_eq!(p.next_event(clk(2)), Some(54_926_240), "65537 clocks");
    assert!(!p.out(0, 54_926_239));
    assert!(p.out(0, 54_926_240));
    // mode 2: a period of 65536 clocks
    let p = channel0(0x34, 0);
    assert_eq!(p.next_event(0), Some(clk(65_537)));
    assert_eq!(p.next_event(clk(65_537)), Some(clk(131_073)));
    // mode 3: 32768 clocks high, from 65536 down by two
    let mut p = channel0(0x36, 0);
    assert_eq!(latched(&mut p, 0, clk(1)), 0);
    assert_eq!(latched(&mut p, 0, clk(2)), 0xFFFE);
    assert!(p.out(0, clk(32_769) - 1));
    assert!(!p.out(0, clk(32_769)));
    // one byte of 0 is 65536 too
    let mut p = Pit::new();
    ctl(&mut p, 0x10, 0);
    p.write(CH0, 0, 0);
    assert_eq!(p.next_event(0), Some(54_926_240));
}

#[test]
fn a_count_of_1_in_modes_2_and_3_never_raises_out() {
    // the chip forbids it; here mode 2 stays low, mode 3 high, and there are no edges
    for (ctrl, level) in [(0x14, false), (0x16, true)] {
        let mut p = Pit::new();
        ctl(&mut p, ctrl, 0);
        p.write(CH0, 1, 0);
        p.take_edges(0);
        assert_eq!(p.out(0, clk(5)), level, "{ctrl:#x}");
        assert_eq!(p.next_event(0), None);
        assert_eq!(p.take_edges(clk(1000)), 0);
    }
}

#[test]
fn bcd_counting() {
    let mut p = channel0(0x31, 0x0100); // mode 0, BCD: 100
    assert_eq!(latched(&mut p, 0, clk(1)), 0x0100);
    assert_eq!(latched(&mut p, 0, clk(2)), 0x0099);
    assert!(!p.out(0, clk(101) - 1));
    assert!(p.out(0, clk(101)));
    assert_eq!(latched(&mut p, 0, clk(101)), 0x0000);
    assert_eq!(latched(&mut p, 0, clk(102)), 0x9999, "wraps in decimal");
    assert_eq!(status(&mut p, 0, clk(102)), 0xB1);
    assert_eq!(p.unsupported, 0);
    // all four digits
    let mut p = channel0(0x31, 0x1234);
    assert_eq!(latched(&mut p, 0, clk(2)), 0x1233);
    assert_eq!(latched(&mut p, 0, clk(236)), 0x0999);
    // 0 is 10000
    let mut p = channel0(0x31, 0);
    assert_eq!(p.next_event(0), Some(clk(10_001)));
    assert_eq!(latched(&mut p, 0, clk(1)), 0x0000);
    assert_eq!(latched(&mut p, 0, clk(2)), 0x9999);
    // mode 2 and mode 3
    let mut p = channel0(0x35, 0x0050);
    assert_eq!(latched(&mut p, 0, clk(2)), 0x0049);
    assert_eq!(p.next_event(clk(51)), Some(clk(101)));
    let mut p = channel0(0x37, 0x0012);
    assert_eq!(latched(&mut p, 0, clk(1)), 0x0012);
    assert_eq!(latched(&mut p, 0, clk(2)), 0x0010);
    assert_eq!(latched(&mut p, 0, clk(7)), 0x0012, "the low half");
    assert_eq!(p.unsupported, 0);
    // a digit above 9: counted with its weight, and as unsupported
    let p = channel0(0x31, 0x00AF);
    assert_eq!(p.unsupported, 1);
    assert_eq!(p.next_event(0), Some(clk(116)), "10 * 10 + 15 + 1 clocks");
}

// ----------------------------------------------------------------- IRQ0 edges

#[test]
fn control_words_that_raise_out0_are_edges() {
    let mut p = Pit::new();
    assert!(!p.out(0, 0));
    ctl(&mut p, 0x30, 0);
    assert_eq!(p.take_edges(0), 0, "mode 0: still low");
    ctl(&mut p, 0x34, 10);
    assert_eq!(p.take_edges(10), 1, "mode 2: OUT goes high");
    ctl(&mut p, 0x34, 20);
    assert_eq!(p.take_edges(20), 0, "high to high");
    ctl(&mut p, 0x30, 30);
    assert_eq!(p.take_edges(30), 0, "falling");
    ctl(&mut p, 0x34, 40);
    count16(&mut p, CH0, 5, 40);
    assert_eq!(
        p.take_edges(40 + clk(5)),
        1,
        "the control word's, not the pulse's yet"
    );
    assert!(!p.out(0, 40 + clk(5)), "mode 2's pulse");
    ctl(&mut p, 0x34, 40 + clk(5));
    assert_eq!(p.take_edges(40 + clk(5)), 1, "the pulse cut short");
    // writes to the other channels leave OUT0 alone
    ctl(&mut p, 0x30, 100_000);
    ctl(&mut p, 0x74, 100_000);
    count16(&mut p, CH1, 2, 100_000);
    ctl(&mut p, 0xB4, 100_000);
    count16(&mut p, CH2, 2, 100_000);
    p.write(SPK, 1, 100_000);
    assert!(!p.out2(100_000 + clk(2)), "channel 2 runs");
    assert!(!p.out(1, 100_000 + clk(2)), "channel 1 runs");
    assert_eq!(p.take_edges(100_000 + clk(1000)), 0);
    assert_eq!(p.next_event(100_000), None);
}

#[test]
fn linux_quick_pit_calibrate_reads_the_msb_of_channel_2() {
    let mut p = Pit::new();
    let v = p.read(SPK, 0).unwrap();
    p.write(SPK, (v & !0x02) | 0x01, 0); // gate high, speaker off
    ctl(&mut p, 0xB0, 0);
    p.write(CH2, 0xFF, 0);
    p.write(CH2, 0xFF, 0);
    // pit_verify_msb: the LSB is read and ignored, then the MSB
    let mut msb = |t| {
        p.read(CH2, t);
        p.read(CH2, t).unwrap()
    };
    assert_eq!(msb(clk(256)), 0xFF, "0xFF00");
    assert_eq!(msb(clk(257)), 0xFE, "0xFEFF");
    assert_eq!(msb(clk(512)), 0xFE);
    assert_eq!(msb(clk(513)), 0xFD);
}
