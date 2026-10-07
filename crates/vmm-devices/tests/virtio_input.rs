//! virtio-input (the keyboard and the tablet) driven the way a guest driver
//! does it, straight against the device: register handshake, the device
//! configuration queried by `select` and `subsel` as Linux's virtio_input
//! probe does, descriptors in guest RAM, the events in the used ring.

use vmm_devices::ioapic;
use vmm_devices::lapic::{self, reg};
use vmm_devices::machine::{pci_pin, slot, Machine, PCI_ADDRESS, PCI_DATA};
use vmm_devices::pci::{CAPABILITIES, CLASS_CODE, COMMAND, DEVICE_ID, SUBSYSTEM_VENDOR};
use vmm_devices::virtio::*;
use vmm_devices::virtio_input::*;

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

/// Runs the handshake with the given accepted features and enables both queues.
fn bring_up(d: &mut VirtioInput, ev: &Q, st: &Q, accept: u64) -> u8 {
    handshake(d, ev, st, accept, true)
}

/// As `bring_up`; `reset`: whether the driver writes 0 to the status register first.
fn handshake(d: &mut VirtioInput, ev: &Q, st: &Q, accept: u64, reset: bool) -> u8 {
    d.t.cfg.write(COMMAND, 2, 0x0006);
    if reset {
        d.mmio_write(0x14, 1, 0);
    }
    d.mmio_write(0x14, 1, 1);
    d.mmio_write(0x14, 1, 3);
    d.mmio_write(0x08, 4, 0);
    d.mmio_write(0x0C, 4, accept as u32);
    d.mmio_write(0x08, 4, 1);
    d.mmio_write(0x0C, 4, (accept >> 32) as u32);
    d.mmio_write(0x14, 1, 0xB);
    for (i, q) in [ev, st].into_iter().enumerate() {
        d.mmio_write(0x16, 2, i as u32);
        d.mmio_write(0x18, 2, u32::from(q.n));
        d.mmio_write(0x20, 4, q.desc as u32);
        d.mmio_write(0x28, 4, q.avail as u32);
        d.mmio_write(0x30, 4, q.used as u32);
        d.mmio_write(0x1C, 2, 1);
    }
    let s = d.t.mmio_read(0x14, 1) as u8;
    if s & 8 != 0 {
        d.mmio_write(0x14, 1, u32::from(s | 4));
    }
    d.t.mmio_read(0x14, 1) as u8
}

/// Selects in the device configuration, byte by byte as Linux does, and reads `size` and the union.
fn query(d: &mut VirtioInput, select: u8, subsel: u8) -> (u8, Vec<u8>) {
    d.mmio_write(0x2000, 1, u32::from(select));
    d.mmio_write(0x2001, 1, u32::from(subsel));
    let size = d.t.mmio_read(0x2002, 1) as u8;
    let u = (0..128)
        .map(|i| d.t.mmio_read(0x2008 + i, 1) as u8)
        .collect();
    (size, u)
}

/// The union an answer of `bytes` should show: those bytes, then zeros.
fn union(bytes: &[u8]) -> Vec<u8> {
    let mut u = bytes.to_vec();
    u.resize(128, 0);
    u
}

const KEY_ESC: u16 = 1;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_KPDOT: u16 = 83;
const KEY_102ND: u16 = 86;
const KEY_F12: u16 = 88;
const KEY_KPENTER: u16 = 96;
const KEY_RIGHTALT: u16 = 100;
const KEY_LINEFEED: u16 = 101;
const KEY_HOME: u16 = 102;
const KEY_DELETE: u16 = 111;
const KEY_PAUSE: u16 = 119;
const KEY_LEFTMETA: u16 = 125;
const KEY_COMPOSE: u16 = 127;
const ABS_Z: u8 = 2;
const EV_REL: u8 = 2;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
/// Event buffers, 16 bytes apart (8 of them are the event, 8 must stay untouched).
const BUF: u64 = 0x8000;
const STS: u64 = 0x10000;
const FILL: u8 = 0xAA;

struct Rig {
    d: VirtioInput,
    ram: Ram,
    ev: Q,
    st: Q,
}

fn rig_of(mut d: VirtioInput) -> Rig {
    let ev = Q::new(0x1000, 128);
    let st = Q::new(0x4000, 16);
    assert_eq!(bring_up(&mut d, &ev, &st, F_VERSION_1), 0xF);
    Rig {
        d,
        ram: Ram(vec![0; 0x40000]),
        ev,
        st,
    }
}

fn kbd() -> Rig {
    rig_of(VirtioInput::keyboard(14))
}

fn tablet() -> Rig {
    rig_of(VirtioInput::tablet(12, WIDTH, HEIGHT))
}

type Ev = (u16, u16, u32);
const SYN: Ev = (0, 0, 0);

fn decode(b: &[u8]) -> Ev {
    (
        u16::from_le_bytes([b[0], b[1]]),
        u16::from_le_bytes([b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
    )
}

impl Rig {
    /// Posts `k` buffers of eight bytes, the next free descriptors, at `BUF + 16 * descriptor`.
    fn post(&mut self, k: usize) {
        for _ in 0..k {
            let at = BUF + 16 * u64::from(self.ev.next_desc);
            assert!(self.ram.write(at, &[FILL; 16]));
            self.ev.add(&mut self.ram, &[(at, 8, true)]);
        }
    }

    fn service(&mut self) -> (u32, u32) {
        self.d.mmio_write(0x3000, 4, 0);
        self.d.service(&mut self.ram)
    }

    /// The events the device returned since the last look: each buffer got eight bytes and no more.
    fn received(&mut self) -> Vec<Ev> {
        let used = self.ev.take_used(&self.ram);
        used.iter()
            .map(|&(head, len)| {
                assert_eq!(len, 8, "buffer {head}");
                let mut b = [0u8; 16];
                assert!(self.ram.read(BUF + 16 * u64::from(head), &mut b));
                assert_eq!(b[8..], [FILL; 8], "nothing past the event");
                decode(&b)
            })
            .collect()
    }

    /// Sends one event on the status queue in a buffer of eight bytes at `at`.
    fn status(&mut self, at: u64, e: Ev) {
        let mut b = [0u8; 8];
        b[..2].copy_from_slice(&e.0.to_le_bytes());
        b[2..4].copy_from_slice(&e.1.to_le_bytes());
        b[4..].copy_from_slice(&e.2.to_le_bytes());
        assert!(self.ram.write(at, &b));
        self.st.add(&mut self.ram, &[(at, 8, false)]);
    }
}

// ------------------------------------------------------------ identification

#[test]
fn it_identifies_as_a_modern_input_device_with_two_queues_and_no_features() {
    assert_eq!((DEVICE_TYPE, CLASS, CONFIG_LEN), (18, 0x09_8000, 136));
    for (mut d, line) in [
        (VirtioInput::keyboard(14), 14),
        (VirtioInput::tablet(12, WIDTH, HEIGHT), 12),
    ] {
        assert_eq!(d.t.cfg.read(DEVICE_ID, 2), 0x1052);
        assert_eq!(d.t.cfg.read(CLASS_CODE, 4) & 0xFF_FFFF, 0x09_8000);
        assert_eq!(d.t.cfg.read(SUBSYSTEM_VENDOR, 4), 0x0012_1AF4);
        assert_eq!(d.t.cfg.interrupt_line(), line);
        assert_eq!(d.t.mmio_read(0x12, 2), 2, "num_queues");
        d.mmio_write(0x00, 4, 0);
        assert_eq!(d.t.mmio_read(0x04, 4), 0, "no device features");
        d.mmio_write(0x00, 4, 1);
        assert_eq!(d.t.mmio_read(0x04, 4), 1, "VERSION_1");
        // the device configuration capability is 136 bytes long
        let mut p = d.t.cfg.read(CAPABILITIES, 1) as usize;
        let mut caps = Vec::new();
        while p != 0 {
            caps.push((
                d.t.cfg.read(p + 3, 1),
                d.t.cfg.read(p + 8, 4),
                d.t.cfg.read(p + 12, 4),
            ));
            p = d.t.cfg.read(p + 1, 1) as usize;
        }
        assert_eq!(
            caps,
            [
                (1, 0, 0x38),
                (2, 0x3000, 8),
                (3, 0x1000, 4),
                (4, 0x2000, 136)
            ]
        );
        // a feature it did not offer is refused
        let ev = Q::new(0x1000, 8);
        let st = Q::new(0x4000, 8);
        let s = bring_up(&mut d, &ev, &st, F_VERSION_1 | 1);
        assert_eq!(s & 8, 0, "FEATURES_OK refused");
        assert_eq!(s & 4, 0);
    }
}

#[test]
fn the_configuration_starts_unset_and_empty() {
    let mut d = VirtioInput::tablet(12, WIDTH, HEIGHT);
    for i in 0..CONFIG_LEN as u64 + 8 {
        assert_eq!(d.t.mmio_read(0x2000 + i, 1), 0, "byte {i}");
    }
    assert_eq!(query(&mut d, cfg::UNSET, 0), (0, union(&[])));
}

// ------------------------------------------------------------ configuration

#[test]
fn name_serial_and_ids_answer_with_their_sizes() {
    let mut k = VirtioInput::keyboard(14);
    assert_eq!(
        query(&mut k, cfg::ID_NAME, 0),
        (14, union(b"NANOX keyboard"))
    );
    assert_eq!(
        query(&mut k, cfg::ID_SERIAL, 0),
        (11, union(b"nanox-kbd-0"))
    );
    assert_eq!(
        query(&mut k, cfg::ID_DEVIDS, 0),
        (8, union(&[6, 0, 0xF4, 0x1A, 1, 0, 0x00, 0x01]))
    );
    let mut t = VirtioInput::tablet(12, WIDTH, HEIGHT);
    assert_eq!(query(&mut t, cfg::ID_NAME, 0), (12, union(b"NANOX tablet")));
    assert_eq!(
        query(&mut t, cfg::ID_SERIAL, 0),
        (14, union(b"nanox-tablet-0"))
    );
    assert_eq!(
        query(&mut t, cfg::ID_DEVIDS, 0),
        (8, union(&[6, 0, 0xF4, 0x1A, 2, 0, 0x00, 0x01]))
    );
    assert_eq!(
        (BUS_VIRTUAL, VERSION, VENDOR),
        (6, 0x0100, 0x1AF4),
        "BUS_VIRTUAL, version 1.0, the virtio vendor"
    );
    // select and subsel read back; the reserved bytes are zero
    k.mmio_write(0x2000, 1, u32::from(cfg::ID_NAME));
    assert_eq!(k.t.mmio_read(0x2000, 4), 0x000E_0001);
    assert_eq!(k.t.mmio_read(0x2004, 4), 0);
}

#[test]
fn the_ids_need_subsel_zero() {
    let mut k = VirtioInput::keyboard(14);
    for select in [cfg::ID_NAME, cfg::ID_SERIAL, cfg::ID_DEVIDS] {
        for subsel in [1u8, 0x80, 0xFF] {
            assert_eq!(
                query(&mut k, select, subsel),
                (0, union(&[])),
                "{select} {subsel}"
            );
        }
    }
}

#[test]
fn a_new_selection_leaves_nothing_of_the_last_one() {
    let mut k = VirtioInput::keyboard(14);
    assert_eq!(query(&mut k, cfg::ID_NAME, 0).0, 14);
    assert_eq!(query(&mut k, cfg::EV_BITS, 0x14), (1, union(&[0x03])));
    assert_eq!(query(&mut k, cfg::UNSET, 0), (0, union(&[])));
    assert_eq!(k.t.mmio_read(0x2001, 1), 0);
}

#[test]
fn the_keyboard_has_its_keys_autorepeat_and_three_leds() {
    let mut k = VirtioInput::keyboard(14);
    let mut keys = vec![0xFE];
    keys.extend([0xFF; 9]);
    keys.extend([0xCF, 0x01, 0xDF, 0xFF, 0x80, 0xE0]);
    assert_eq!(query(&mut k, cfg::EV_BITS, 1), (16, union(&keys)));
    assert_eq!(
        query(&mut k, cfg::EV_BITS, 0x14),
        (1, union(&[0x03])),
        "EV_REP: REP_DELAY and REP_PERIOD"
    );
    assert_eq!(
        query(&mut k, cfg::EV_BITS, 0x11),
        (1, union(&[0x07])),
        "EV_LED: Num, Caps and Scroll Lock"
    );
    for kind in [0u8, EV_REL, 3, 4, 0x12, 0x15, 0x1F, 0xFF] {
        assert_eq!(query(&mut k, cfg::EV_BITS, kind), (0, union(&[])), "{kind}");
    }
    assert_eq!(
        (EV_SYN, EV_KEY, EV_ABS, EV_LED, EV_REP, SYN_REPORT),
        (0, 1, 3, 0x11, 0x14, 0)
    );
}

#[test]
fn the_tablet_has_three_buttons_and_two_axes() {
    let mut t = VirtioInput::tablet(12, WIDTH, HEIGHT);
    let mut buttons = vec![0; 34];
    buttons.push(0x07);
    assert_eq!(query(&mut t, cfg::EV_BITS, 1), (35, union(&buttons)));
    assert_eq!(query(&mut t, cfg::EV_BITS, 3), (1, union(&[0x03])));
    for kind in [0u8, EV_REL, 0x11, 0x14, 0xFF] {
        assert_eq!(query(&mut t, cfg::EV_BITS, kind), (0, union(&[])), "{kind}");
    }
    assert_eq!(
        (BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, ABS_X, ABS_Y),
        (0x110, 0x111, 0x112, 0, 1)
    );
}

#[test]
fn abs_info_gives_each_axis_its_range_and_nothing_else() {
    let mut t = VirtioInput::tablet(12, WIDTH, HEIGHT);
    for (axis, max) in [(0u8, WIDTH - 1), (1, HEIGHT - 1)] {
        let (size, u) = query(&mut t, cfg::ABS_INFO, axis);
        assert_eq!(size, 20, "axis {axis}");
        let mut want = vec![0; 4];
        want.extend(max.to_le_bytes());
        assert_eq!(u, union(&want), "min, max, fuzz, flat, res");
        // as Linux reads them: five le32 fields
        let fields: Vec<u32> = (0..5).map(|i| t.t.mmio_read(0x2008 + 4 * i, 4)).collect();
        assert_eq!(fields, [0, max, 0, 0, 0]);
    }
    for axis in [ABS_Z, 0x2F, 0x35, 0x80, 0xFF] {
        assert_eq!(
            query(&mut t, cfg::ABS_INFO, axis),
            (0, union(&[])),
            "{axis}"
        );
    }
    let mut k = VirtioInput::keyboard(14);
    for axis in [0u8, 1] {
        assert_eq!(query(&mut k, cfg::ABS_INFO, axis), (0, union(&[])));
    }
}

#[test]
fn neither_device_has_input_properties_and_unknown_selects_answer_nothing() {
    let mut k = VirtioInput::keyboard(14);
    let mut t = VirtioInput::tablet(12, WIDTH, HEIGHT);
    assert_eq!(
        [
            cfg::UNSET,
            cfg::ID_NAME,
            cfg::ID_SERIAL,
            cfg::ID_DEVIDS,
            cfg::PROP_BITS,
            cfg::EV_BITS,
            cfg::ABS_INFO
        ],
        [0, 1, 2, 3, 0x10, 0x11, 0x12],
        "VIRTIO_INPUT_CFG_*"
    );
    for d in [&mut k, &mut t] {
        for subsel in [0u8, 1, 3] {
            assert_eq!(query(d, 0x10, subsel), (0, union(&[])), "PROP_BITS");
        }
        for select in [4u8, 5, 0x0F, 0x13, 0x20, 0xFF] {
            for subsel in [0u8, 1] {
                assert_eq!(query(d, select, subsel), (0, union(&[])), "{select}");
            }
        }
    }
}

#[test]
fn select_and_subsel_take_byte_word_and_dword_writes_and_nothing_else_is_writable() {
    let mut t = VirtioInput::tablet(12, WIDTH, HEIGHT);
    // one 16-bit write: ABS_INFO for ABS_Y
    t.mmio_write(0x2000, 2, 0x0112);
    assert_eq!(t.t.mmio_read(0x2000, 4), 0x0014_0112);
    assert_eq!(t.t.mmio_read(0x200C, 4), HEIGHT - 1);
    // one 32-bit write: EV_BITS for EV_ABS; the upper bytes (size, reserved) are not written
    t.mmio_write(0x2000, 4, 0xAABB_0311);
    assert_eq!(t.t.mmio_read(0x2000, 4), 0x0001_0311);
    assert_eq!(t.t.mmio_read(0x2008, 1), 0x03);
    // subsel alone
    t.mmio_write(0x2001, 1, 1);
    assert_eq!(t.t.mmio_read(0x2000, 4), 0x0023_0111, "EV_KEY: 35 bytes");
    // size, the reserved bytes and the union are read-only
    for (off, size) in [
        (0x2002u64, 1u8),
        (0x2003, 1),
        (0x2004, 4),
        (0x2008, 4),
        (0x2001, 2),
    ] {
        t.mmio_write(off, size, 0x5555_5555);
    }
    assert_eq!(t.t.mmio_read(0x2000, 4), 0x0023_0111);
    assert_eq!(t.t.mmio_read(0x2004, 4), 0);
    assert_eq!(t.t.mmio_read(0x2008 + 34, 1), 0x07);
    assert_eq!(t.t.mmio_read(0x2008, 4), 0);
    // the transport still takes its own registers
    t.mmio_write(0x16, 2, 1);
    assert_eq!(t.t.mmio_read(0x16, 2), 1);
}

// ------------------------------------------------------------------ events

#[test]
fn a_key_is_its_event_and_a_syn_report_in_one_buffer_each() {
    let mut r = kbd();
    r.post(4);
    assert!(r.d.key(KEY_A, true));
    assert_eq!(r.service(), (2, 0));
    assert_eq!(r.received(), [(1, KEY_A, 1), SYN]);
    assert!(r.d.t.irq());
    assert!(r.d.key(KEY_A, false));
    assert_eq!(r.service(), (2, 0));
    assert_eq!(r.received(), [(1, KEY_A, 0), SYN]);
    assert_eq!(r.d.delivered, 4);
    assert_eq!(r.d.queued(), 0);
    assert_eq!(r.service(), (0, 0), "no events left, the buffers stay");
    assert!(r.ev.take_used(&r.ram).is_empty());
}

#[test]
fn the_tablet_reports_position_and_buttons_clamped_to_its_axes() {
    let mut r = tablet();
    r.post(32);
    assert!(r.d.move_to(100, 200));
    assert!(r.d.move_to(5000, 6000));
    assert!(r.d.move_to(WIDTH - 1, HEIGHT - 1));
    assert!(r.d.move_to(WIDTH, 3));
    assert!(r.d.button(BTN_RIGHT, true));
    assert!(r.d.button(BTN_MIDDLE, false));
    assert!(r.d.key(BTN_LEFT, true));
    assert_eq!(r.service(), (18, 0));
    assert_eq!(
        r.received(),
        [
            (3, 0, 100),
            (3, 1, 200),
            SYN,
            (3, 0, WIDTH - 1),
            (3, 1, HEIGHT - 1),
            SYN,
            (3, 0, WIDTH - 1),
            (3, 1, HEIGHT - 1),
            SYN,
            (3, 0, WIDTH - 1),
            (3, 1, 3),
            SYN,
            (1, BTN_RIGHT, 1),
            SYN,
            (1, BTN_MIDDLE, 0),
            SYN,
            (1, BTN_LEFT, 1),
            SYN
        ]
    );
}

#[test]
fn a_device_takes_only_the_codes_it_has() {
    let mut r = kbd();
    for code in [
        KEY_ESC,
        KEY_KPDOT,
        KEY_102ND,
        KEY_F12,
        KEY_KPENTER,
        KEY_RIGHTALT,
        KEY_HOME,
        KEY_DELETE,
        KEY_PAUSE,
        KEY_LEFTMETA,
        KEY_COMPOSE,
    ] {
        assert!(r.d.key(code, true), "{code}");
    }
    for code in [
        0,
        84,
        85,
        89,
        95,
        KEY_LINEFEED,
        112,
        118,
        120,
        124,
        128,
        BTN_LEFT,
        u16::MAX,
    ] {
        assert!(!r.d.key(code, true), "{code}");
    }
    assert!(!r.d.button(BTN_LEFT, true), "the keyboard has no buttons");
    assert!(!r.d.move_to(1, 2), "nor axes");
    assert_eq!(r.d.queued(), 11);
    let mut r = tablet();
    for code in [KEY_A, 0x10F, 0x113, 0] {
        assert!(!r.d.key(code, true), "{code}");
        assert!(!r.d.button(code, false), "{code}");
    }
    assert_eq!(r.d.queued(), 0);
}

#[test]
fn events_wait_for_buffers_and_none_is_lost_below_capacity() {
    let mut r = kbd();
    for i in 0..QUEUE as u16 {
        assert!(r.d.key(KEY_ESC + i, i % 2 == 0));
    }
    assert_eq!(r.service(), (0, 0), "no buffers");
    assert_eq!((r.d.queued(), r.d.dropped), (QUEUE, 0));
    // five buffers: two reports and the first half of the third
    r.post(5);
    assert_eq!(r.service(), (5, 0));
    assert_eq!(r.received(), [(1, 1, 1), SYN, (1, 2, 0), SYN, (1, 3, 1)]);
    assert_eq!(r.d.queued(), QUEUE - 2, "the third has begun");
    r.post(64);
    assert_eq!(r.service(), (59, 0));
    let mut want = vec![SYN];
    for i in 3..QUEUE as u16 {
        want.push((1, KEY_ESC + i, u32::from(i % 2 == 0)));
        want.push(SYN);
    }
    assert_eq!(r.received(), want);
    assert_eq!((r.d.queued(), r.d.dropped, r.d.delivered), (0, 0, 64));
    // the five buffers left over take the next report
    assert!(r.d.key(KEY_S, true));
    assert_eq!(r.service(), (2, 0));
    assert_eq!(r.received(), [(1, KEY_S, 1), SYN]);
}

#[test]
fn a_full_queue_drops_its_oldest_whole_reports() {
    let mut r = kbd();
    for i in 0..QUEUE as u16 + 3 {
        assert!(r.d.key(KEY_ESC + i, true));
    }
    assert_eq!((r.d.queued(), r.d.dropped), (QUEUE, 3));
    r.post(2 * QUEUE);
    assert_eq!(r.service(), (2 * QUEUE as u32, 0));
    let mut want = Vec::new();
    for i in 3..QUEUE as u16 + 3 {
        want.push((1, KEY_ESC + i, 1));
        want.push(SYN);
    }
    assert_eq!(r.received(), want, "the three oldest went, whole");
}

#[test]
fn a_report_the_guest_has_begun_to_receive_is_never_dropped() {
    let mut r = tablet();
    let at = |i: u32| (10 + i, 300 + 2 * i);
    for i in 0..QUEUE as u32 {
        assert!(r.d.move_to(at(i).0, at(i).1));
    }
    r.post(1);
    assert_eq!(r.service(), (1, 0));
    assert_eq!(r.received(), [(3, 0, at(0).0)]);
    // two more reports: the second and third oldest go, the begun one stays first
    assert!(r.d.move_to(at(32).0, at(32).1));
    assert!(r.d.move_to(at(33).0, at(33).1));
    assert_eq!((r.d.queued(), r.d.dropped), (QUEUE, 2));
    r.post(2);
    assert_eq!(r.service(), (2, 0));
    assert_eq!(
        r.received(),
        [(3, 1, at(0).1), SYN],
        "the rest of the begun report"
    );
    assert_eq!(r.d.queued(), QUEUE - 1);
    // nothing has begun now: when the queue is full again, the oldest goes
    assert!(r.d.move_to(at(34).0, at(34).1));
    assert_eq!((r.d.queued(), r.d.dropped), (QUEUE, 2));
    assert!(r.d.move_to(at(35).0, at(35).1));
    assert_eq!((r.d.queued(), r.d.dropped), (QUEUE, 3));
    r.post(3 * QUEUE);
    assert_eq!(r.service(), (3 * QUEUE as u32, 0));
    let mut want = Vec::new();
    for i in (4..QUEUE as u32).chain(32..36) {
        want.extend([(3, 0, at(i).0), (3, 1, at(i).1), SYN]);
    }
    assert_eq!(r.received(), want);
}

#[test]
fn bad_event_buffers_are_returned_empty_and_the_event_waits_for_a_good_one() {
    let mut r = kbd();
    assert!(r.d.key(KEY_A, true));
    let fill = [0x5Au8; 16];
    assert!(r.ram.write(BUF, &fill));
    r.ev.add(&mut r.ram, &[(BUF, 8, false)]); // 0: the device may not write it
    r.ev.add(&mut r.ram, &[(BUF + 0x100, 7, true)]); // 1: too short
    r.ev.add(
        &mut r.ram,
        &[(BUF + 0x200, 4, true), (BUF + 0x300, 3, true)],
    ); // 2, 3: too short together
    r.ev.add(
        &mut r.ram,
        &[(BUF + 0x400, 8, true), (BUF + 0x500, 8, false)],
    ); // 4, 5: mixed
    r.ev.add(&mut r.ram, &[(0x10_0000, 8, true)]); // 6: not guest memory
    r.ev.add(&mut r.ram, &[(BUF + 0x600, 4, true), (0x10_0000, 4, true)]); // 7, 8: half of it is not
                                                                           // 9, 10, 11: an empty buffer anywhere, then the event split over two
    r.ev.add(
        &mut r.ram,
        &[
            (u64::MAX, 0, true),
            (BUF + 0x700, 3, true),
            (BUF + 0x800, 9, true),
        ],
    );
    r.ev.add(&mut r.ram, &[(BUF + 0x900, 16, true)]); // 12: room for two, takes one
    assert!(r.ram.write(BUF + 0x803, &[FILL; 6]));
    assert!(r.ram.write(BUF + 0x908, &[FILL; 8]));
    assert_eq!(r.service(), (8, 0));
    assert_eq!(
        r.ev.take_used(&r.ram),
        [
            (0, 0),
            (1, 0),
            (2, 0),
            (4, 0),
            (6, 0),
            (7, 0),
            (9, 8),
            (12, 8)
        ]
    );
    assert_eq!(r.d.bad_buffers, 6);
    assert_eq!(r.d.delivered, 2);
    let mut b = [0u8; 16];
    assert!(r.ram.read(BUF, &mut b));
    assert_eq!(b, fill, "a read-only buffer is left alone");
    let mut split = [0u8; 8];
    assert!(r.ram.read(BUF + 0x700, &mut split[..3]));
    assert!(r.ram.read(BUF + 0x800, &mut split[3..]));
    assert_eq!(decode(&split), (1, KEY_A, 1));
    let mut rest = [0u8; 4];
    assert!(r.ram.read(BUF + 0x805, &mut rest));
    assert_eq!(rest, [FILL; 4], "the second buffer took five bytes");
    assert!(r.ram.read(BUF + 0x900, &mut b));
    assert_eq!(decode(&b), SYN);
    assert_eq!(b[8..], [FILL; 8]);
}

#[test]
fn the_status_queue_takes_the_leds_and_ignores_everything_else() {
    let mut r = kbd();
    let steps: [(Ev, u8); 13] = [
        ((0x11, 1, 1), 0b010),
        ((0x11, 0, 1), 0b011),
        ((0x11, 1, 0), 0b001),
        ((0x11, 2, 0x0100_0000), 0b101),
        // the value is the whole le32: any byte of it set means on
        ((0x11, 1, 0x0000_0100), 0b111),
        ((0x11, 1, 0), 0b101),
        ((0x11, 1, 0x0001_0000), 0b111),
        ((0x11, 1, 0), 0b101),
        ((0x11, 3, 1), 0b101),
        ((0x11, 8, 1), 0b101),
        ((0x14, 1, 1), 0b101),
        ((1, 1, 1), 0b101),
        ((0x11, 0, 0), 0b100),
    ];
    for (i, (e, leds)) in steps.into_iter().enumerate() {
        r.status(STS + 16 * i as u64, e);
        assert_eq!(r.service(), (0, 1));
        assert_eq!(r.d.leds, leds, "{e:?}");
        assert!(r.d.t.irq(), "the status buffer was completed");
        r.d.t.mmio_read(0x1000, 1);
    }
    assert_eq!(r.d.status_events, 13);
    assert_eq!(r.st.take_used(&r.ram).len(), 13);
    assert!(r.ev.take_used(&r.ram).is_empty());
    assert_eq!((LED_NUML, LED_CAPSL, LED_SCROLLL), (0, 1, 2));
    // the tablet has no LEDs
    let mut t = tablet();
    t.status(STS, (0x11, 1, 1));
    assert_eq!(t.service(), (0, 1));
    assert_eq!((t.d.leds, t.d.status_events), (0, 1));
}

#[test]
fn bad_status_buffers_are_counted_and_returned() {
    let mut r = kbd();
    let caps_on = [0x11, 0, 1, 0, 1, 0, 0, 0];
    assert!(r.ram.write(STS, &caps_on));
    assert!(r.ram.write(STS + 0x100, &caps_on[..2]));
    assert!(r.ram.write(STS + 0x200, &caps_on[2..]));
    r.st.add(&mut r.ram, &[(STS, 8, true)]); // device-writable
    r.st.add(&mut r.ram, &[(STS, 7, false)]); // too short
    r.st.add(&mut r.ram, &[(STS, 8, false), (STS + 0x300, 8, true)]); // mixed
    r.st.add(&mut r.ram, &[(0x10_0000, 8, false)]); // not guest memory
    assert_eq!(r.service(), (0, 4));
    assert_eq!((r.d.status_bad, r.d.status_events, r.d.leds), (4, 0, 0));
    assert_eq!(r.st.take_used(&r.ram), [(0, 0), (1, 0), (2, 0), (4, 0)]);
    // split over two buffers, with an empty one at a bogus address between them
    r.st.add(
        &mut r.ram,
        &[
            (STS + 0x100, 2, false),
            (u64::MAX, 0, false),
            (STS + 0x200, 6, false),
        ],
    );
    assert_eq!(r.service(), (0, 1));
    assert_eq!((r.d.status_bad, r.d.status_events, r.d.leds), (4, 1, 0b010));
}

#[test]
fn a_driver_that_does_not_reset_first_still_gets_the_first_event_first() {
    let mut d = VirtioInput::keyboard(14);
    let (ev, st) = (Q::new(0x1000, 128), Q::new(0x4000, 16));
    assert_eq!(handshake(&mut d, &ev, &st, F_VERSION_1, false), 0xF);
    assert_eq!(d.t.resets, 0, "no reset happened");
    let mut r = Rig {
        d,
        ram: Ram(vec![0; 0x40000]),
        ev,
        st,
    };
    assert!(r.d.key(KEY_A, true));
    assert!(r.d.key(KEY_S, true));
    r.post(4);
    assert_eq!(r.service(), (4, 0));
    assert_eq!(
        r.received(),
        [(1, KEY_A, 1), SYN, (1, KEY_S, 1), SYN],
        "from the first event of the first report"
    );
}

#[test]
fn events_and_status_are_served_in_one_call() {
    let mut r = kbd();
    assert!(r.d.key(KEY_A, true));
    r.post(2);
    r.status(STS, (0x11, 2, 1));
    assert_eq!(r.service(), (2, 1));
    assert_eq!(r.received(), [(1, KEY_A, 1), SYN]);
    assert_eq!(r.d.leds, 0b100);
}

#[test]
fn nothing_moves_without_bus_mastering() {
    let mut r = kbd();
    r.d.t.cfg.write(COMMAND, 2, 0x0002);
    assert!(r.d.key(KEY_A, true));
    r.post(2);
    r.status(STS, (0x11, 1, 1));
    assert_eq!(r.service(), (0, 0));
    assert!(r.ev.take_used(&r.ram).is_empty());
    assert_eq!((r.d.leds, r.d.queued()), (0, 1));
    r.d.t.cfg.write(COMMAND, 2, 0x0006);
    assert_eq!(r.service(), (2, 1));
    assert_eq!(r.received(), [(1, KEY_A, 1), SYN]);
    assert_eq!(r.d.leds, 0b010);
}

#[test]
fn reports_are_ignored_while_the_driver_is_not_running() {
    let mut d = VirtioInput::keyboard(14);
    assert!(d.key(KEY_A, true), "the key exists");
    assert_eq!((d.queued(), d.ignored), (0, 1));
    let mut r = kbd();
    // stopped by DEVICE_NEEDS_RESET: a descriptor that links to itself
    let mut desc = [0u8; 16];
    desc[8..12].copy_from_slice(&8u32.to_le_bytes());
    desc[12..14].copy_from_slice(&3u16.to_le_bytes());
    assert!(r.ram.write(r.ev.desc, &desc));
    assert!(r.ram.write(r.ev.avail + 4, &0u16.to_le_bytes()));
    assert!(r.ram.write(r.ev.avail + 2, &1u16.to_le_bytes()));
    assert!(r.d.key(KEY_A, true));
    assert_eq!(r.service(), (0, 0));
    assert_ne!(r.d.t.status() & STATUS_NEEDS_RESET, 0);
    assert!(!r.d.t.driver_ok());
    assert_eq!(r.d.delivered, 0);
    assert!(r.d.key(KEY_S, true));
    assert_eq!((r.d.queued(), r.d.ignored), (1, 1));
}

#[test]
fn a_reset_empties_the_queue_and_forgets_a_begun_report() {
    let mut r = tablet();
    assert!(r.d.move_to(7, 9));
    r.post(1);
    assert_eq!(r.service(), (1, 0));
    assert_eq!(r.received(), [(3, 0, 7)]);
    // the driver resets the device and sets it up again
    let (ev, st) = (Q::new(0x1000, 128), Q::new(0x4000, 16));
    assert_eq!(bring_up(&mut r.d, &ev, &st, F_VERSION_1), 0xF);
    (r.ev, r.st) = (ev, st);
    assert!(r.d.move_to(11, 13));
    assert!(r.d.move_to(17, 19));
    assert_eq!(r.d.queued(), 2, "the old report is gone");
    r.post(6);
    assert_eq!(r.service(), (6, 0));
    assert_eq!(
        r.received(),
        [(3, 0, 11), (3, 1, 13), SYN, (3, 0, 17), (3, 1, 19), SYN]
    );
    // a reset the service notices before any new report
    assert!(r.d.move_to(23, 29));
    let (ev, st) = (Q::new(0x1000, 128), Q::new(0x4000, 16));
    assert_eq!(bring_up(&mut r.d, &ev, &st, F_VERSION_1), 0xF);
    (r.ev, r.st) = (ev, st);
    r.post(3);
    assert_eq!(r.service(), (0, 0));
    assert_eq!(r.d.queued(), 0);
    assert!(r.d.move_to(31, 37));
    assert_eq!(r.service(), (3, 0));
    assert_eq!(r.received(), [(3, 0, 31), (3, 1, 37), SYN]);
}

#[test]
fn the_isr_says_used_buffers_and_clears_on_read() {
    let mut r = kbd();
    assert!(!r.d.t.irq());
    assert!(r.d.key(KEY_A, true));
    assert!(!r.d.t.irq(), "queued, nothing delivered");
    r.post(1);
    r.service();
    assert!(r.d.t.irq());
    assert_eq!(r.d.t.mmio_read(0x1000, 1), 1);
    assert!(!r.d.t.irq());
    assert_eq!(r.d.t.take_kicks(), 0, "the service took the notification");
}

// ------------------------------------------------------- on the platform bus

fn cfg_addr(dev: u8, off: u32) -> u32 {
    (1 << 31) | (u32::from(dev) << 11) | (off & !3)
}

fn cfg_read(m: &mut Machine, dev: u8, off: u32, size: u8) -> u32 {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(dev, off), 0);
    m.io_in(PCI_DATA + (off & 3) as u16, size, 0)
}

fn cfg_write(m: &mut Machine, dev: u8, off: u32, size: u8, v: u32) {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(dev, off), 0);
    m.io_out(PCI_DATA + (off & 3) as u16, size, v, 0);
}

const KBD_BAR: u64 = 0xC002_0000;
const TAB_BAR: u64 = 0xC002_4000;

fn bus_bring_up(m: &mut Machine, dev: u8, bar: u64, ev: &Q, st: &Q) {
    cfg_write(m, dev, 0x10, 4, bar as u32);
    cfg_write(m, dev, 0x04, 2, 0x0006);
    for (off, size, v) in [
        (0x14u64, 1u8, 0u32),
        (0x14, 1, 1),
        (0x14, 1, 3),
        (0x08, 4, 1),
        (0x0C, 4, 1),
        (0x14, 1, 0xB),
    ] {
        m.mmio_write(bar + off, size, u64::from(v), 0);
    }
    for (i, q) in [ev, st].into_iter().enumerate() {
        m.mmio_write(bar + 0x16, 2, i as u64, 0);
        m.mmio_write(bar + 0x18, 2, u64::from(q.n), 0);
        m.mmio_write(bar + 0x20, 4, q.desc, 0);
        m.mmio_write(bar + 0x28, 4, q.avail, 0);
        m.mmio_write(bar + 0x30, 4, q.used, 0);
        m.mmio_write(bar + 0x1C, 2, 1, 0);
    }
    m.mmio_write(bar + 0x14, 1, 0xF, 0);
    assert_eq!(m.mmio_read(bar + 0x14, 1, 0), 0xF);
}

#[test]
fn on_the_bus_the_keyboard_is_slot_7_on_irq_14_and_the_tablet_slot_8_on_irq_6() {
    let mut m = Machine::new(0, 100_000_000);
    assert_eq!((slot::KEYBOARD, slot::TABLET), (7, 8));
    for (dev, line) in [(7u8, 14u32), (8, 6)] {
        assert_eq!(cfg_read(&mut m, dev, 0, 4), 0x1052_1AF4);
        assert_eq!(cfg_read(&mut m, dev, 0x3C, 1), line, "8259 line");
    }
    assert_eq!((pci_pin(7), pci_pin(8)), (19, 16));
    let (kev, kst) = (Q::new(0x1000, 8), Q::new(0x4000, 8));
    let (tev, tst) = (Q::new(0x10000, 8), Q::new(0x13000, 8));
    bus_bring_up(&mut m, 7, KBD_BAR, &kev, &kst);
    bus_bring_up(&mut m, 8, TAB_BAR, &tev, &tst);
    // the configuration through the bus: each device answers for itself
    m.mmio_write(KBD_BAR + 0x2000, 1, u64::from(cfg::ID_NAME), 0);
    m.mmio_write(TAB_BAR + 0x2000, 1, u64::from(cfg::ABS_INFO), 0);
    m.mmio_write(TAB_BAR + 0x2001, 1, u64::from(ABS_Y), 0);
    assert_eq!(m.mmio_read(KBD_BAR + 0x2002, 1, 0), 14);
    assert_eq!(m.mmio_read(TAB_BAR + 0x2002, 1, 0), 20);
    assert_eq!(m.mmio_read(TAB_BAR + 0x200C, 4, 0), 767, "768 rows");
    m.mmio_write(TAB_BAR + 0x2001, 1, u64::from(ABS_X), 0);
    assert_eq!(m.mmio_read(TAB_BAR + 0x200C, 4, 0), 1023, "1024 columns");
    assert_eq!(m.unclaimed_mmio, 0);
    // the 8259: IRQ 6 and 14 level-triggered and unmasked, with the cascade
    for (cmd, data, icw3, base) in [(0x20u16, 0x21u16, 4u8, 0x20u8), (0xA0, 0xA1, 2, 0x28)] {
        m.io_out(cmd, 1, 0x11, 0);
        m.io_out(data, 1, u32::from(base), 0);
        m.io_out(data, 1, u32::from(icw3), 0);
        m.io_out(data, 1, 1, 0);
        m.io_out(data, 1, 0xFF, 0);
    }
    m.io_out(0x4D0, 1, 0x40, 0);
    m.io_out(0x4D1, 1, 0x40, 0);
    m.io_out(0x21, 1, 0xBB, 0);
    m.io_out(0xA1, 1, 0xBF, 0);
    let mut ram = Ram(vec![0; 0x40000]);
    let (mut kev, mut kst, mut tev, mut tst) = (kev, kst, tev, tst);
    assert!(ram.write(0x22000, &[0x11, 0, 1, 0, 1, 0, 0, 0]));
    // a key and the Caps Lock LED: IRQ 14
    for i in 0..2 {
        kev.add(&mut ram, &[(0x20000 + 16 * i, 8, true)]);
    }
    kst.add(&mut ram, &[(0x22000, 8, false)]);
    assert!(m.keyboard.key(KEY_A, true));
    assert_eq!(m.service_input(&mut ram, 0), (2, 1));
    assert!(
        m.pic.int_pending(),
        "the service itself carried the line to the 8259"
    );
    assert_eq!(m.keyboard.leds, 0b010);
    assert_eq!(m.pending(0), Some(0x2E), "IRQ 14");
    assert_eq!(m.acknowledge(0), Some(0x2E));
    assert_eq!(m.mmio_read(KBD_BAR + 0x1000, 1, 0), 1);
    m.io_out(0xA0, 1, 0x20, 0);
    m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(m.pending(0), None, "the ISR read dropped the line");
    // both devices in one call; the keyboard's IRQ 14, through the cascade on IRQ 2, outranks the
    // tablet's 6
    for i in 2..4 {
        kev.add(&mut ram, &[(0x20000 + 16 * i, 8, true)]);
    }
    for i in 0..3 {
        tev.add(&mut ram, &[(0x21000 + 16 * i, 8, true)]);
    }
    kst.add(&mut ram, &[(0x22000, 8, false)]);
    tst.add(&mut ram, &[(0x22000, 8, false)]);
    assert!(m.keyboard.key(KEY_A, false));
    assert!(m.tablet.move_to(512, 384));
    assert_eq!(m.service_input(&mut ram, 0), (5, 2));
    assert!(m.pic.int_pending(), "and again");
    assert_eq!(m.acknowledge(0), Some(0x2E), "IRQ 14 first");
    assert_eq!(m.mmio_read(KBD_BAR + 0x1000, 1, 0), 1);
    m.io_out(0xA0, 1, 0x20, 0);
    m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(m.pending(0), Some(0x26), "then IRQ 6");
    assert_eq!(kev.take_used(&ram), [(0, 8), (1, 8), (2, 8), (3, 8)]);
    assert_eq!(tev.take_used(&ram), [(0, 8), (1, 8), (2, 8)]);
    assert_eq!(kst.take_used(&ram), [(0, 0), (1, 0)]);
    assert_eq!(tst.take_used(&ram), [(0, 0)]);
    let mut b = [0u8; 8];
    assert!(ram.read(0x21000, &mut b));
    assert_eq!(decode(&b), (3, 0, 512));
    assert!(ram.read(0x21010, &mut b));
    assert_eq!(decode(&b), (3, 1, 384));
    assert!(ram.read(0x20020, &mut b));
    assert_eq!(decode(&b), (1, KEY_A, 0));
}

#[test]
fn functions_sharing_an_io_apic_pin_are_wired_or() {
    let mut m = Machine::new(0, 100_000_000);
    let lapic_base = lapic::DEFAULT_BASE;
    m.mmio_write(lapic_base + u64::from(reg::SVR), 4, 0x1FF, 0);
    m.mmio_write(lapic_base + u64::from(reg::LVT_LINT0), 4, 1 << 16, 0);
    let route = |m: &mut Machine, pin: u8, low: u32| {
        m.mmio_write(ioapic::DEFAULT_BASE, 4, u64::from(0x10 + 2 * pin + 1), 0);
        m.mmio_write(ioapic::DEFAULT_BASE + 0x10, 4, 0, 0);
        m.mmio_write(ioapic::DEFAULT_BASE, 4, u64::from(0x10 + 2 * pin), 0);
        m.mmio_write(ioapic::DEFAULT_BASE + 0x10, 4, u64::from(low), 0);
    };
    let level_low = (1 << 15) | (1 << 13);
    for (pin, vector) in [(16u8, 0x60u32), (17, 0x70), (18, 0x80), (19, 0x90)] {
        route(&mut m, pin, level_low | vector);
    }
    let eoi = |m: &mut Machine| m.mmio_write(lapic_base + u64::from(reg::EOI), 4, 0, 0);
    // pin 19: the disk (listed first) and the keyboard; pin 16: the network card and the tablet
    for (first, second, vector) in [
        (slot::BLK, slot::KEYBOARD, 0x90u8),
        (slot::NET, slot::TABLET, 0x60),
    ] {
        assert_eq!(pci_pin(first), pci_pin(second));
        assert_eq!(m.pending(0), None);
        transport(&mut m, first).interrupt();
        assert_eq!(m.pending(0), Some(vector), "the first alone");
        assert_eq!(m.acknowledge(0), Some(vector));
        transport(&mut m, second).interrupt();
        transport(&mut m, first).mmio_read(0x1000, 1);
        eoi(&mut m);
        assert_eq!(m.pending(0), Some(vector), "the second alone keeps it low");
        assert_eq!(m.acknowledge(0), Some(vector));
        transport(&mut m, second).mmio_read(0x1000, 1);
        eoi(&mut m);
        assert_eq!(m.pending(0), None, "both quiet");
    }
}

fn transport(m: &mut Machine, dev: u8) -> &mut VirtioPci {
    match dev {
        slot::BLK => &mut m.blk.t,
        slot::NET => &mut m.net.t,
        slot::KEYBOARD => &mut m.keyboard.t,
        _ => &mut m.tablet.t,
    }
}
