//! The 16550A as Linux's 8250 driver meets it: probing, console output,
//! receive, and the interrupt causes in priority order.

use vmm_devices::uart::*;

const THR: u16 = BASE;
const IER: u16 = BASE + 1;
const IIR: u16 = BASE + 2;
const LCR: u16 = BASE + 3;
const MCR: u16 = BASE + 4;
const LSR: u16 = BASE + 5;
const MSR: u16 = BASE + 6;
const SCR: u16 = BASE + 7;

fn rd(u: &mut Uart, p: u16) -> u8 {
    u.read(p).unwrap()
}

fn tx_all(u: &mut Uart) -> Vec<u8> {
    let mut out = vec![0u8; 512];
    let n = u.take_tx(&mut out);
    out.truncate(n);
    out
}

#[test]
fn only_its_eight_ports() {
    let mut u = Uart::new();
    for p in [BASE - 1, BASE + 8, 0x2F8, 0x3E8] {
        assert!(
            !Uart::owns(p) && u.read(p).is_none() && !u.write(p, 1),
            "{p:#x}"
        );
    }
    for p in BASE..BASE + 8 {
        assert!(Uart::owns(p) && u.read(p).is_some() && u.write(p, 0));
    }
}

#[test]
fn reset_state() {
    let mut u = Uart::new();
    assert_eq!(
        rd(&mut u, LSR),
        0x60,
        "THR empty, transmitter empty, nothing received"
    );
    assert_eq!(rd(&mut u, IIR), 1, "no interrupt, FIFOs off");
    assert_eq!(rd(&mut u, IER), 0);
    assert_eq!(rd(&mut u, LCR), 0);
    assert_eq!(rd(&mut u, MCR), 0);
    assert_eq!(rd(&mut u, MSR), 0);
    assert!(!u.irq());
}

#[test]
fn the_scratch_register_remembers() {
    let mut u = Uart::new();
    for v in [0x55, 0xAA, 0, 0xFF] {
        u.write(SCR, v);
        assert_eq!(rd(&mut u, SCR), v);
    }
}

#[test]
fn the_divisor_latch_hides_thr_and_ier() {
    let mut u = Uart::new();
    u.write(LCR, 0x83);
    u.write(THR, 0x0C);
    u.write(IER, 0x01);
    assert_eq!(u.divisor(), 0x010C);
    assert_eq!((rd(&mut u, THR), rd(&mut u, IER)), (0x0C, 0x01));
    u.write(LCR, 0x03);
    assert_eq!(
        rd(&mut u, IER),
        0,
        "with DLAB off IER is the real one again"
    );
    assert_eq!(u.divisor(), 0x010C);
    assert!(tx_all(&mut u).is_empty(), "divisor writes are not output");
}

#[test]
fn bytes_written_reach_the_host_in_order() {
    let mut u = Uart::new();
    for b in b"hello\n" {
        u.write(THR, *b);
        assert_eq!(rd(&mut u, LSR) & 0x60, 0x60, "THR is always ready");
    }
    assert_eq!(tx_all(&mut u), b"hello\n");
    assert!(tx_all(&mut u).is_empty());
    // partial reads keep the rest
    for b in b"abcdef" {
        u.write(THR, *b);
    }
    let mut two = [0u8; 2];
    assert_eq!(u.take_tx(&mut two), 2);
    assert_eq!(&two, b"ab");
    assert_eq!(tx_all(&mut u), b"cdef");
}

#[test]
fn a_host_that_does_not_read_loses_bytes_and_it_is_counted() {
    let mut u = Uart::new();
    for i in 0..TX_BUFFER + 10 {
        u.write(THR, i as u8);
    }
    assert_eq!(u.dropped_tx, 10);
    assert_eq!(tx_all(&mut u).len(), TX_BUFFER);
}

#[test]
fn receive_sets_data_ready_and_reads_in_order() {
    let mut u = Uart::new();
    u.push_rx(b'a');
    u.push_rx(b'b');
    assert_eq!(rd(&mut u, LSR) & 1, 1);
    assert_eq!(rd(&mut u, THR), b'a');
    assert_eq!(rd(&mut u, LSR) & 1, 1);
    assert_eq!(rd(&mut u, THR), b'b');
    assert_eq!(rd(&mut u, LSR) & 1, 0);
    assert_eq!(rd(&mut u, THR), 0, "an empty receiver reads zero");
}

#[test]
fn a_full_fifo_overruns_and_the_flag_clears_on_reading_lsr() {
    let mut u = Uart::new();
    for i in 0..FIFO_DEPTH as u8 {
        u.push_rx(i);
    }
    u.push_rx(0xEE);
    assert_eq!(u.overruns, 1);
    let lsr = rd(&mut u, LSR);
    assert_eq!(lsr & 3, 3, "data ready and overrun");
    assert_eq!(
        rd(&mut u, LSR) & 2,
        0,
        "the overrun bit is cleared by the read"
    );
    for i in 0..FIFO_DEPTH as u8 {
        assert_eq!(rd(&mut u, THR), i, "the dropped byte is not in the FIFO");
    }
}

#[test]
fn fifo_detection_by_iir_bits_7_6() {
    let mut u = Uart::new();
    u.write(IER - 1 + 1, 0); // IER
    u.write(IIR, 0x01); // FCR: enable
    assert_eq!(rd(&mut u, IIR) & 0xC0, 0xC0);
    u.write(IIR, 0x00);
    assert_eq!(rd(&mut u, IIR) & 0xC0, 0);
}

#[test]
fn clearing_the_fifo_discards_received_bytes() {
    let mut u = Uart::new();
    u.push_rx(1);
    u.push_rx(2);
    u.write(IIR, 0x03); // enable + clear receive FIFO
    assert_eq!(rd(&mut u, LSR) & 1, 0);
    assert_eq!(rd(&mut u, THR), 0);
    u.push_rx(3);
    assert_eq!(rd(&mut u, THR), 3);
}

#[test]
fn received_data_interrupt() {
    let mut u = Uart::new();
    u.push_rx(b'x');
    assert!(!u.irq(), "not enabled");
    u.write(IER, 1);
    assert!(u.irq());
    assert_eq!(rd(&mut u, IIR) & 0x0F, 0b0100);
    assert_eq!(rd(&mut u, THR), b'x');
    assert!(!u.irq(), "reading the byte clears the cause");
    assert_eq!(rd(&mut u, IIR) & 0x0F, 1);
}

#[test]
fn thr_empty_interrupt_follows_the_datasheet() {
    let mut u = Uart::new();
    u.write(IER, 2);
    assert!(u.irq(), "enabling it while THR is empty raises it");
    assert_eq!(rd(&mut u, IIR) & 0x0F, 0b0010);
    assert!(
        !u.irq(),
        "reading IIR with THR-empty as the cause clears it"
    );
    assert_eq!(rd(&mut u, IIR) & 0x0F, 1);
    u.write(THR, b'a');
    assert!(u.irq(), "and writing a byte makes THR empty again at once");
    u.write(IER, 0);
    assert!(!u.irq(), "disabling the interrupt drops it");
    u.write(IER, 2);
    u.write(THR, b'b');
    u.write(THR, b'c');
    assert!(u.irq());
    // a cause with higher priority is reported first and does not clear the lower one
    u.push_rx(b'z');
    u.write(IER, 3);
    assert_eq!(rd(&mut u, IIR) & 0x0F, 0b0100);
    assert!(u.irq());
    assert_eq!(rd(&mut u, THR), b'z');
    assert_eq!(
        rd(&mut u, IIR) & 0x0F,
        0b0010,
        "the lower cause was waiting"
    );
}

#[test]
fn line_status_interrupt_has_the_highest_priority() {
    let mut u = Uart::new();
    for i in 0..=FIFO_DEPTH as u8 {
        u.push_rx(i);
    }
    u.write(IER, 0x05);
    assert_eq!(rd(&mut u, IIR) & 0x0F, 0b0110);
    assert!(u.irq());
    rd(&mut u, LSR);
    assert_eq!(
        rd(&mut u, IIR) & 0x0F,
        0b0100,
        "after LSR is read, the data cause remains"
    );
}

#[test]
fn loopback_returns_what_is_written_and_nothing_goes_out() {
    let mut u = Uart::new();
    u.write(MCR, 0x10);
    u.write(THR, 0x5A);
    assert!(tx_all(&mut u).is_empty());
    assert_eq!(rd(&mut u, LSR) & 1, 1);
    assert_eq!(rd(&mut u, THR), 0x5A);
    u.write(MCR, 0x03);
    u.write(THR, b'!');
    assert_eq!(
        tx_all(&mut u),
        b"!",
        "out of loopback the byte goes out again"
    );
}

#[test]
fn loopback_wires_modem_outputs_to_inputs_the_way_linux_probes() {
    let mut u = Uart::new();
    // Linux: MCR = LOOP | OUT2 | RTS | DTR ... then MSR & 0xF0 is checked.
    u.write(MCR, 0x10 | 0x0F);
    assert_eq!(rd(&mut u, MSR) & 0xF0, 0xF0, "CTS DSR RI DCD all high");
    u.write(MCR, 0x10);
    let m = rd(&mut u, MSR);
    assert_eq!(m & 0xF0, 0);
    assert_eq!(m & 0x0F, 0b1111, "all four changed, RI fell");
    assert_eq!(rd(&mut u, MSR) & 0x0F, 0, "the delta bits clear on reading");
    u.write(MCR, 0x10 | 0x02);
    assert_eq!(rd(&mut u, MSR), 0x10 | 0x01, "CTS up with its delta");
    // leaving loopback drops the inputs
    u.write(MCR, 0);
    assert_eq!(rd(&mut u, MSR) & 0xF0, 0);
}

#[test]
fn modem_status_interrupt() {
    let mut u = Uart::new();
    u.write(IER, 8);
    assert!(!u.irq());
    u.write(MCR, 0x12);
    assert!(u.irq());
    assert_eq!(rd(&mut u, IIR) & 0x0F, 0, "cause 0000: modem status");
    rd(&mut u, MSR);
    assert!(!u.irq());
}

#[test]
fn unsupported_features_are_counted_not_silently_accepted() {
    let mut u = Uart::new();
    u.write(IIR, 0xC1); // FIFO trigger level 14
    u.write(MCR, 0x20); // auto flow control
    u.write(LSR, 0);
    u.write(MSR, 0);
    assert_eq!(u.unsupported, 4);
}

#[test]
fn reads_of_write_only_or_hidden_registers_are_stable() {
    let mut u = Uart::new();
    u.write(LCR, 0x80);
    u.write(THR, 0x34);
    u.write(IER, 0x12);
    assert_eq!(u.divisor(), 0x1234);
    assert_eq!(rd(&mut u, IIR) & 0x0F, 1, "IIR is not affected by DLAB");
    u.write(LCR, 0x1B);
    assert_eq!(rd(&mut u, LCR), 0x1B);
}

#[test]
fn modem_status_does_not_interrupt_unless_enabled() {
    let mut u = Uart::new();
    u.write(MCR, 0x12);
    assert!(!u.irq(), "IER bit 3 is off");
    u.write(IER, 8);
    assert!(u.irq(), "the change was waiting");
}

#[test]
fn rewriting_ier_does_not_raise_thre_again() {
    let mut u = Uart::new();
    u.write(IER, 2);
    assert_eq!(rd(&mut u, IIR) & 0x0F, 0b0010);
    u.write(IER, 2);
    assert!(
        !u.irq(),
        "only enabling it (a 0 to 1 change) raises the cause"
    );
    u.write(IER, 0);
    u.write(IER, 2);
    assert!(u.irq());
}

#[test]
fn ier_has_four_bits() {
    let mut u = Uart::new();
    u.write(IER, 0xFF);
    assert_eq!(rd(&mut u, IER), 0x0F);
}

#[test]
fn ri_has_no_delta_when_it_rises_only_when_it_falls() {
    let mut u = Uart::new();
    u.write(MCR, 0x10 | 0x04); // loopback, OUT1 -> RI
    assert_eq!(rd(&mut u, MSR), 0x40, "RI is up and no trailing-edge flag");
    u.write(MCR, 0x10);
    assert_eq!(
        rd(&mut u, MSR),
        0x04,
        "RI fell: the trailing-edge flag, nothing else"
    );
}
