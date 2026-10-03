//! virtio-console (the agent channel) driven the way a guest driver does it,
//! straight against the virtio transport: register handshake, descriptors in
//! guest RAM, notification, the device's answer in the used ring.

use std::collections::VecDeque;

use vmm_devices::pci::{CLASS_CODE, COMMAND, DEVICE_ID};
use vmm_devices::virtio::*;
use vmm_devices::virtio_console::{ConsoleBackend, VirtioConsole, CLASS, DEVICE_TYPE};

struct Ram(Vec<u8>);

impl GuestMemory for Ram {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        let Some(end) = gpa.checked_add(buf.len() as u64) else {
            return false;
        };
        match self.0.get(gpa as usize..end as usize) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        let Some(end) = gpa.checked_add(data.len() as u64) else {
            return false;
        };
        match self.0.get_mut(gpa as usize..end as usize) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

/// RAM below 0x40000, and "RAM everywhere" above it (reads give zeros, writes vanish): for address-overflow cases.
struct Wide(Ram);

impl GuestMemory for Wide {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        if gpa < 0x40000 {
            self.0.read(gpa, buf)
        } else {
            buf.fill(0);
            true
        }
    }
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        if gpa < 0x40000 {
            self.0.write(gpa, data)
        } else {
            true
        }
    }
}

#[derive(Default)]
struct Host {
    from_guest: Vec<u8>,
    for_guest: VecDeque<u8>,
    /// Report more bytes than asked for.
    greedy: bool,
}

impl ConsoleBackend for Host {
    fn write(&mut self, data: &[u8]) {
        self.from_guest.extend_from_slice(data);
    }
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut n = 0;
        while n < buf.len() {
            let Some(b) = self.for_guest.pop_front() else {
                break;
            };
            buf[n] = b;
            n += 1;
        }
        if self.greedy {
            n + 5
        } else {
            n
        }
    }
}

struct Q {
    n: u16,
    desc: u64,
    avail: u64,
    used: u64,
    avail_idx: u16,
    next_desc: u16,
    seen_used: u16,
}

impl Q {
    fn new(base: u64, n: u16) -> Self {
        Self {
            n,
            desc: base,
            avail: base + 0x1000,
            used: base + 0x2000,
            avail_idx: 0,
            next_desc: 0,
            seen_used: 0,
        }
    }

    fn add(&mut self, ram: &mut Ram, bufs: &[(u64, u32, bool)]) -> u16 {
        let head = self.next_desc;
        for (i, &(addr, len, write)) in bufs.iter().enumerate() {
            let idx = self.next_desc;
            self.next_desc = (self.next_desc + 1) % self.n;
            let last = i + 1 == bufs.len();
            let flags: u16 = (if write { 2 } else { 0 }) | (if last { 0 } else { 1 });
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&addr.to_le_bytes());
            d[8..12].copy_from_slice(&len.to_le_bytes());
            d[12..14].copy_from_slice(&flags.to_le_bytes());
            d[14..16].copy_from_slice(&self.next_desc.to_le_bytes());
            assert!(ram.write(self.desc + 16 * u64::from(idx), &d));
        }
        let slot = self.avail + 4 + 2 * u64::from(self.avail_idx % self.n);
        assert!(ram.write(slot, &head.to_le_bytes()));
        self.avail_idx = self.avail_idx.wrapping_add(1);
        assert!(ram.write(self.avail + 2, &self.avail_idx.to_le_bytes()));
        head
    }

    fn take_used(&mut self, ram: &Ram) -> Vec<(u32, u32)> {
        let mut idx = [0u8; 2];
        assert!(ram.read(self.used + 2, &mut idx));
        let idx = u16::from_le_bytes(idx);
        let mut out = Vec::new();
        while self.seen_used != idx {
            let mut e = [0u8; 8];
            assert!(ram.read(
                self.used + 4 + 8 * u64::from(self.seen_used % self.n),
                &mut e
            ));
            out.push((
                u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            ));
            self.seen_used = self.seen_used.wrapping_add(1);
        }
        out
    }
}

fn w(c: &mut VirtioConsole, off: u64, size: u8, v: u32) {
    c.t.mmio_write(off, size, v);
}

fn r(c: &mut VirtioConsole, off: u64, size: u8) -> u32 {
    c.t.mmio_read(off, size)
}

/// Runs the handshake with the given accepted features and enables the queues.
fn bring_up(c: &mut VirtioConsole, rx: &Q, tx: &Q, accept: u64) -> u8 {
    c.t.cfg.write(COMMAND, 2, 0x0006);
    w(c, 0x14, 1, 0);
    w(c, 0x14, 1, 1);
    w(c, 0x14, 1, 3);
    w(c, 0x08, 4, 0);
    w(c, 0x0C, 4, accept as u32);
    w(c, 0x08, 4, 1);
    w(c, 0x0C, 4, (accept >> 32) as u32);
    w(c, 0x14, 1, 0xB);
    for (i, q) in [rx, tx].into_iter().enumerate() {
        w(c, 0x16, 2, i as u32);
        w(c, 0x18, 2, u32::from(q.n));
        w(c, 0x20, 4, q.desc as u32);
        w(c, 0x28, 4, q.avail as u32);
        w(c, 0x30, 4, q.used as u32);
        w(c, 0x1C, 2, 1);
    }
    let s = r(c, 0x14, 1) as u8;
    if s & 8 != 0 {
        w(c, 0x14, 1, u32::from(s | 4));
    }
    r(c, 0x14, 1) as u8
}

const DATA: u64 = 0x8000;
const MORE: u64 = 0x9000;

struct Rig {
    c: VirtioConsole,
    ram: Ram,
    host: Host,
    rx: Q,
    tx: Q,
}

fn rig() -> Rig {
    let mut c = VirtioConsole::new(10);
    let rx = Q::new(0x1000, 8);
    let tx = Q::new(0x4000, 8);
    assert_eq!(bring_up(&mut c, &rx, &tx, F_VERSION_1), 0xF);
    Rig {
        c,
        ram: Ram(vec![0; 0x40000]),
        host: Host::default(),
        rx,
        tx,
    }
}

impl Rig {
    fn kick(&mut self, q: u64) -> (u32, u32) {
        w(&mut self.c, 0x3000 + 4 * q, 4, q as u32);
        self.c.service(&mut self.ram, &mut self.host)
    }

    fn put(&mut self, at: u64, data: &[u8]) {
        assert!(self.ram.write(at, data));
    }

    fn get(&self, at: u64, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        assert!(self.ram.read(at, &mut v));
        v
    }
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(7) ^ seed).collect()
}

#[test]
fn it_identifies_as_a_modern_console_with_two_queues_and_no_features() {
    let mut c = VirtioConsole::new(10);
    assert_eq!(DEVICE_TYPE, 3);
    assert_eq!(c.t.cfg.read(DEVICE_ID, 2), 0x1043);
    assert_eq!(c.t.cfg.read(CLASS_CODE, 4) & 0xFF_FFFF, CLASS);
    assert_eq!(CLASS, 0x07_8000);
    assert_eq!(c.t.cfg.interrupt_line(), 10);
    assert_eq!(r(&mut c, 0x12, 2), 2, "num_queues");
    w(&mut c, 0x00, 4, 0);
    assert_eq!(r(&mut c, 0x04, 4), 0, "no device features");
    w(&mut c, 0x00, 4, 1);
    assert_eq!(r(&mut c, 0x04, 4), 1, "VERSION_1");
    assert_eq!(r(&mut c, 0x2000, 4), 0, "empty device configuration");
    // a feature it did not offer is refused
    let rx = Q::new(0x1000, 8);
    let tx = Q::new(0x4000, 8);
    let s = bring_up(&mut c, &rx, &tx, F_VERSION_1 | 1);
    assert_eq!(s & 8, 0, "FEATURES_OK refused");
    assert_eq!(s & 4, 0);
}

#[test]
fn bytes_sent_by_the_guest_reach_the_host_in_order_across_descriptors_and_chains() {
    let mut t = rig();
    let a = pattern(700, 1);
    let b = pattern(5, 2);
    let c = pattern(300, 3);
    t.put(DATA, &a);
    t.put(DATA + 0x2000, &b);
    t.put(MORE, &c);
    let h1 =
        t.tx.add(&mut t.ram, &[(DATA, 700, false), (DATA + 0x2000, 5, false)]);
    let h2 = t.tx.add(&mut t.ram, &[(MORE, 300, false)]);
    assert_eq!(t.kick(1), (2, 0));
    let mut want = a.clone();
    want.extend(&b);
    want.extend(&c);
    assert_eq!(t.host.from_guest, want);
    assert_eq!(t.c.bytes_out, 1005);
    assert_eq!(
        t.tx.take_used(&t.ram),
        [(u32::from(h1), 0), (u32::from(h2), 0)]
    );
    assert!(t.c.t.irq());
    assert_eq!(t.c.tx_errors, 0);
}

#[test]
fn a_zero_length_and_an_exactly_chunk_sized_buffer_are_handled() {
    let mut t = rig();
    let a = pattern(256, 9);
    t.put(DATA, &a);
    t.tx.add(&mut t.ram, &[(DATA, 0, false), (DATA, 256, false)]);
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(t.host.from_guest, a);
    assert_eq!(t.c.bytes_out, 256);
}

#[test]
fn a_transmit_chain_stops_at_a_device_writable_buffer_keeping_what_came_before() {
    let mut t = rig();
    t.put(DATA, &pattern(10, 4));
    t.put(MORE, &pattern(10, 5));
    t.tx.add(
        &mut t.ram,
        &[
            (DATA, 10, false),
            (DATA + 0x100, 10, true),
            (MORE, 10, false),
        ],
    );
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(
        t.host.from_guest,
        pattern(10, 4),
        "the stream already delivered it"
    );
    assert_eq!(t.c.tx_errors, 1);
    assert_eq!(
        t.tx.take_used(&t.ram).len(),
        1,
        "the chain is still returned"
    );
}

#[test]
fn unreadable_memory_ends_a_transmit_chain_after_the_bytes_before_it() {
    let mut t = rig();
    t.put(DATA, &pattern(300, 6));
    // the second buffer lies beyond the end of RAM
    t.tx.add(
        &mut t.ram,
        &[(DATA, 300, false), (0x10_0000, 16, false), (DATA, 4, false)],
    );
    assert_eq!(t.kick(1), (1, 0));
    assert_eq!(t.host.from_guest, pattern(300, 6));
    assert_eq!(t.c.tx_errors, 1);
}

#[test]
fn a_transmit_buffer_that_runs_past_the_end_of_the_address_space_is_an_error() {
    let mut c = VirtioConsole::new(10);
    let rx = Q::new(0x1000, 8);
    let mut tx = Q::new(0x4000, 8);
    assert_eq!(bring_up(&mut c, &rx, &tx, F_VERSION_1), 0xF);
    let mut mem = Wide(Ram(vec![0; 0x40000]));
    tx.add(&mut mem.0, &[(u64::MAX - 100, 300, false)]);
    let mut host = Host::default();
    w(&mut c, 0x3004, 4, 1);
    assert_eq!(c.service(&mut mem, &mut host), (1, 0));
    assert_eq!(
        host.from_guest.len(),
        256,
        "the first chunk went, the second would overflow"
    );
    assert_eq!(c.tx_errors, 1);
}

#[test]
fn host_bytes_fill_receive_buffers_exactly_and_spill_over_descriptors() {
    let mut t = rig();
    let data = pattern(600, 7);
    t.host.for_guest.extend(&data);
    let head =
        t.rx.add(&mut t.ram, &[(DATA, 200, true), (MORE, 1000, true)]);
    assert_eq!(t.kick(0), (0, 1));
    assert_eq!(t.get(DATA, 200), data[..200]);
    assert_eq!(t.get(MORE, 400), data[200..]);
    assert_eq!(t.rx.take_used(&t.ram), [(u32::from(head), 600)]);
    assert_eq!(t.c.bytes_in, 600);
    assert!(t.host.for_guest.is_empty());
    assert!(t.c.t.irq());
}

#[test]
fn a_small_buffer_takes_only_what_fits_and_the_rest_stays_with_the_host() {
    let mut t = rig();
    let data = pattern(300, 8);
    t.host.for_guest.extend(&data);
    t.rx.add(&mut t.ram, &[(DATA, 100, true)]);
    t.kick(0);
    assert_eq!(t.get(DATA, 100), data[..100]);
    assert_eq!(
        t.host.for_guest.len(),
        200,
        "nothing was taken from the host beyond the buffer"
    );
    t.rx.add(&mut t.ram, &[(MORE, 500, true)]);
    t.kick(0);
    assert_eq!(t.get(MORE, 200), data[100..]);
    assert_eq!(t.rx.take_used(&t.ram), [(0, 100), (1, 200)]);
}

#[test]
fn a_posted_buffer_waits_for_data_and_keeps_its_place() {
    let mut t = rig();
    t.rx.add(&mut t.ram, &[(DATA, 64, true)]);
    t.rx.add(&mut t.ram, &[(MORE, 64, true)]);
    assert_eq!(t.kick(0), (0, 0), "nothing to deliver");
    assert!(t.rx.take_used(&t.ram).is_empty());
    t.host.for_guest.extend(pattern(10, 1));
    assert_eq!(t.kick(0), (0, 1));
    t.host.for_guest.extend(pattern(11, 2));
    assert_eq!(t.kick(0), (0, 1));
    assert_eq!(
        t.rx.take_used(&t.ram),
        [(0, 10), (1, 11)],
        "first buffer first"
    );
    assert_eq!(t.get(DATA, 10), pattern(10, 1));
    assert_eq!(t.get(MORE, 11), pattern(11, 2));
}

#[test]
fn receive_buffers_the_device_may_not_write_or_that_have_no_room_are_returned_empty() {
    let mut t = rig();
    t.host.for_guest.extend(pattern(40, 3));
    t.put(DATA, &[0x55; 64]);
    t.rx.add(&mut t.ram, &[(DATA, 64, false)]);
    t.rx.add(
        &mut t.ram,
        &[(DATA + 0x100, 32, true), (DATA + 0x200, 32, false)],
    );
    t.rx.add(&mut t.ram, &[(MORE, 0, true)]);
    assert_eq!(t.kick(0), (0, 3));
    assert_eq!(
        t.get(DATA, 64),
        [0x55; 64],
        "a read-only buffer is left alone"
    );
    assert_eq!(
        t.get(DATA + 0x100, 32),
        [0; 32],
        "so is the writable part of a mixed chain"
    );
    assert_eq!(t.c.rx_bad, 3);
    assert_eq!(t.host.for_guest.len(), 40, "no data was spent on them");
    // heads 0, 1 and 3 (the second chain used descriptors 1 and 2)
    assert_eq!(t.rx.take_used(&t.ram), [(0, 0), (1, 0), (3, 0)]);
    // a good buffer afterwards works
    t.rx.add(&mut t.ram, &[(MORE, 64, true)]);
    assert_eq!(t.kick(0), (0, 1));
    assert_eq!(t.get(MORE, 40), pattern(40, 3));
}

#[test]
fn unwritable_guest_memory_loses_the_bytes_taken_and_says_so() {
    let mut t = rig();
    t.host.for_guest.extend(pattern(1000, 1));
    t.rx.add(&mut t.ram, &[(0x10_0000, 1000, true)]);
    assert_eq!(t.kick(0), (0, 1));
    assert_eq!(
        t.c.rx_lost, 256,
        "one chunk was taken and could not be stored"
    );
    assert_eq!(t.host.for_guest.len(), 744, "no more than that");
    assert_eq!(t.c.bytes_in, 0);
    assert_eq!(t.rx.take_used(&t.ram), [(0, 0)]);
    // an address that overflows
    let mut t = rig();
    t.host.for_guest.extend(pattern(10, 1));
    t.rx.add(&mut t.ram, &[(u64::MAX - 3, 10, true)]);
    assert_eq!(t.kick(0), (0, 1));
    assert_eq!(t.c.rx_lost, 10);
}

#[test]
fn a_backend_that_reports_too_much_is_clamped_to_the_buffer() {
    let mut t = rig();
    t.host.greedy = true;
    t.host.for_guest.extend(pattern(1000, 2));
    t.rx.add(&mut t.ram, &[(DATA, 10, true)]);
    t.kick(0);
    assert_eq!(t.rx.take_used(&t.ram), [(0, 10)]);
    assert_eq!(t.c.bytes_in, 10);
}

#[test]
fn nothing_moves_without_bus_mastering_and_everything_does_once_it_is_on() {
    let mut t = rig();
    t.c.t.cfg.write(COMMAND, 2, 0x0002);
    t.put(DATA, &pattern(8, 1));
    t.tx.add(&mut t.ram, &[(DATA, 8, false)]);
    t.host.for_guest.extend(pattern(8, 2));
    t.rx.add(&mut t.ram, &[(MORE, 8, true)]);
    assert_eq!(t.kick(1), (0, 0));
    assert!(t.host.from_guest.is_empty());
    assert_eq!(t.host.for_guest.len(), 8);
    t.c.t.cfg.write(COMMAND, 2, 0x0006);
    assert_eq!(t.kick(1), (1, 1));
    assert_eq!(t.host.from_guest, pattern(8, 1));
    assert_eq!(t.get(MORE, 8), pattern(8, 2));
}

#[test]
fn service_consumes_notifications_and_the_isr_clears_on_read() {
    let mut t = rig();
    t.kick(1);
    assert_eq!(t.c.t.take_kicks(), 0);
    t.put(DATA, &[1]);
    t.tx.add(&mut t.ram, &[(DATA, 1, false)]);
    t.kick(1);
    assert!(t.c.t.irq());
    assert_eq!(r(&mut t.c, 0x1000, 1), 1);
    assert!(!t.c.t.irq());
}

#[test]
fn a_broken_chain_needs_reset_and_stops_the_service() {
    let mut t = rig();
    let mut d = [0u8; 16];
    d[8..12].copy_from_slice(&16u32.to_le_bytes());
    d[12..14].copy_from_slice(&1u16.to_le_bytes());
    assert!(t.ram.write(t.tx.desc, &d)); // descriptor 0 links to itself
    assert!(t.ram.write(t.tx.avail + 4, &0u16.to_le_bytes()));
    assert!(t.ram.write(t.tx.avail + 2, &1u16.to_le_bytes()));
    assert_eq!(t.kick(1), (0, 0));
    assert_ne!(t.c.t.status() & STATUS_NEEDS_RESET, 0);
    assert!(!t.c.t.driver_ok());
}

#[test]
fn both_directions_work_in_one_service_call() {
    let mut t = rig();
    t.put(DATA, &pattern(32, 1));
    t.tx.add(&mut t.ram, &[(DATA, 32, false)]);
    t.rx.add(&mut t.ram, &[(MORE, 64, true)]);
    t.host.for_guest.extend(pattern(20, 2));
    assert_eq!(t.kick(1), (1, 1));
    assert_eq!(t.host.from_guest, pattern(32, 1));
    assert_eq!(t.get(MORE, 20), pattern(20, 2));
}
