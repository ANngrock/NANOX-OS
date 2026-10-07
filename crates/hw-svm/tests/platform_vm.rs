//! The platform vCPU (`hw_svm::platform_vm`) on the scripted processor: every
//! port and every access to device memory reaches `vmm_devices::machine`,
//! interrupts come from its controllers on one virtual clock, virtio requests
//! are served when the guest notifies and host input at the poll points, and
//! the machine's reset and power-off end the run. Expected values are worked
//! out here from the data sheets' formulas, not read back from the devices.

mod common;

use std::collections::VecDeque;

use common::*;
use hw_svm::platform_vm::{Host, InputHost, PlatformVcpu, TSC_HZ};
use hw_svm::vmcb::{bits, ctl, misc1, misc2, save, tlb};
use hw_svm::vmm::{Outcome, Vcpu, Verdict};
use hw_svm::PAGE_SIZE;
use vmm_devices::hpet;
use vmm_devices::machine::{slot, Machine};
use vmm_devices::virtio::GuestMemory;
use vmm_devices::virtio_blk::{BlockBackend, SECTOR};
use vmm_devices::virtio_console::ConsoleBackend;
use vmm_devices::virtio_gpu::{Rect, Scanout};
use vmm_devices::virtio_net::{NetBackend, MAX_FRAME};

/// 2023-11-14 22:13:20 UTC.
const EPOCH: i64 = 1_700_000_000;
const BUS_HZ: u64 = 1_000_000_000;
const PIT_HZ: u64 = 1_193_182;
const PM_HZ: u64 = 3_579_545;
const APIC: u64 = 0xFEE0_0000;
const IOAPIC: u64 = 0xFEC0_0000;
const HPET: u64 = 0xFED0_0000;
const ECAM: u64 = 0xB000_0000;
const PASS: Verdict = Verdict::DebugExit {
    value: 0x10,
    status: 33,
};
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RBP: u8 = 5;
const RSI: u8 = 6;
const RDI: u8 = 7;

// ---- guest instructions -------------------------------------------------------

fn out(port: u16, size: u8, value: u32) -> Step {
    Step::Out { port, size, value }
}

fn out8(port: u16, value: u32) -> Step {
    out(port, 1, value)
}

fn inp(port: u16, size: u8, expect: u32) -> Step {
    Step::In { port, size, expect }
}

fn debug_exit() -> Step {
    out8(0xF4, 0x10)
}

/// ModRM/SIB/disp32 of an absolute `[disp32]` operand with ModRM.reg = `reg`.
fn absolute(b: &mut Vec<u8>, reg: u8, gpa: u64) {
    b.extend([0x04 | reg << 3, 0x25]);
    b.extend((gpa as u32).to_le_bytes());
}

/// mov <size> [gpa], imm
fn st_with(gpa: u64, size: u8, imm: u64, assist: bool) -> Step {
    let mut b = match size {
        1 => vec![0xC6],
        2 => vec![0x66, 0xC7],
        4 => vec![0xC7],
        _ => vec![0x48, 0xC7],
    };
    absolute(&mut b, 0, gpa);
    let n = usize::from(size.min(4));
    b.extend(&imm.to_le_bytes()[..n]);
    mmio(gpa, &b, assist)
}

fn st(gpa: u64, size: u8, imm: u64) -> Step {
    st_with(gpa, size, imm, true)
}

/// mov <size> [gpa], reg
fn st_reg(gpa: u64, size: u8, reg: u8) -> Step {
    let mut b = match size {
        1 => vec![0x88],
        2 => vec![0x66, 0x89],
        4 => vec![0x89],
        _ => vec![0x48, 0x89],
    };
    absolute(&mut b, reg, gpa);
    mmio(gpa, &b, true)
}

/// mov reg, <size> [gpa] (1 byte: AL, CL, DL, BL)
fn ld_with(gpa: u64, size: u8, reg: u8, assist: bool) -> Step {
    let mut b = match size {
        1 => vec![0x8A],
        2 => vec![0x66, 0x8B],
        4 => vec![0x8B],
        _ => vec![0x48, 0x8B],
    };
    absolute(&mut b, reg, gpa);
    mmio(gpa, &b, assist)
}

fn ld(gpa: u64, size: u8, reg: u8) -> Step {
    ld_with(gpa, size, reg, true)
}

/// The 8259 pair as Linux sets it up (vectors 0x20 and 0x28, cascade on 2),
/// with the master's mask `imr`.
fn pic_init(imr: u32) -> Vec<Step> {
    vec![
        out8(0x20, 0x11),
        out8(0x21, 0x20),
        out8(0x21, 0x04),
        out8(0x21, 0x01),
        out8(0x21, imr),
    ]
}

/// The local APIC on, and I/O APIC pin `pin` to `vector` with `flags`
/// (bit 15 level, bit 13 active low).
fn apic_route(pin: u64, vector: u64, flags: u64) -> Vec<Step> {
    vec![
        st(APIC + 0xF0, 4, 0x1FF),
        st(IOAPIC, 4, 0x10 + 2 * pin),
        st(IOAPIC + 0x10, 4, vector | flags),
    ]
}

const LEVEL_LOW: u64 = 1 << 15 | 1 << 13;

fn eoi() -> Step {
    st(APIC + 0xB0, 4, 0)
}

/// OUT0 of PIT mode 2 rises at clock n + 1 of the run, then every n; clock k
/// comes ceil(k / 1.193182 MHz) after the count is written.
fn pit_ns(clocks: u64) -> u64 {
    (clocks * 1_000_000_000).div_ceil(PIT_HZ)
}

// ---- virtio --------------------------------------------------------------------

/// Queue size the tests use.
const VQ: u64 = 8;
const NEXT: u16 = 1;
const WRITE: u16 = 2;

fn bar_of(dev: u8) -> u64 {
    0xC000_0000 + u64::from(dev) * 0x1_0000
}

/// What a driver does before it uses function `dev`: BAR 0, memory and bus
/// master, the status handshake with VERSION_1, each queue (index, ring area:
/// descriptors at +0, available ring at +0x100, used ring at +0x200),
/// DRIVER_OK. With `enable_rbx` the queue is enabled from BX.
fn driver(dev: u8, queues: &[(u16, u64)], enable_rbx: bool) -> Vec<Step> {
    let bar = bar_of(dev);
    let cfg = 0x8000_0000 | u32::from(dev) << 11;
    let mut s = vec![
        out(0xCF8, 4, cfg | 0x10),
        out(0xCFC, 4, bar as u32),
        out(0xCF8, 4, cfg | 0x04),
        out(0xCFC, 2, 6),
        st(bar + 0x14, 1, 1),
        st(bar + 0x14, 1, 3),
        st(bar + 0x08, 4, 1),
        st(bar + 0x0C, 4, 1),
        st(bar + 0x14, 1, 0xB),
    ];
    for &(q, area) in queues {
        s.extend([
            st(bar + 0x16, 2, q.into()),
            st(bar + 0x18, 2, VQ),
            st(bar + 0x20, 4, area),
            st(bar + 0x28, 4, area + 0x100),
            st(bar + 0x30, 4, area + 0x200),
            if enable_rbx {
                st_reg(bar + 0x1C, 2, RBX)
            } else {
                st(bar + 0x1C, 2, 1)
            },
        ]);
    }
    s.push(st(bar + 0x14, 1, 0xF));
    s
}

fn notify(dev: u8, q: u16) -> Step {
    st(bar_of(dev) + 0x3000 + 4 * u64::from(q), 2, q.into())
}

/// Reads (and so clears) the ISR of `dev` into CL.
fn read_isr(dev: u8) -> Step {
    ld(bar_of(dev) + 0x1000, 1, RCX)
}

// ---- the host -------------------------------------------------------------------

/// Guest RAM for DMA: [base, base + len).
struct Ram {
    base: u64,
    bytes: Vec<u8>,
}

impl Ram {
    fn range(&self, gpa: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let off = usize::try_from(gpa.checked_sub(self.base)?).ok()?;
        let end = off.checked_add(len)?;
        (end <= self.bytes.len()).then_some(off..end)
    }

    fn put(&mut self, gpa: u64, data: &[u8]) {
        let r = self.range(gpa, data.len()).expect("test address in RAM");
        self.bytes[r].copy_from_slice(data);
    }

    fn get(&self, gpa: u64, len: usize) -> &[u8] {
        &self.bytes[self.range(gpa, len).expect("test address in RAM")]
    }

    fn u16(&self, gpa: u64) -> u16 {
        u16::from_le_bytes(self.get(gpa, 2).try_into().unwrap())
    }

    fn u32(&self, gpa: u64) -> u32 {
        u32::from_le_bytes(self.get(gpa, 4).try_into().unwrap())
    }

    fn desc(&mut self, area: u64, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut e = Vec::new();
        e.extend(addr.to_le_bytes());
        e.extend(len.to_le_bytes());
        e.extend(flags.to_le_bytes());
        e.extend(next.to_le_bytes());
        self.put(area + 16 * u64::from(i), &e);
    }

    /// The available ring of the queue at `area` offers chain `head`.
    fn offer(&mut self, area: u64, head: u16) {
        self.put(area + 0x100, &[0, 0, 1, 0]);
        self.put(area + 0x104, &head.to_le_bytes());
    }

    /// (used index, first used element (id, len)) of the queue at `area`.
    fn used(&self, area: u64) -> (u16, u32, u32) {
        (
            self.u16(area + 0x202),
            self.u32(area + 0x204),
            self.u32(area + 0x208),
        )
    }
}

impl GuestMemory for Ram {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        match self.range(gpa, buf.len()) {
            Some(r) => {
                buf.copy_from_slice(&self.bytes[r]);
                true
            }
            None => false,
        }
    }

    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        match self.range(gpa, data.len()) {
            Some(r) => {
                self.bytes[r].copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

struct Disk {
    data: Vec<[u8; SECTOR]>,
    reads: Vec<u64>,
    writes: Vec<u64>,
}

impl BlockBackend for Disk {
    fn sectors(&self) -> u64 {
        self.data.len() as u64
    }
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        let Some(s) = self.data.get(sector as usize) else {
            return false;
        };
        *buf = *s;
        self.reads.push(sector);
        true
    }
    fn write(&mut self, sector: u64, data: &[u8; SECTOR]) -> bool {
        let Some(s) = self.data.get_mut(sector as usize) else {
            return false;
        };
        *s = *data;
        self.writes.push(sector);
        true
    }
    fn flush(&mut self) -> bool {
        true
    }
}

#[derive(Default)]
struct Net {
    sent: Vec<Vec<u8>>,
    inbox: VecDeque<Vec<u8>>,
}

impl NetBackend for Net {
    fn send(&mut self, frame: &[u8]) {
        self.sent.push(frame.to_vec());
    }
    fn recv(&mut self, buf: &mut [u8; MAX_FRAME]) -> Option<usize> {
        let f = self.inbox.pop_front()?;
        buf[..f.len()].copy_from_slice(&f);
        Some(f.len())
    }
}

#[derive(Default)]
struct Screen {
    created: Vec<(u32, u32, u32, u32)>,
    other: u32,
}

impl Scanout for Screen {
    fn create(&mut self, resource: u32, width: u32, height: u32, format: u32) -> bool {
        self.created.push((resource, width, height, format));
        true
    }
    fn destroy(&mut self, _: u32) {
        self.other += 1;
    }
    fn put(&mut self, _: u32, _: u32, _: u32, _: &[u8]) {
        self.other += 1;
    }
    fn show(&mut self, _: u32, _: u32, _: Rect) {
        self.other += 1;
    }
    fn present(&mut self, _: u32, _: u32, _: Rect) {
        self.other += 1;
    }
}

#[derive(Default)]
struct Console {
    out: Vec<u8>,
    inbox: VecDeque<u8>,
}

impl ConsoleBackend for Console {
    fn write(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
    }
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.inbox.len());
        for b in buf.iter_mut().take(n) {
            *b = self.inbox.pop_front().unwrap();
        }
        n
    }
}

/// Host input: the virtual time of every poll; per poll, a PS/2 scancode
/// byte or nothing; a virtio-input key for the first poll.
#[derive(Default)]
struct Input {
    polls: Vec<u64>,
    ps2: VecDeque<Option<u8>>,
    virtio_key: Option<u16>,
}

impl InputHost for Input {
    fn poll(&mut self, m: &mut Machine, now: u64) {
        self.polls.push(now);
        if let Some(Some(b)) = self.ps2.pop_front() {
            m.key(b);
        }
        if let Some(k) = self.virtio_key.take() {
            assert!(m.keyboard.key(k, true), "the keyboard has key {k}");
        }
    }
}

struct World {
    ram: Ram,
    disk: Disk,
    net: Net,
    screen: Screen,
    console: Console,
    input: Input,
}

impl World {
    fn new() -> Self {
        let data = (0..8u32)
            .map(|s| core::array::from_fn(|i| (s * 37 + i as u32 * 7) as u8))
            .collect();
        Self {
            ram: Ram {
                base: 0x8000,
                bytes: vec![0; 0x8000],
            },
            disk: Disk {
                data,
                reads: Vec::new(),
                writes: Vec::new(),
            },
            net: Net::default(),
            screen: Screen::default(),
            console: Console::default(),
            input: Input::default(),
        }
    }

    fn host(&mut self) -> Host<'_> {
        Host {
            mem: &mut self.ram,
            disk: &mut self.disk,
            net: &mut self.net,
            display: &mut self.screen,
            console: &mut self.console,
            input: &mut self.input,
        }
    }
}

fn machine() -> Machine {
    Machine::new(EPOCH, BUS_HZ)
}

/// Runs `script` on a fresh machine (after `setup`); the outcome and the
/// machine afterwards.
fn go(script: &[Step], world: &mut World, setup: impl FnOnce(&mut Machine)) -> (Outcome, Machine) {
    let mut rig = Rig::platform(script);
    let mut serial = [0u8; 64];
    let mut m = machine();
    setup(&mut m);
    let mut v = PlatformVcpu::new(rig.cfg, m, &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    rig.cpu.assert_clean();
    (o, v.machine().clone())
}

// ---- ports ----------------------------------------------------------------------

/// UART, PIC, PIT, RTC and CMOS, i8042, ACPI PM, PCI configuration and an
/// empty port, each with the exact value IN leaves in RAX (the rig checks
/// that IN AL/AX keep the rest of RAX and IN EAX clears its upper half).
#[test]
fn ports_reach_the_legacy_devices_with_exact_rax() {
    // PIT channel 0, mode 2, count 0x1000 written by exit 14, latched by
    // exit 23: 10 clocks of the run, the first loads the count.
    let clocks = (23_000 - 14_000) * PIT_HZ / 1_000_000_000;
    let count = 0x1000 - (clocks - 1) as u32;
    assert_eq!(count, 0x0FF7);
    // The PM timer read by exit 30.
    let pm = (30_000 * PM_HZ / 1_000_000_000) as u32;
    let script = [
        out8(0x3FF, 0xA5),
        inp(0x3FF, 1, 0xA5), // scratch register
        inp(0x3FD, 1, 0x60), // LSR: THRE | TEMT
        out8(0x3F8, u32::from(b'H')),
        out8(0x3F8, u32::from(b'i')),
        out8(0x20, 0x11),
        out8(0x21, 0x20),
        out8(0x21, 0x04),
        out8(0x21, 0x01),
        out8(0x21, 0xB7),
        inp(0x21, 1, 0xB7), // IMR
        out8(0x43, 0x34),
        out8(0x40, 0x00),
        out8(0x40, 0x10),
        out8(0x70, 0x00),
        inp(0x71, 1, 0x20), // seconds, BCD
        out8(0x70, 0x04),
        inp(0x71, 1, 0x22), // hours, 24-hour BCD
        out8(0x70, 0x40),
        out8(0x71, 0x5A),
        out8(0x70, 0x40),
        inp(0x71, 1, 0x5A), // NVRAM
        out8(0x43, 0x00),   // latch channel 0
        inp(0x40, 1, count & 0xFF),
        inp(0x40, 1, count >> 8),
        inp(0x64, 1, 0x14), // i8042: unlocked, system flag
        out8(0x64, 0x20),   // read the command byte
        inp(0x64, 1, 0x1D), // output buffer full, last write a command
        inp(0x60, 1, 0x45), // interrupt, system, translation
        inp(0x608, 4, pm),
        out8(0xB2, 0x02), // ACPI enable through the SMI command port
        inp(0x604, 2, 0x0001),
        out(0xCF8, 4, 0x8000_0000),
        inp(0xCFC, 4, 0x29C0_8086), // the q35 host bridge
        inp(0xCFE, 2, 0x29C0),
        inp(0x1234, 2, 0xFFFF), // nothing there: two empty byte ports
        out8(0x1234, 0x77),
        debug_exit(),
    ];
    let mut rig = Rig::platform(&script);
    let mut serial = [0u8; 8];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let mut world = World::new();
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(v.serial(), b"Hi", "COM1 output is captured");
    assert!(!o.serial_truncated, "it fits");
    let m = v.machine();
    assert_eq!(
        (m.unclaimed_in, m.unclaimed_out, m.unclaimed_mmio),
        (2, 1, 0),
        "empty ports are counted, byte by byte"
    );
    assert_eq!(o.virtual_ns, 38_000, "1 us per exit");
    assert_eq!(v.now_ns(), o.virtual_ns);
    assert!(world.input.polls.is_empty(), "no HLT, no host interrupt");
    rig.cpu.assert_clean();
}

#[test]
fn serial_output_beyond_the_buffer_is_reported() {
    let mut rig = Rig::platform(&[
        out8(0x3F8, u32::from(b'O')),
        out8(0x3F8, u32::from(b'K')),
        debug_exit(),
    ]);
    let mut serial = [0u8; 1];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!((o.serial_len, o.serial_truncated), (1, true));
    assert_eq!(v.serial(), b"O");
}

// ---- device memory --------------------------------------------------------------

/// LAPIC, I/O APIC, HPET, ECAM, a virtio BAR placed through ECAM and an
/// empty address, with the instruction from decode assists and fetched
/// through the guest's page tables.
#[test]
fn device_memory_through_decode_assists_and_the_guest_page_walk() {
    for assist in [true, false] {
        let blk = ECAM + (u64::from(slot::BLK) << 15);
        let mut rig = Rig::platform(&[
            ld_with(APIC + 0x30, 4, RDX, assist),
            st_with(IOAPIC, 4, 0x01, assist),
            ld_with(IOAPIC + 0x10, 4, RBX, assist),
            ld_with(HPET, 8, RCX, assist),
            ld_with(ECAM, 4, RSI, assist),
            st_with(blk + 0x10, 4, 0xC000_0000, assist),
            st_with(blk + 0x04, 2, 6, assist),
            ld_with(0xC000_0012, 2, RDI, assist),
            ld_with(0xA_0000, 4, RBP, assist),
            st_with(0xA_0000, 1, 0x12, assist),
            debug_exit(),
        ]);
        if !assist {
            rig.place_code();
        }
        rig.gprs.rdx = 0xFFFF_FFFF_0000_0000;
        rig.gprs.rdi = 0xFFFF_FFFF_FFFF_0000;
        let mut serial = [0u8; 4];
        let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
        let o = run_platform(&mut rig, &mut v, &mut World::new().host());
        assert_eq!(o.verdict, PASS, "assist={assist}");
        let g = rig.gprs;
        assert_eq!(g.rdx, 0x0005_0014, "LAPIC version, zero-extended");
        assert_eq!(g.rbx, 0x0017_0020, "I/O APIC version: 24 pins");
        assert_eq!(g.rcx, hpet::CAPABILITIES, "64-bit HPET read");
        assert_eq!(g.rsi, 0x29C0_8086, "host bridge through ECAM");
        assert_eq!(g.rdi, 0xFFFF_FFFF_FFFF_0001, "num_queues, 16-bit merge");
        assert_eq!(g.rbp, 0xFFFF_FFFF, "empty address reads ones");
        assert_eq!(o.mmio, 10);
        assert_eq!(v.machine().unclaimed_mmio, 2);
        rig.cpu.assert_clean();
    }
}

/// A 64-bit store reaches the device whole (HPET timer 1's comparator) and a
/// 64-bit load brings it back.
#[test]
fn quadword_device_accesses() {
    let mut rig = Rig::platform(&[
        st_reg(HPET + 0x128, 8, RBX),
        ld(HPET + 0x128, 8, RCX),
        Step::Hlt,
    ]);
    rig.gprs.rbx = 0x1234_5678_9ABC_DEF0;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, Verdict::Halted);
    assert_eq!((rig.gprs.rcx, o.mmio), (0x1234_5678_9ABC_DEF0, 2));
    rig.cpu.assert_clean();
}

/// mov [gpa], r32 or mov r32, [gpa] (`opcode` 0x89 or 0x8B) for any of the
/// 16 registers (REX.R for R8D..R15D).
fn reg32(opcode: u8, gpa: u64, reg: u8) -> Step {
    let mut b = if reg >= 8 { vec![0x44] } else { vec![] };
    b.push(opcode);
    absolute(&mut b, reg & 7, gpa);
    mmio(gpa, &b, true)
}

/// Every general register as the source of a store and the destination of
/// a load: register r goes to the low half of an I/O APIC entry of its own
/// and comes back; the 32-bit store drops the upper half, the 32-bit load
/// zero-extends. (Pins 9 and 16..19 have live inputs whose delivery status
/// would read back.)
#[test]
fn every_register_reaches_device_memory_and_back() {
    const PINS: [u64; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 20];
    let value = |r: u64| 0xABCD_0000_0001_0020 + (r << 32) + r;
    let mut script = Vec::new();
    for opcode in [0x89, 0x8B] {
        for r in 0..16u8 {
            script.push(st(IOAPIC, 4, 0x10 + 2 * PINS[usize::from(r)]));
            script.push(reg32(opcode, IOAPIC + 0x10, r));
        }
    }
    script.push(Step::Hlt); // IF=0: ends the run without touching RAX
    let mut rig = Rig::platform(&script);
    let g = &mut rig.gprs;
    for (r, slot) in [
        &mut g.rcx, &mut g.rdx, &mut g.rbx, &mut g.rbp, &mut g.rsi, &mut g.rdi, &mut g.r8,
        &mut g.r9, &mut g.r10, &mut g.r11, &mut g.r12, &mut g.r13, &mut g.r14, &mut g.r15,
    ]
    .into_iter()
    .enumerate()
    {
        let index = [1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15][r];
        *slot = value(index);
    }
    rig.vmcb().set_rax(value(0));
    rig.vmcb().write_u64(save::RSP, value(4));
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, Verdict::Halted);
    let low = |r: u64| value(r) & 0xFFFF_FFFF;
    let g = rig.gprs;
    let got = [
        rig.vmcb().rax(),
        g.rcx,
        g.rdx,
        g.rbx,
        rig.vmcb().read_u64(save::RSP),
        g.rbp,
        g.rsi,
        g.rdi,
        g.r8,
        g.r9,
        g.r10,
        g.r11,
        g.r12,
        g.r13,
        g.r14,
        g.r15,
    ];
    let want: Vec<u64> = (0..16).map(low).collect();
    assert_eq!(got.to_vec(), want);
    assert_eq!(o.mmio, 64);
    rig.cpu.assert_clean();
}

/// Only a data access where no RAM is mapped is device memory: a permission
/// fault, a fetch, or a fault in the guest's page-table walk ends the run.
#[test]
fn faults_that_are_not_device_accesses_end_the_run() {
    for error in [
        1 | 4 | 1 << 32,      // read from a RAM page the guest may not read
        1 | 2 | 4 | 1 << 32,  // write to a read-only RAM page
        4 | 1 << 4 | 1 << 32, // instruction fetch from nowhere
        4 | 1 << 33,          // the guest's page tables point nowhere
    ] {
        let mut rig = Rig::platform(&[Step::Npf {
            gpa: 0xA_0000,
            error,
        }]);
        let mut serial = [0u8; 4];
        let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
        let o = run_platform(&mut rig, &mut v, &mut World::new().host());
        assert_eq!(
            o.verdict,
            Verdict::NestedPageFault {
                gpa: 0xA_0000,
                error
            },
            "{error:#x}"
        );
        assert_eq!((o.mmio, v.machine().unclaimed_mmio), (0, 0));
    }
}

#[test]
fn string_io_and_undecodable_mmio_are_reported() {
    let mut rig = Rig::platform(&[Step::OutString { port: 0x3F8 }]);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, Verdict::UnsupportedIo { port: 0x3F8 });
    // add [rax], eax is not an MMIO form; without assists and with no code
    // in guest memory there are no bytes at all.
    for (bytes, assist) in [(&[0x01u8, 0x00][..], true), (&[0x8B, 0x00], false)] {
        let mut rig = Rig::platform(&[mmio(HPET, bytes, assist)]);
        let rip = rig.cpu.rip_of(0);
        let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
        let o = run_platform(&mut rig, &mut v, &mut World::new().host());
        assert_eq!(o.verdict, Verdict::MmioUnsupported { gpa: HPET, rip });
    }
}

// ---- interrupts and time ----------------------------------------------------------

/// PIT channel 0 → IRQ0 → 8259 (the local APIC is software-disabled, so the
/// 8259 is the CPU's interrupt). The mode-2 control word itself raises OUT0
/// (an edge while IF=0): the first HLT finds it pending and skips no time;
/// each later HLT skips exactly to the next OUT0 edge, and the vector is
/// taken right after the HLT.
#[test]
fn pit_irq0_reaches_the_guest_through_the_8259() {
    let first = 8_000 + pit_ns(101);
    let second = 8_000 + pit_ns(201);
    assert_eq!((first, second), (92_648, 176_458));
    let mut script = pic_init(0xFE);
    script.extend([
        out8(0x43, 0x34), // exit 6: OUT0 rises
        out8(0x40, 100),
        out8(0x40, 0), // count 100 by exit 8
        Step::Sti,
        Step::Hlt, // exit 9: the control word's edge is pending
        out8(0x20, 0x20),
        Step::Hlt, // until the first period's edge
        out8(0x20, 0x20),
        Step::Hlt, // until the second
        debug_exit(),
    ]);
    let mut rig = Rig::platform(&script);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let mut world = World::new();
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    assert_eq!(o.verdict, PASS);
    let rip = |i| rig.cpu.rip_of(i);
    assert_eq!(
        rig.cpu.interrupts,
        vec![(0x20, rip(10)), (0x20, rip(12)), (0x20, rip(14))]
    );
    assert_eq!(o.irqs, 3);
    assert_eq!(o.virtual_ns, second + 1_000, "the last exit after the edge");
    assert_eq!(
        world.input.polls,
        vec![9_000, 11_000, first + 2_000],
        "every HLT polls"
    );
    assert_eq!(v.machine().pit_coalesced, 0);
    rig.cpu.assert_clean();
}

/// HPET timer 0 in legacy replacement → IRQ0 → I/O APIC pin 2 → LAPIC. It
/// fires while IF=0: the interrupt waits for STI and its shadow, then the
/// window brings the VMM back.
#[test]
fn hpet_irq0_through_the_ioapic_waits_for_the_guest() {
    let mut script = apic_route(2, 0x30, 0);
    script.extend([
        st(HPET + 0x10, 4, 3),      // enabled, legacy replacement, by exit 4
        st(HPET + 0x100, 4, 0x104), // timer 0: interrupt, 32-bit
        st(HPET + 0x108, 4, 50),    // 50 ticks of 100 ns: at 9 us
        out8(0x80, 0),
        out8(0x80, 0),
        out8(0x80, 0), // exit 9: fired, IF=0
        out8(0x80, 0),
        Step::Sti,
        Step::Load { gpa: 0x5000 }, // the STI shadow
        Step::Load { gpa: 0x5000 }, // the window opens here
        eoi(),
        debug_exit(),
    ]);
    let mut rig = Rig::platform(&script);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.cpu.interrupts, vec![(0x30, rig.cpu.rip_of(12))]);
    assert_eq!((o.irqs, o.virtual_ns), (1, 13_000));
    // Taken: the window is closed again, V_INTR_MASKING alone remains.
    let vmcb = rig.vmcb();
    assert_eq!(vmcb.read_u64(ctl::VINTR), 1 << 24);
    assert_eq!(vmcb.read_u32(ctl::INTERCEPT_MISC1), INTERCEPTS_3);
    rig.cpu.assert_clean();
}

/// While an interrupt waits for IF, the window asks for V_IRQ at the highest
/// priority, ignoring the virtual TPR, with the VINTR intercept.
#[test]
fn the_interrupt_window_bits() {
    let mut rig = Rig::platform(&[
        st(APIC + 0xF0, 4, 0x1FF),
        st(APIC + 0x320, 4, 0x40),
        st(APIC + 0x380, 4, 1),
        out8(0x80, 0), // pending from here on, IF=0
        Step::Hlt,
    ]);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, Verdict::Halted);
    let vmcb = rig.vmcb();
    assert_eq!(
        vmcb.read_u64(ctl::VINTR),
        1 << 24 | 1 << 8 | 0xF << 16 | 1 << 20
    );
    assert_eq!(vmcb.read_u32(ctl::INTERCEPT_MISC1), INTERCEPTS_3 | 1 << 4);
    assert!(rig.cpu.interrupts.is_empty());
}

/// Intercept vector 3 (APM 15.9): INTR, NMI, SMI, INIT (bits 0-3), RDTSC (14),
/// CPUID (18), PAUSE (23), HLT (24), IOIO_PROT (27), MSR_PROT (28), SHUTDOWN (31).
const INTERCEPTS_3: u32 = 0x9984_400F;

/// The control area: exactly the intercepts the loop handles, nested paging
/// with the configured tables and maps, V_INTR_MASKING, no stale event.
#[test]
fn the_control_area() {
    let mut rig = Rig::platform(&[]);
    let cfg = rig.cfg;
    let mut serial = [0u8; 4];
    let v = PlatformVcpu::new(cfg, machine(), &mut serial);
    let mut vmcb = rig.vmcb();
    vmcb.set_event_inj(1 << 31 | 0x30);
    v.prepare(&mut vmcb);
    assert_eq!(vmcb.read_u32(ctl::INTERCEPT_MISC1), INTERCEPTS_3);
    // Vector 4: VMRUN, VMMCALL, VMLOAD, VMSAVE, STGI, CLGI, SKINIT, RDTSCP.
    assert_eq!(vmcb.read_u32(ctl::INTERCEPT_MISC2), 0xFF);
    assert_eq!(vmcb.read_u64(ctl::VINTR), 1 << 24);
    assert_eq!(vmcb.event_inj(), 0);
    assert_eq!(vmcb.read_u32(ctl::ASID), cfg.asid);
    assert_eq!(vmcb.read_u64(ctl::NP_ENABLE), 1);
    assert_eq!(vmcb.read_u64(ctl::N_CR3), cfg.npt_root);
    assert_eq!(vmcb.read_u64(ctl::IOPM_BASE), IOPM_PA);
    assert_eq!(vmcb.read_u64(ctl::MSRPM_BASE), MSRPM_PA);
}

/// Each budget stops the run at its exact boundary.
#[test]
fn budgets_stop_the_run_at_their_boundary() {
    // Five host ticks of 1 us reach 5 us of virtual time.
    let mut rig = Rig::platform(&[Step::SpinForever]);
    rig.cfg.max_virtual_ns = 5_000;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!((o.verdict, o.exits), (Verdict::VirtualTimeout, 5));
    // The clock advances 1 us per reading: one at the start, one per exit.
    let mut rig = Rig::platform(&[Step::SpinForever]);
    rig.cfg.max_time_us = 3;
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!((o.verdict, o.exits), (Verdict::Timeout, 3));
    let mut rig = Rig::platform(&[Step::SpinForever]);
    rig.cfg.max_exits = 4;
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!((o.verdict, o.exits), (Verdict::ExitBudget, 4));
}

/// Two devices scheduled: each HLT skips exactly to the earlier deadline.
#[test]
fn hlt_wakes_at_the_earliest_device_deadline() {
    let mut script = apic_route(2, 0x31, 0);
    script.insert(1, st(APIC + 0x320, 4, 0x40)); // LAPIC timer: one-shot, vector 0x40
    script.extend([
        st(HPET + 0x10, 4, 3), // by exit 5
        st(HPET + 0x100, 4, 0x104),
        st(HPET + 0x108, 4, 400),   // at 5 us + 40 us
        st(APIC + 0x380, 4, 5_000), // by exit 8: 5000 counts at bus/2 = 10 us
        Step::Sti,
        Step::Hlt, // until 18 us (LAPIC)
        eoi(),
        Step::Hlt, // until 45 us (HPET)
        eoi(),
        debug_exit(),
    ]);
    let mut rig = Rig::platform(&script);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(
        rig.cpu.interrupts,
        vec![(0x40, rig.cpu.rip_of(10)), (0x31, rig.cpu.rip_of(12))]
    );
    assert_eq!(o.virtual_ns, 45_000 + 2_000);
    rig.cpu.assert_clean();
}

/// A host that paces virtual time: an HLT stops at the limit (the run ends,
/// the guest wakes without an event); resumed with the limit lifted, the
/// guest halts again and sleeps on to its deadline.
#[test]
fn hlt_stops_at_the_virtual_time_limit() {
    let script = [
        st(APIC + 0xF0, 4, 0x1FF),
        st(APIC + 0x320, 4, 0x40),  // LAPIC timer: one-shot, vector 0x40
        st(APIC + 0x380, 4, 5_000), // at exit 3: 10 us later, 13 us
        Step::Sti,
        Step::Hlt, // exit 4, stopped at 8 us
        Step::Hlt, // until 13 us
        eoi(),
        debug_exit(),
    ];
    let mut rig = Rig::platform(&script);
    rig.cfg.max_virtual_ns = 8_000;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(
        (o.verdict, o.exits, o.virtual_ns),
        (Verdict::VirtualTimeout, 4, 8_000)
    );
    assert_eq!(v.now_ns(), 8_000);
    assert!(rig.cpu.interrupts.is_empty(), "woken without an event");
    v.set_virtual_limit(u64::MAX);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.cpu.interrupts, vec![(0x40, rig.cpu.rip_of(6))]);
    assert_eq!(o.virtual_ns, 13_000 + 2_000);
    rig.cpu.assert_clean();
}

#[test]
fn hlt_without_a_wake_up_source_is_final() {
    // IF=1, nothing scheduled; IF=0 with a LAPIC deadline.
    for (script, polls) in [
        (vec![Step::Sti, Step::Hlt], 1),
        (
            vec![
                st(APIC + 0xF0, 4, 0x1FF),
                st(APIC + 0x320, 4, 0x40),
                st(APIC + 0x380, 4, 10),
                Step::Hlt,
            ],
            0,
        ),
    ] {
        let mut world = World::new();
        let (o, _) = go(&script, &mut world, |_| {});
        assert_eq!(o.verdict, Verdict::Halted);
        assert_eq!(world.input.polls.len(), polls);
    }
}

// ---- virtio ---------------------------------------------------------------------

/// A virtio-blk read through the loop: the driver's set-up writes do not
/// serve the queue, the notification does; the sector lands in guest
/// memory, the used ring says so, and the interrupt (INTA → I/O APIC pin 19,
/// level, active low) is taken right after the notifying instruction. The
/// queue is enabled with a 16-bit store of BX whose upper bits are set: only
/// the stored bytes may reach the device.
#[test]
fn virtio_blk_request_end_to_end() {
    const AREA: u64 = 0x8000;
    let mut script = apic_route(19, 0x41, LEVEL_LOW);
    script.push(Step::Sti); // any early completion would interrupt at once
    script.extend(driver(slot::BLK, &[(0, AREA)], true));
    script.push(ld(bar_of(slot::BLK) + 0x2000, 4, RSI)); // capacity
    let notified = script.len();
    script.extend([
        notify(slot::BLK, 0),
        read_isr(slot::BLK),
        eoi(),
        debug_exit(),
    ]);
    let mut world = World::new();
    let r = &mut world.ram;
    r.put(0x8400, &[0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]); // IN, sector 2
    r.put(0x8900, &[0xFF]);
    r.desc(AREA, 0, 0x8400, 16, NEXT, 1);
    r.desc(AREA, 1, 0x8600, SECTOR as u32, NEXT | WRITE, 2);
    r.desc(AREA, 2, 0x8900, 1, WRITE, 0);
    r.offer(AREA, 0);
    let mut rig = Rig::platform(&script);
    rig.gprs.rbx = 0x5555_0001;
    rig.gprs.rsi = 0xFFFF_FFFF_0000_0000;
    rig.gprs.rcx = 0x1234_5600;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.gprs.rsi, 8, "capacity: the disk's sectors");
    assert_eq!(world.disk.reads, vec![2]);
    assert!(world.disk.writes.is_empty());
    assert_eq!(world.ram.get(0x8600, SECTOR), &world.disk.data[2][..]);
    assert_eq!(world.ram.get(0x8900, 1), &[0], "status OK");
    assert_eq!(world.ram.used(AREA), (1, 0, SECTOR as u32 + 1));
    assert_eq!(
        rig.cpu.interrupts,
        vec![(0x41, rig.cpu.rip_of(notified + 1))],
        "served at the notification"
    );
    assert_eq!(rig.gprs.rcx, 0x1234_5601, "ISR: queue interrupt");
    assert_eq!(o.irqs, 1, "reading the ISR lowered the line before EOI");
    rig.cpu.assert_clean();
}

/// One request on queue `q` of `dev` (its chain already in `world`), notified.
fn notify_once(dev: u8, q: u16, world: &mut World) -> Machine {
    let mut script = driver(dev, &[(q, 0x8000)], false);
    script.extend([notify(dev, q), debug_exit()]);
    let (o, m) = go(&script, world, |_| {});
    assert_eq!(o.verdict, PASS);
    assert_eq!(world.ram.used(0x8000).0, 1, "slot {dev}: request completed");
    assert!(world.disk.reads.is_empty() && world.input.polls.is_empty());
    m
}

#[test]
fn notification_serves_the_network_card() {
    let mut world = World::new();
    let frame: Vec<u8> = (0..60u8).map(|i| i * 3 + 1).collect();
    let mut packet = vec![0u8; 12];
    packet.extend(&frame);
    world.ram.put(0x8400, &packet);
    world.ram.desc(0x8000, 0, 0x8400, packet.len() as u32, 0, 0);
    world.ram.offer(0x8000, 0);
    let m = notify_once(slot::NET, 1, &mut world);
    assert_eq!(world.net.sent, vec![frame]);
    assert!(world.console.out.is_empty() && world.screen.created.is_empty());
    assert_eq!(m.net.sent, 1);
}

#[test]
fn notification_serves_the_agent_channel() {
    let mut world = World::new();
    world.ram.put(0x8400, b"hello agent");
    world.ram.desc(0x8000, 0, 0x8400, 11, 0, 0);
    world.ram.offer(0x8000, 0);
    notify_once(slot::CONSOLE, 1, &mut world);
    assert_eq!(world.console.out, b"hello agent");
    assert!(world.net.sent.is_empty() && world.screen.created.is_empty());
}

#[test]
fn notification_serves_the_display() {
    let mut world = World::new();
    let mut cmd = vec![0u8; 24];
    cmd[..4].copy_from_slice(&0x101u32.to_le_bytes()); // RESOURCE_CREATE_2D
    for v in [1u32, 1, 64, 32] {
        cmd.extend(v.to_le_bytes()); // resource, format B8G8R8A8, width, height
    }
    world.ram.put(0x8400, &cmd);
    world.ram.desc(0x8000, 0, 0x8400, 40, NEXT, 1);
    world.ram.desc(0x8000, 1, 0x8500, 24, WRITE, 0);
    world.ram.offer(0x8000, 0);
    notify_once(slot::GPU, 0, &mut world);
    assert_eq!(world.screen.created, vec![(1, 64, 32, 1)]);
    assert_eq!(world.ram.u32(0x8500), 0x1100, "OK_NODATA");
    assert!(world.net.sent.is_empty() && world.console.out.is_empty());
}

/// The status queues of the keyboard (Caps Lock on) and of the tablet (no
/// LEDs: taken and ignored).
#[test]
fn notification_serves_the_input_devices() {
    for dev in [slot::KEYBOARD, slot::TABLET] {
        let mut world = World::new();
        world.ram.put(0x8400, &[0x11, 0, 1, 0, 1, 0, 0, 0]); // EV_LED, LED_CAPSL, on
        world.ram.desc(0x8000, 0, 0x8400, 8, 0, 0);
        world.ram.offer(0x8000, 0);
        let m = notify_once(dev, 1, &mut world);
        let (kbd, tablet) = (&m.keyboard, &m.tablet);
        if dev == slot::KEYBOARD {
            assert_eq!(
                (kbd.leds, kbd.status_events, tablet.status_events),
                (2, 1, 0)
            );
        } else {
            assert_eq!(
                (kbd.leds, kbd.status_events, tablet.status_events),
                (0, 0, 1)
            );
        }
    }
}

/// A HLT is a poll point: the host's input is handed over first, then the
/// keyboard, the network card and the agent channel get what the host has;
/// the frame's interrupt ends the HLT without skipping time.
#[test]
fn hlt_polls_the_host() {
    let mut script = apic_route(16, 0x42, LEVEL_LOW); // virtio-net's INTA
    script.extend(driver(slot::NET, &[(0, 0x8000)], false));
    script.extend(driver(slot::CONSOLE, &[(0, 0x9000)], false));
    script.extend(driver(slot::KEYBOARD, &[(0, 0xA000)], false));
    let hlt = script.len() + 1;
    script.extend([
        Step::Sti,
        Step::Hlt,
        read_isr(slot::NET),
        eoi(),
        debug_exit(),
    ]);
    let mut world = World::new();
    let frame: Vec<u8> = (0..64u8).map(|i| 0xFF - i).collect();
    world.net.inbox.push_back(frame.clone());
    world.console.inbox.extend(b"to guest");
    world.input.virtio_key = Some(30);
    let r = &mut world.ram;
    r.desc(0x8000, 0, 0x8400, 0x800, WRITE, 0);
    r.offer(0x8000, 0);
    r.desc(0x9000, 0, 0x9400, 32, WRITE, 0);
    r.offer(0x9000, 0);
    r.desc(0xA000, 0, 0xA400, 8, WRITE, 0);
    r.offer(0xA000, 0);
    let mut rig = Rig::platform(&script);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    assert_eq!(o.verdict, PASS);
    // Every step before the HLT but Sti is an exit.
    assert_eq!(world.input.polls, vec![hlt as u64 * 1_000]);
    assert_eq!(rig.cpu.interrupts, vec![(0x42, rig.cpu.rip_of(hlt + 1))]);
    assert_eq!(o.virtual_ns, o.exits * 1_000, "no time skipped");
    let r = &world.ram;
    assert_eq!(r.used(0x8000), (1, 0, 12 + 64));
    assert_eq!(&r.get(0x8400, 12)[10..], &[1, 0], "num_buffers");
    assert_eq!(r.get(0x8400 + 12, 64), &frame[..]);
    assert_eq!(r.used(0x9000), (1, 0, 8));
    assert_eq!(r.get(0x9400, 8), b"to guest");
    assert_eq!(r.used(0xA000), (1, 0, 8));
    assert_eq!(
        r.get(0xA400, 8),
        &[1, 0, 30, 0, 1, 0, 0, 0],
        "EV_KEY A down"
    );
    assert!(world.net.inbox.is_empty() && world.console.inbox.is_empty());
    rig.cpu.assert_clean();
}

/// A host interrupt is a poll point too: a PS/2 key arriving at the second
/// tick is IRQ1 through the 8259, taken at once, read translated to set 1.
#[test]
fn host_interrupts_poll_the_host() {
    let mut script = pic_init(0xFD);
    script.extend([
        Step::Sti,
        Step::Tick,
        Step::Tick,
        Step::Tick,
        inp(0x60, 1, 0x1E), // set 2 0x1C ('A') in set 1
        out8(0x20, 0x20),
        debug_exit(),
    ]);
    let mut world = World::new();
    world.input.ps2.extend([None, Some(0x1C)]);
    let mut rig = Rig::platform(&script);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(world.input.polls, vec![6_000, 7_000, 8_000]);
    assert_eq!(rig.cpu.interrupts, vec![(0x21, rig.cpu.rip_of(7))]);
    rig.cpu.assert_clean();
}

// ---- platform outputs ---------------------------------------------------------------

#[test]
fn reset_and_sleep_requests_end_the_run() {
    for (script, want) in [
        // An A20 write alone is no reset.
        (vec![out8(0x92, 0x02), out8(0x92, 0x01)], Verdict::Reset),
        (vec![out8(0x64, 0xFE)], Verdict::Reset), // pulse the reset line
        (vec![out(0x604, 2, 5 << 10 | 1 << 13)], Verdict::PowerOff), // S5, SLP_EN
        (
            vec![out(0x604, 2, 1 << 10 | 1 << 13)],
            Verdict::Sleep { slp_typ: 1 },
        ),
    ] {
        let mut world = World::new();
        let (o, mut m) = go(&script, &mut world, |_| {});
        assert_eq!(o.verdict, want, "{script:?}");
        assert_eq!(o.exits, script.len() as u64, "{script:?}");
        assert!(
            !m.take_reset() && m.take_sleep().is_none(),
            "requests taken"
        );
    }
}

// ---- CPUID and MSRs -----------------------------------------------------------------

fn rd(msr: u32) -> Step {
    Step::MsrEmulated {
        msr,
        write: false,
        value: 0,
    }
}

fn wr(msr: u32, value: u64) -> Step {
    Step::MsrEmulated {
        msr,
        write: true,
        value,
    }
}

/// Refused: #GP.
fn wr_gp(msr: u32, value: u64) -> Step {
    Step::Wrmsr { msr, value }
}

// The architectural MSR numbers, written out: the module's constants are
// part of what is tested.
const EFER: u32 = 0xC000_0080;
const TSC: u32 = 0x10;
const MTRR_CAP: u32 = 0xFE;
const MTRR_DEF_TYPE: u32 = 0x2FF;
const INT_PENDING_MSG: u32 = 0xC001_0055;

#[test]
fn msr_policy_for_linux() {
    let (lme, lma, nxe, sce) = (bits::EFER_LME, bits::EFER_LMA, bits::EFER_NXE, 1);
    let long = lme | lma | nxe;
    let mut script = vec![
        rd(EFER), // SVME hidden
        wr(EFER, long | sce),
        rd(EFER),
        wr_gp(EFER, long | sce | bits::EFER_SVME), // no nested SVM
        wr_gp(EFER, lma | nxe | sce),              // LME off with paging on
        wr_gp(EFER, long | sce | 1 << 13),         // LMSLE
        wr_gp(EFER, long | sce | 1 << 40),         // a reserved bit in EDX
        wr(EFER, lme | nxe | sce),                 // LMA is the processor's
        rd(0x1B),
        rd(TSC), // virtual time, like RDTSC
        wr_gp(TSC, 5),
        rd(MTRR_CAP),
        wr_gp(MTRR_CAP, 0),
        rd(INT_PENDING_MSG),
        wr_gp(INT_PENDING_MSG, 0),
        rd(MTRR_DEF_TYPE),
    ];
    // Every valid default type, with FE and E; reserved types and bits.
    for t in [0u64, 1, 4, 5, 6] {
        script.extend([wr(MTRR_DEF_TYPE, 0xC00 | t), rd(MTRR_DEF_TYPE)]);
    }
    for v in [0x802, 0x803, 0x807, 0x906, 0xA06, 0x1806, 1 << 32 | 0xC06] {
        script.push(wr_gp(MTRR_DEF_TYPE, v));
    }
    script.extend([rd(MTRR_DEF_TYPE), Step::Rdmsr { msr: 0xC001_0131 }]);
    script.push(debug_exit());
    let mut rig = Rig::platform(&script);
    rig.cpu.msrs.insert(TSC, 0x1234_5678_9ABC); // the host's, never seen by the guest
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    let tsc = 10 * rig.cfg.exit_quantum_ns; // the tenth exit
    let mut want = vec![long, long | sce, 0xFEE0_0900, tsc, 0, 0];
    want.push(0x806); // MTRRs on, write-back
    want.extend([0xC00, 0xC01, 0xC04, 0xC05, 0xC06, 0xC06]);
    assert_eq!(rig.cpu.rdmsr_results, want);
    assert_eq!(o.msr_faults, 4 + 1 + 1 + 1 + 7 + 1);
    assert_eq!(
        rig.vmcb().read_u64(save::EFER),
        long | sce | bits::EFER_SVME,
        "LMA and SVME kept"
    );
    rig.cpu.assert_clean();
}

/// Before long mode (paging off) LME may change; LMA stays clear, it is the
/// processor's to set.
#[test]
fn efer_lme_changes_while_paging_is_off() {
    let (lme, svme) = (bits::EFER_LME, bits::EFER_SVME);
    let mut rig = Rig::platform(&[wr(EFER, lme | 1), rd(EFER), wr(EFER, 1), debug_exit()]);
    {
        let mut vmcb = rig.vmcb();
        let cr0 = vmcb.read_u64(save::CR0);
        vmcb.write_u64(save::CR0, cr0 & !bits::CR0_PG);
        vmcb.write_u64(save::EFER, svme);
    }
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.cpu.rdmsr_results, vec![lme | 1]);
    assert_eq!(rig.vmcb().read_u64(save::EFER), svme | 1, "LME off again");
    assert_eq!(o.msr_faults, 0);
    rig.cpu.assert_clean();
}

/// RDMSR leaves EDX:EAX zero-extended: the upper halves of RAX and RDX clear.
#[test]
fn rdmsr_clears_the_upper_halves() {
    let mut rig = Rig::platform(&[rd(0x1B), Step::Hlt]);
    rig.gprs.rdx = u64::MAX;
    rig.vmcb().set_rax(u64::MAX);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, Verdict::Halted);
    assert_eq!((rig.vmcb().rax(), rig.gprs.rdx), (0xFEE0_0900, 0));
}

/// Without NRIP save the loop advances RIP by the instruction lengths it
/// knows (RDMSR/WRMSR 2, PAUSE 2, HLT 1); the rig checks every RIP.
#[test]
fn without_nrips_fixed_instruction_lengths_are_used() {
    let mut rig = Rig::platform(&[
        rd(0x1B),
        wr(0x1B, 0xFEE0_0900),
        Step::Pause,
        st(APIC + 0xF0, 4, 0x1FF),
        st(APIC + 0x320, 4, 0x40),
        st(APIC + 0x380, 4, 100),
        Step::Sti,
        Step::Hlt,
        eoi(),
        debug_exit(),
    ]);
    rig.cfg.nrips = false;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.cpu.interrupts, vec![(0x40, rig.cpu.rip_of(8))]);
    rig.cpu.assert_clean();
}

/// CMP of device memory: the memory operand on the left (CMP m, imm/r) or
/// on the right (CMP r, m); only RFLAGS change.
#[test]
fn compares_with_device_memory_set_the_flags() {
    let version = 0x0005_0014u32;
    let mut cmp_imm = vec![0x81, 0x3C, 0x25];
    cmp_imm.extend(((APIC + 0x30) as u32).to_le_bytes());
    cmp_imm.extend(version.to_le_bytes());
    let mut cmp_rm = vec![0x3B, 0x14, 0x25]; // cmp edx, [m]
    cmp_rm.extend(((APIC + 0x30) as u32).to_le_bytes());
    let mut cmp_mr = vec![0x39, 0x14, 0x25]; // cmp [m], edx
    cmp_mr.extend(((APIC + 0x30) as u32).to_le_bytes());
    let (cf, pf, af, zf, sf) = (1u64, 1 << 2, 1 << 4, 1 << 6, 1 << 7);
    for (bytes, flags) in [
        (cmp_imm, 2 | zf | pf),          // equal
        (cmp_rm, 2 | cf | pf | af | sf), // 0x50013 - 0x50014
        (cmp_mr, 2),                     // 0x50014 - 0x50013
    ] {
        let mut rig = Rig::platform(&[mmio(APIC + 0x30, &bytes, true), Step::Hlt]);
        rig.gprs.rdx = u64::from(version) - 1;
        let mut serial = [0u8; 4];
        let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
        let o = run_platform(&mut rig, &mut v, &mut World::new().host());
        assert_eq!(o.verdict, Verdict::Halted);
        assert_eq!(rig.vmcb().rflags(), flags, "{bytes:02x?}");
        assert_eq!((o.mmio, rig.gprs.rdx), (1, u64::from(version) - 1));
        rig.cpu.assert_clean();
    }
}

#[test]
fn cpuid_for_linux() {
    let umip_cet_la57 = 1 << 2 | 1 << 7 | 1 << 16;
    let leaves = [
        (7, 0),
        (7, 1),
        (0x8000_001F, 0),
        (1, 0),
        (0x4000_0000, 0),
        (0x4000_0001, 0),
        (0x10, 0),
        (0x11, 0),
        (0x8000_0000, 0),
        (0x8000_0021, 0),
        (0x8000_0022, 0),
    ];
    let mut script: Vec<Step> = leaves
        .iter()
        .map(|&(leaf, sub)| Step::Cpuid { leaf, sub })
        .collect();
    script.push(debug_exit());
    let mut rig = Rig::platform(&script);
    let host7 = [0, 0x219C_07AB, umip_cet_la57 | 1 << 3, 1 << 20 | 1 << 4];
    // A host with VMX, MONITOR, x2APIC and TSC-deadline, and data at and
    // above its maxima (0x10 and 8000_0021h) and in the hypervisor range.
    let hidden = 1 << 5 | 1 << 3 | 1 << 21 | 1 << 24;
    let ecx1 = 0x7ED8_3203 | hidden;
    let h = &mut rig.cpu.host_cpuid;
    h.insert((7, 0), host7);
    h.insert((7, 1), host7);
    h.insert((0x8000_001F, 0), [0xF, 0x5F, 0x1FD, 1]);
    h.insert((1, 0), [0x00A5_0F00, 0x0010_0800, ecx1, 0x178B_FBFF]);
    h.insert((0x4000_0001, 0), [1, 2, 3, 4]);
    h.insert((0x10, 0), [5, 6, 7, 8]);
    h.insert((0x11, 0), [9, 10, 11, 12]);
    h.insert((0x8000_0021, 0), [13, 14, 15, 16]);
    h.insert((0x8000_0022, 0), [17, 18, 19, 20]);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    assert_eq!(
        run_platform(&mut rig, &mut v, &mut World::new().host()).verdict,
        PASS
    );
    let r = &rig.cpu.cpuid_results;
    assert_eq!(
        r[0],
        [0, 0x219C_07AB & !(1 << 1), 1 << 3, 1 << 4],
        "UMIP, CET, LA57 and TSC_ADJUST hidden"
    );
    assert_eq!(r[1], host7, "subleaf 1 untouched");
    assert_eq!(r[2], [0; 4], "no SME/SEV");
    assert_eq!(
        r[3],
        [
            0x00A5_0F00,
            0x0010_0800,
            0x7ED8_3203 | 1 << 31,
            0x178B_FBFF & !(1 << 7 | 1 << 14)
        ],
        "hypervisor present; VMX, MONITOR, x2APIC, TSC-deadline, MCE and MCA hidden"
    );
    // "NanoxVMM": the hypervisor's highest leaf, then the signature.
    let sig = [0x6F6E_614E, 0x4D4D_5678, 0];
    assert_eq!(r[4], [0x4000_0000, sig[0], sig[1], sig[2]]);
    assert_eq!(r[5], [0; 4], "no other hypervisor leaf");
    assert_eq!(r[6], [5, 6, 7, 8], "the highest basic leaf");
    assert_eq!(r[7], [0; 4], "above it");
    assert_eq!(r[8][0], 0x8000_0021, "the highest extended leaf");
    assert_eq!(r[9], [13, 14, 15, 16]);
    assert_eq!(r[10], [0; 4], "above it");
    rig.cpu.assert_clean();
}

// ---- nested paging --------------------------------------------------------------------

#[test]
fn unmap_flushes_the_guest_tlb() {
    let mut rig = Rig::platform(&[
        Step::Load { gpa: 2 * PAGE_SIZE },
        debug_exit(),
        Step::Load { gpa: 2 * PAGE_SIZE },
    ]);
    rig.cfg.flush_by_asid = true;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let mut world = World::new();
    assert_eq!(
        run_platform(&mut rig, &mut v, &mut world.host()).verdict,
        PASS
    );
    let token = rig
        .npt
        .unmap(&mut rig.cpu.mem, 2 * PAGE_SIZE, PAGE_SIZE)
        .unwrap();
    v.note_unmap(token);
    let o = run_platform(&mut rig, &mut v, &mut world.host());
    // Where no RAM is mapped is device memory now; the page with the guest's
    // page tables is gone too, so the load cannot be fetched.
    let rip = rig.cpu.rip_of(2);
    assert_eq!(
        o.verdict,
        Verdict::MmioUnsupported {
            gpa: 2 * PAGE_SIZE,
            rip
        }
    );
    assert_eq!(rig.cpu.tlb_controls.last(), Some(&tlb::FLUSH_ASID));
    rig.cpu.assert_clean();
}

#[test]
fn the_machine_is_the_callers() {
    let mut serial = [0u8; 4];
    let rig = Rig::platform(&[]);
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    v.machine_mut().key(0x1C);
    assert!(v.machine().kbd.irq1());
}

// ---- time stamp counter -----------------------------------------------------------------

#[test]
fn rdtsc_reads_virtual_time() {
    let script = [
        Step::Rdtsc,
        Step::Rdtsc,
        out8(0x80, 1),
        Step::Rdtsc,
        debug_exit(),
    ];
    let mut rig = Rig::platform(&script);
    rig.cfg.exit_quantum_ns = 1_234;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    // TSC_HZ is 1 GHz: the TSC is virtual nanoseconds; every exit charges one quantum
    assert_eq!(TSC_HZ, 1_000_000_000);
    assert_eq!(rig.cpu.rdtsc_results, vec![1_234, 2 * 1_234, 4 * 1_234]);
    let vmcb = rig.vmcb();
    assert_ne!(vmcb.read_u32(ctl::INTERCEPT_MISC1) & misc1::RDTSC, 0);
    assert_ne!(vmcb.read_u32(ctl::INTERCEPT_MISC2) & misc2::RDTSCP, 0);
    rig.cpu.assert_clean();
}

#[test]
fn a_tsc_beyond_32_bits_splits_into_edx_and_eax() {
    let mut rig = Rig::platform(&[Step::Rdtsc, debug_exit()]);
    rig.cfg.exit_quantum_ns = 0x1_2345_6789;
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    assert_eq!(
        run_platform(&mut rig, &mut v, &mut World::new().host()).verdict,
        PASS
    );
    assert_eq!(rig.cpu.rdtsc_results, vec![0x1_2345_6789]);
    rig.cpu.assert_clean();
}

#[test]
fn rdtscp_faults_and_cpuid_hides_it_and_tsc_adjust() {
    let script = [
        Step::Rdtscp,
        Step::Cpuid {
            leaf: 0x8000_0001,
            sub: 0,
        },
        Step::Cpuid { leaf: 7, sub: 0 },
        debug_exit(),
    ];
    let mut rig = Rig::platform(&script);
    rig.cpu
        .host_cpuid
        .insert((0x8000_0001, 0), [0, 0, 0x75C2_37FF, 0x2FD3_FBFF | 1 << 27]);
    rig.cpu
        .host_cpuid
        .insert((7, 0), [0, 0x219C_07AB | 1 << 1, 0, 0]);
    let mut serial = [0u8; 4];
    let mut v = PlatformVcpu::new(rig.cfg, machine(), &mut serial);
    let o = run_platform(&mut rig, &mut v, &mut World::new().host());
    assert_eq!(o.verdict, PASS);
    assert_eq!(o.ud_injected, 1, "RDTSCP: #UD");
    assert_eq!(rig.cpu.injected, vec![6 | 3 << 8 | 1 << 31]);
    assert_eq!(rig.cpu.cpuid_results[0][3] & 1 << 27, 0, "no RDTSCP");
    assert_eq!(rig.cpu.cpuid_results[1][1] & 1 << 1, 0, "no TSC_ADJUST");
    assert_eq!(rig.cpu.cpuid_results[1][1], 0x219C_07AB & !(1 << 1));
    rig.cpu.assert_clean();
}

#[test]
fn the_candidate_vcpu_leaves_the_tsc_alone() {
    let mut rig = Rig::new(&[Step::Rdtsc, debug_exit()]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(run(&mut rig, &mut v).verdict, PASS);
    assert_eq!(
        rig.cpu.rdtsc_results,
        vec![0x10_0000_0000],
        "the host's TSC, no exit"
    );
    rig.cpu.assert_clean();
}
