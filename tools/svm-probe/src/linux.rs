//! The `linux` case: a real Linux bzImage with an initramfs (fw_cfg
//! `opt/nanox/bzimage` and `opt/nanox/initrd`, the command line optionally in
//! `opt/nanox/cmdline`) boots on the whole emulated platform —
//! `hw_svm::platform_vm::PlatformVcpu` on a `vmm_devices::machine::Machine` —
//! on this processor's SVM (docs/specs/M11-WINDOW.md §5).
//!
//! The VMM does what firmware and a boot loader would: the platform's ACPI
//! tables in guest RAM (`vmm_devices::acpi::build`), the kernel, the
//! initramfs, the zero page with the e820 map and the entry page tables
//! (`guest_boot::linux::load_linux`), the 64-bit entry state in the VMCB
//! (`Vmcb::setup_linux_boot`). Guest RAM and its nested tables come from the
//! firmware (`hw::allocate_pages`); all of the RAM is mapped, every other
//! guest-physical address is device memory. The RAM's size is fw_cfg
//! `opt/nanox/ram-mib` (default [`DEFAULT_RAM_MIB`]); what does not fit below
//! `LOW_RAM_LIMIT` continues at 4 GiB, as the loader's e820 map says. It is
//! allocated in pieces of [`RAM_CHUNK`] wherever the firmware has them
//! ([`RamChunks`]): its free memory is split (in OVMF below and above 4 GiB,
//! and again at 1 GiB boundaries), on real machines all the more.
//!
//! The run is cut into slices of [`SLICE_EXITS`] exits (the probe has no wall
//! clock; its `Clock` counts the loop's iterations, so a slice ends with
//! `Verdict::Timeout` and the run is resumed): after each slice the guest's
//! new COM1 lines are printed as `NANOX:SVM-PROBE:LINUX <line>`, so a guest
//! that hangs still leaves its output in the serial log. The case passes when
//! the guest printed `NANOX_GUEST_REPORT_END` (tools/hostguest/init) and then
//! ended the run itself (power off, halt or reset).
//!
//! At the end the case prints what the guest asked of the VMM, counted at
//! every exit: exit codes, I/O ports, MSRs, CPUID leaves and device-memory
//! pages (`NANOX:SVM-PROBE:LINUX-EXITS`, `-PORTS`, `-MSRS`, `-CPUID`, `-MMIO`),
//! and the guest's screen: a linear framebuffer ([`FB`]) as UEFI GOP would
//! leave one, which Linux's EFI framebuffer drivers draw the console on
//! (`NANOX:SVM-PROBE:LINUX-SCREEN`, see [`dump_screen`]). While the guest
//! runs, the firmware's own display shows the NANOX screen with the server
//! window and the guest's screen in it (`crate::screen`), dumped at the end
//! as `NANOX:SVM-PROBE:NANOX-SCREEN`.

use crate::hw::Serial;
use crate::screen::{Display, Status};
use crate::{fwcfg, has, hw, Cpu, Env, Frames, GuestMap, Phys, HOST_TICK_COUNT};
use canvas::View;
use core::cell::Cell;
use core::fmt::Write;
use guest_boot::linux::{self, Acpi, Framebuffer, LinuxConfig, HIGH_RAM_BASE, LOW_RAM_LIMIT};
use guest_boot::GuestMemory as _;
use hw_svm::exit::code;
use hw_svm::perm::{MsrPermissionMap, MSRPM_BYTES};
use hw_svm::platform_vm::{Host, InputHost, PlatformVcpu};
use hw_svm::vmcb::{ctl, Gprs};
use hw_svm::vmm::{Outcome, Verdict, VmConfig};
use hw_svm::{Clock, Npt, NptPerms, SvmCpu, Vmcb, PAGE_SIZE};
use vmm_devices::acpi::{self, Platform};
use vmm_devices::i8042::set1_to_set2;
use vmm_devices::machine::{Machine, ECAM_BASE, ECAM_SIZE};
use vmm_devices::virtio::GuestMemory;
use vmm_devices::virtio_blk::{BlockBackend, SECTOR};
use vmm_devices::virtio_console::ConsoleBackend;
use vmm_devices::virtio_gpu::{Rect, Scanout};
use vmm_devices::virtio_net::{NetBackend, MAX_FRAME};
use vswitch::endpoint::Endpoint;

/// Guest RAM without fw_cfg `opt/nanox/ram-mib`: enough for the kernel
/// (init_size ~62 MiB from 16 MiB), its memory map and the initramfs at the top.
const DEFAULT_RAM_MIB: u64 = 256;
/// The guest RAM fw_cfg may ask for.
const MIN_RAM_MIB: u64 = 128;
const MAX_RAM_MIB: u64 = 64 << 10;
/// The ACPI tables: at 1 MiB, below the kernel (16 MiB).
const ACPI_GPA: u64 = 0x10_0000;
const ACPI_MAX: usize = 0x2000;
/// When the runner passes no command line.
const DEFAULT_CMDLINE: &[u8] = b"console=ttyS0 earlyprintk=serial,ttyS0,115200 panic=-1";
const CMDLINE_MAX: usize = 2048;
/// Virtual time per exit. The guest's TSC is virtual time too (RDTSC exits), so
/// every TSC read moves time; 2 us per exit lets Linux's PIT calibration see the
/// 1000 polls in 10 ms it requires (docs/specs/M11-WINDOW.md).
const QUANTUM_NS: u64 = 2_000;
const SLICE_EXITS: u64 = 20_000;
/// Slices of an interactive run (run.py --show), at most this many exits or
/// [`PACE_NS`] of virtual time each.
const INTERACTIVE_SLICE_EXITS: u64 = 4_000;
/// Virtual time an interactive run hands out at once, then waits for its own
/// clock to catch up ([`Pace`]).
const PACE_NS: u64 = 10_000_000;
/// How often an interactive run redraws the screen, in its own nanoseconds.
const REDRAW_NS: u64 = 200_000_000;
const MAX_EXITS: u64 = 100_000_000;
const MAX_VIRTUAL_NS: u64 = 600_000_000_000;
/// The RTC's time at virtual time 0 (2026-09-21 12:53:20 UTC): fixed, so
/// runs differ only by host timing.
const RTC_EPOCH: i64 = 1_790_000_000;
/// The emulated local APIC timer's input clock.
const LAPIC_BUS_HZ: u64 = 1_000_000_000;
/// The guest's screen, 1024 x 768 XRGB: probe memory mapped at a guest-physical
/// address in the PCI window far above the BARs (Linux assigns them from its
/// bottom), reserved in the e820 map and reported in the zero page.
const FB: Framebuffer = Framebuffer {
    gpa: 0xFC00_0000,
    width: 1024,
    height: 768,
    stride: 4096,
};
const FB_BYTES: u64 = FB.bytes();
/// The largest display mode the NANOX screen takes: the window (4/5 of the
/// screen, `serverwin`) then shows the guest's 1024 x 768 almost unscaled.
const DISPLAY_MAX: (u32, u32) = (1280, 1024);
/// FNV-1a, 64 bits: the screen dump's check.
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;
/// COM1 output kept for the verdict (the boot log is ~100 KiB).
const SERIAL_BYTES: usize = 1 << 20;
static mut SERIAL: [u8; SERIAL_BYTES] = [0; SERIAL_BYTES];

/// Guest RAM is allocated in pieces of this size (the last one may be
/// shorter). `LOW_RAM_LIMIT` is a whole number of them, so no piece straddles
/// the low RAM's end.
pub const RAM_CHUNK: u64 = 64 << 20;
const MAX_RAM_CHUNKS: usize = (MAX_RAM_MIB << 20).div_ceil(RAM_CHUNK) as usize;
const _: () = assert!(LOW_RAM_LIMIT.is_multiple_of(RAM_CHUNK));

/// Where guest RAM lives in the host: piece `i` holds the guest RAM bytes
/// from `i * RAM_CHUNK` at `host[i]`. The first `low` bytes of guest RAM are
/// at guest-physical 0, the rest at 4 GiB.
#[derive(Clone, Copy, Debug)]
pub struct RamChunks {
    host: [u64; MAX_RAM_CHUNKS],
    size: u64,
    low: u64,
}

/// The case's guest RAM pieces (8 KiB of addresses: a static, not the stack).
static mut CHUNKS: RamChunks = RamChunks {
    host: [0; MAX_RAM_CHUNKS],
    size: 0,
    low: 0,
};

impl RamChunks {
    /// Guest RAM's byte offset of guest-physical `gpa`, if it is RAM.
    fn offset(&self, gpa: u64) -> Option<u64> {
        if gpa < self.low {
            return Some(gpa);
        }
        let high = gpa.checked_sub(HIGH_RAM_BASE)?;
        (high < self.size - self.low).then_some(self.low + high)
    }

    /// The host address of `gpa` and how many of `len` bytes from there stay
    /// in its piece (at least one), if it is RAM.
    pub fn span(&self, gpa: u64, len: u64) -> Option<(u64, u64)> {
        let off = self.offset(gpa)?;
        let i = (off / RAM_CHUNK) as usize;
        let inside = off % RAM_CHUNK;
        let piece = RAM_CHUNK.min(self.size - i as u64 * RAM_CHUNK);
        Some((self.host[i] + inside, len.clamp(1, piece - inside)))
    }

    /// Copies between guest RAM at `gpa` and probe memory, piece by piece;
    /// false (and nothing copied past the gap) where `gpa` leaves RAM.
    fn copy(&self, mut gpa: u64, len: usize, mut each: impl FnMut(u64, usize, usize)) -> bool {
        let mut done = 0;
        while done < len {
            let Some((host, n)) = self.span(gpa, (len - done) as u64) else {
                return false;
            };
            each(host, done, n as usize);
            done += n as usize;
            gpa += n;
        }
        true
    }

    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        self.copy(gpa, buf.len(), |host, at, n| {
            // SAFETY: `host..host + n` is guest RAM inside one piece (`span`),
            // firmware memory the probe allocated and identity-mapped; `buf`
            // is probe memory outside it; no Rust reference to guest RAM
            // exists while the guest does not run.
            unsafe {
                core::ptr::copy_nonoverlapping(host as *const u8, buf[at..at + n].as_mut_ptr(), n)
            }
        })
    }

    fn write(&self, gpa: u64, data: &[u8]) -> bool {
        self.copy(gpa, data.len(), |host, at, n| {
            // SAFETY: as in `read`.
            unsafe { core::ptr::copy_nonoverlapping(data[at..].as_ptr(), host as *mut u8, n) }
        })
    }
}

/// Guest RAM for the devices' DMA.
struct Dma(&'static RamChunks);

impl GuestMemory for Dma {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        self.0.read(gpa, buf)
    }

    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        self.0.write(gpa, data)
    }
}

/// Guest RAM for the loader, which writes only where its plan put things.
struct LoaderRam<'a>(&'a RamChunks);

impl guest_boot::GuestMemory for LoaderRam<'_> {
    fn write(&mut self, gpa: u64, bytes: &[u8]) {
        assert!(
            self.0.write(gpa, bytes),
            "loader write outside RAM: {gpa:#x}"
        );
    }

    fn read(&mut self, gpa: u64, out: &mut [u8]) {
        assert!(self.0.read(gpa, out), "loader read outside RAM: {gpa:#x}");
    }
}

/// Guest RAM in bytes: fw_cfg `opt/nanox/ram-mib` (whole 2 MiB, between
/// [`MIN_RAM_MIB`] and [`MAX_RAM_MIB`]), or [`DEFAULT_RAM_MIB`].
fn ram_bytes() -> u64 {
    let mut buf = [0u8; 24];
    let mib = match fwcfg::read_file("opt/nanox/ram-mib", &mut buf) {
        Ok(text) if !text.is_empty() && text.iter().all(u8::is_ascii_digit) => {
            text.iter().fold(0u64, |n, &c| {
                n.saturating_mul(10).saturating_add(u64::from(c - b'0'))
            })
        }
        _ => DEFAULT_RAM_MIB,
    };
    (mib.clamp(MIN_RAM_MIB, MAX_RAM_MIB) & !1) << 20
}

/// No disk: virtio-blk has capacity 0 and refuses every request.
struct NoDisk;

/// A disk image in memory (fw_cfg `opt/nanox/disk`): whole sectors, reads and writes counted.
struct MemDisk {
    data: &'static mut [u8],
    reads: u64,
    writes: u64,
}

impl BlockBackend for MemDisk {
    fn sectors(&self) -> u64 {
        (self.data.len() / SECTOR) as u64
    }
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        let at = sector as usize * SECTOR;
        match self.data.get(at..at + SECTOR) {
            Some(s) => {
                buf.copy_from_slice(s);
                self.reads += 1;
                true
            }
            None => false,
        }
    }
    fn write(&mut self, sector: u64, data: &[u8; SECTOR]) -> bool {
        let at = sector as usize * SECTOR;
        match self.data.get_mut(at..at + SECTOR) {
            Some(s) => {
                s.copy_from_slice(data);
                self.writes += 1;
                true
            }
            None => false,
        }
    }
    fn flush(&mut self) -> bool {
        true
    }
}

impl BlockBackend for NoDisk {
    fn sectors(&self) -> u64 {
        0
    }
    fn read(&mut self, _: u64, _: &mut [u8; SECTOR]) -> bool {
        false
    }
    fn write(&mut self, _: u64, _: &[u8; SECTOR]) -> bool {
        false
    }
    fn flush(&mut self) -> bool {
        true
    }
}

/// The network: the host's own address on it ([`HOST_IP`], as the gateway of
/// QEMU's user network), which answers ARP and ping (`vswitch::endpoint`);
/// its answers wait for the guest in a queue of [`NET_QUEUE`] frames.
struct Net {
    host: Endpoint,
    sent: u64,
    frames: [[u8; MAX_FRAME]; NET_QUEUE],
    lens: [usize; NET_QUEUE],
    /// The oldest queued frame and how many there are.
    head: usize,
    queued: usize,
    /// Answers that found the queue full.
    dropped: u64,
    /// The start of the first [`NET_KEEP`] frames the guest sent and their lengths.
    kept: [[u8; NET_KEEP_BYTES]; NET_KEEP],
    kept_lens: [usize; NET_KEEP],
}

const NET_KEEP: usize = 8;
const NET_KEEP_BYTES: usize = 64;

const HOST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x35, 0x02];
const HOST_IP: [u8; 4] = [10, 0, 2, 2];
const NET_QUEUE: usize = 4;

impl Net {
    fn new() -> Self {
        Self {
            host: Endpoint::new(HOST_MAC, HOST_IP),
            sent: 0,
            frames: [[0; MAX_FRAME]; NET_QUEUE],
            lens: [0; NET_QUEUE],
            head: 0,
            queued: 0,
            dropped: 0,
            kept: [[0; NET_KEEP_BYTES]; NET_KEEP],
            kept_lens: [0; NET_KEEP],
        }
    }
}

impl NetBackend for Net {
    fn send(&mut self, frame: &[u8]) {
        if let Some(k) = self.kept.get_mut(self.sent as usize) {
            let n = frame.len().min(NET_KEEP_BYTES);
            k[..n].copy_from_slice(&frame[..n]);
            self.kept_lens[self.sent as usize] = frame.len();
        }
        self.sent += 1;
        let mut out = [0u8; MAX_FRAME];
        let Some(n) = self.host.answer(frame, &mut out) else {
            return;
        };
        if self.queued == NET_QUEUE {
            self.dropped += 1;
            return;
        }
        let at = (self.head + self.queued) % NET_QUEUE;
        self.frames[at][..n].copy_from_slice(&out[..n]);
        self.lens[at] = n;
        self.queued += 1;
    }
    fn recv(&mut self, buf: &mut [u8; MAX_FRAME]) -> Option<usize> {
        if self.queued == 0 {
            return None;
        }
        let n = self.lens[self.head];
        buf[..n].copy_from_slice(&self.frames[self.head][..n]);
        self.head = (self.head + 1) % NET_QUEUE;
        self.queued -= 1;
        Some(n)
    }
}

/// No screen: resources are accepted, pixels dropped.
#[derive(Default)]
struct NoScreen {
    resources: u32,
}

impl Scanout for NoScreen {
    fn create(&mut self, _: u32, _: u32, _: u32, _: u32) -> bool {
        self.resources += 1;
        true
    }
    fn destroy(&mut self, _: u32) {}
    fn put(&mut self, _: u32, _: u32, _: u32, _: &[u8]) {}
    fn show(&mut self, _: u32, _: u32, _: Rect) {}
    fn present(&mut self, _: u32, _: u32, _: Rect) {}
}

/// The agent channel's host end: what the guest sends is counted and its first
/// [`AGENT_KEEP`] bytes kept; once the guest has sent a whole line, the host
/// answers with [`AGENT_ANSWER`] (the init's greeting, tools/hostguest/init)
/// and starts typing on the keyboard ([`Typist`]).
struct Agent<'a> {
    bytes: u64,
    kept: [u8; AGENT_KEEP],
    /// How much of the answer the guest has taken; None before the first line.
    answered: Option<usize>,
    typing: &'a Cell<bool>,
}

const AGENT_KEEP: usize = 256;
const AGENT_ANSWER: &[u8] = b"NANOX_HOST_HELLO from the NANOX VMM\n";

impl<'a> Agent<'a> {
    fn new(typing: &'a Cell<bool>) -> Self {
        Self {
            bytes: 0,
            kept: [0; AGENT_KEEP],
            answered: None,
            typing,
        }
    }

    fn kept(&self) -> &[u8] {
        &self.kept[..(self.bytes as usize).min(AGENT_KEEP)]
    }
}

impl ConsoleBackend for Agent<'_> {
    fn write(&mut self, data: &[u8]) {
        for &b in data {
            if let Some(k) = self.kept.get_mut(self.bytes as usize) {
                *k = b;
            }
            self.bytes += 1;
            if b == b'\n' && self.answered.is_none() {
                self.answered = Some(0);
                self.typing.set(true);
            }
        }
    }
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let Some(at) = self.answered else {
            return 0;
        };
        let n = (AGENT_ANSWER.len() - at).min(buf.len());
        buf[..n].copy_from_slice(&AGENT_ANSWER[at..at + n]);
        self.answered = Some(at + n);
        n
    }
}

/// The keyboard's host end: once the agent channel has answered the guest
/// (`start`, set by [`Agent`]), it types [`TYPED`] on the PS/2 keyboard, one
/// key (press and release) whenever the controller's buffer is empty at a poll
/// point. The init reads the line from the first virtual terminal.
struct Typist<'a> {
    start: &'a Cell<bool>,
    /// Keys typed so far.
    typed: usize,
    /// In an interactive run, the probe machine's own keyboard, handed on.
    host: Option<HostKeyboard>,
}

/// `nanox` and Enter as set-2 scancodes (the release is 0xF0 and the code).
const TYPED: [u8; 6] = [0x31, 0x1C, 0x31, 0x44, 0x22, 0x5A];
const TYPED_TEXT: &str = "nanox";

impl InputHost for Typist<'_> {
    fn poll(&mut self, m: &mut Machine, _: u64) {
        if self.start.get() && self.typed < TYPED.len() && m.kbd.queued() == 0 {
            let key = TYPED[self.typed];
            for b in [key, 0xF0, key] {
                m.key(b);
            }
            self.typed += 1;
        }
        if let Some(h) = self.host.as_mut() {
            h.poll(m);
        }
    }
}

/// The probe machine's own keyboard — QEMU's i8042, what is typed into its
/// window — handed on to the guest in an interactive run (fw_cfg
/// `opt/nanox/interactive`, run.py --show): every byte the controller has at
/// a poll point goes to the guest's i8042; set-1 bytes of a translating
/// controller are turned back into set 2 (`i8042::set1_to_set2`). The probe
/// keeps interrupts off, so the firmware's keyboard driver, which runs from
/// the timer, never takes the bytes first.
struct HostKeyboard {
    /// The controller translates to set 1 (bit 6 of its command byte).
    translated: bool,
    /// Key bytes handed on.
    bytes: u64,
}

const PS2_DATA: u16 = 0x60;
const PS2_STATUS: u16 = 0x64;
/// Status: output buffer full, and the byte is the mouse's.
const PS2_OBF: u8 = 1;
const PS2_AUX: u8 = 0x20;

impl HostKeyboard {
    fn open() -> Self {
        // Drop what waits from before, then read the command byte.
        while hw::inb(PS2_STATUS) & PS2_OBF != 0 {
            hw::inb(PS2_DATA);
        }
        hw::outb(PS2_STATUS, 0x20);
        let mut command = 0x40;
        for _ in 0..100_000 {
            if hw::inb(PS2_STATUS) & PS2_OBF != 0 {
                command = hw::inb(PS2_DATA);
                break;
            }
        }
        // The keyboard interface on.
        hw::outb(PS2_STATUS, 0xAE);
        Self {
            translated: command & 0x40 != 0,
            bytes: 0,
        }
    }

    fn poll(&mut self, m: &mut Machine) {
        // A key is a few bytes; whatever is left waits for the next poll point.
        for _ in 0..16 {
            let status = hw::inb(PS2_STATUS);
            if status & PS2_OBF == 0 {
                return;
            }
            let b = hw::inb(PS2_DATA);
            if status & PS2_AUX != 0 {
                continue;
            }
            self.bytes += 1;
            if !self.translated || matches!(b, 0xE0 | 0xE1) {
                m.key(b);
            } else if let Some(code) = set1_to_set2(b & 0x7F) {
                if b & 0x80 != 0 {
                    m.key(0xF0);
                }
                m.key(code);
            }
        }
    }
}

/// An interactive run's real time: the probe machine's TSC, measured against
/// PIT channel 2. The guest's virtual time may lag it (a busy guest runs
/// slower than a real machine) but is never let run ahead of it: an idle
/// guest would otherwise skip from one timer to the next at once, and `sleep`
/// or a blinking cursor would not take their time.
struct Pace {
    tsc_hz: u64,
    tsc0: u64,
    virtual0: u64,
}

impl Pace {
    /// Starts the clock at virtual time `virtual0`.
    fn start(virtual0: u64) -> Self {
        Self {
            tsc_hz: tsc_hz(),
            tsc0: hw::rdtsc(),
            virtual0,
        }
    }

    /// Nanoseconds since the start.
    fn elapsed_ns(&self) -> u64 {
        let ticks = hw::rdtsc().wrapping_sub(self.tsc0);
        (u128::from(ticks) * 1_000_000_000 / u128::from(self.tsc_hz.max(1))) as u64
    }

    /// Waits until as much real time has passed as `virtual_now` is past the start.
    fn wait_for(&self, virtual_now: u64) {
        let target = virtual_now.saturating_sub(self.virtual0);
        while self.elapsed_ns() < target {
            core::hint::spin_loop();
        }
    }
}

/// The probe machine's TSC rate: ticks while PIT channel 2 counts 50 ms down
/// in mode 0 (gate on at port 0x61 bit 0, OUT2 read back in its bit 5).
fn tsc_hz() -> u64 {
    const PIT_HZ: u64 = 1_193_182;
    const COUNT: u16 = 59_659;
    let gate = hw::inb(0x61);
    hw::outb(0x61, (gate & !0x02) | 0x01);
    hw::outb(0x43, 0xB0); // channel 2, low then high byte, mode 0, binary
    hw::outb(0x42, COUNT as u8);
    hw::outb(0x42, (COUNT >> 8) as u8);
    let t0 = hw::rdtsc();
    while hw::inb(0x61) & 0x20 == 0 {
        core::hint::spin_loop();
    }
    let ticks = hw::rdtsc().wrapping_sub(t0);
    hw::outb(0x61, gate);
    ticks * PIT_HZ / u64::from(COUNT)
}

/// The probe has no wall clock: its "microseconds" count the loop's
/// iterations, so a run with `max_time_us = SLICE_EXITS` returns every
/// `SLICE_EXITS` exits.
struct Slices(u64);

impl Clock for Slices {
    fn now_us(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Distinct keys with read and write counts; keys beyond `N` are only
/// counted in `dropped`.
struct Counts<K, const N: usize> {
    keys: [K; N],
    reads: [u64; N],
    writes: [u64; N],
    len: usize,
    dropped: u64,
}

impl<K: Copy + Default + PartialEq + Into<u64>, const N: usize> Counts<K, N> {
    fn new() -> Self {
        Self {
            keys: [K::default(); N],
            reads: [0; N],
            writes: [0; N],
            len: 0,
            dropped: 0,
        }
    }

    fn reads_of(&self, key: K) -> u64 {
        self.keys[..self.len]
            .iter()
            .position(|&k| k == key)
            .map_or(0, |i| self.reads[i])
    }

    fn add(&mut self, key: K, write: bool) {
        let i = match self.keys[..self.len].iter().position(|&k| k == key) {
            Some(i) => i,
            None if self.len < N => {
                self.keys[self.len] = key;
                self.len += 1;
                self.len - 1
            }
            None => {
                self.dropped += 1;
                return;
            }
        };
        if write {
            self.writes[i] += 1;
        } else {
            self.reads[i] += 1;
        }
    }

    /// `NANOX:SVM-PROBE:LINUX-<what> key=reads/writes ...`, keys ascending.
    fn print(&self, what: &str) {
        out!("NANOX:SVM-PROBE:LINUX-{what}");
        let mut order = [0usize; N];
        for (i, o) in order.iter_mut().enumerate().take(self.len) {
            *o = i;
        }
        let order = &mut order[..self.len];
        order.sort_unstable_by_key(|&i| self.keys[i].into());
        for &i in order.iter() {
            let key: u64 = self.keys[i].into();
            out!(" {key:#x}={}/{}", self.reads[i], self.writes[i]);
        }
        if self.dropped > 0 {
            out!(" dropped={}", self.dropped);
        }
        out!("\n");
    }
}

/// What the guest asked of the VMM, counted at every exit: exit codes, I/O
/// ports (reads/writes), MSRs (RDMSR/WRMSR, emulated or refused), CPUID
/// leaves and the pages of device memory (reads/writes) — the surface of
/// docs/research/linux-guest-surface.md, measured under this VMM.
struct Surface {
    exits: Counts<u64, 32>,
    ports: Counts<u16, 128>,
    msrs: Counts<u32, 64>,
    cpuid: Counts<u32, 64>,
    mmio: Counts<u64, 64>,
}

impl Surface {
    fn new() -> Self {
        Self {
            exits: Counts::new(),
            ports: Counts::new(),
            msrs: Counts::new(),
            cpuid: Counts::new(),
            mmio: Counts::new(),
        }
    }

    fn record(&mut self, vmcb: &Vmcb<'_>, g: &Gprs) {
        let exit = vmcb.exit_code();
        self.exits.add(exit, false);
        let info1 = vmcb.exit_info1();
        match exit {
            // EXITINFO1: port in 31:16, bit 0 IN.
            code::IOIO => self.ports.add((info1 >> 16) as u16, info1 & 1 == 0),
            // EXITINFO1: 1 for WRMSR.
            code::MSR => self.msrs.add(g.rcx as u32, info1 & 1 != 0),
            code::CPUID => self.cpuid.add(vmcb.rax() as u32, false),
            // Error code bit 1: a write.
            code::NPF => self.mmio.add(vmcb.exit_info2() & !0xFFF, info1 & 2 != 0),
            _ => {}
        }
    }

    fn print(&self) {
        self.exits.print("EXITS");
        self.ports.print("PORTS");
        self.msrs.print("MSRS");
        self.cpuid.print("CPUID");
        self.mmio.print("MMIO");
    }
}

/// The probe's processor with every exit counted in a [`Surface`].
struct Traced<'a> {
    cpu: &'a mut Cpu,
    surface: &'a mut Surface,
}

impl SvmCpu for Traced<'_> {
    fn read_guest_phys(&mut self, gpa: u64, out: &mut [u8]) -> bool {
        self.cpu.read_guest_phys(gpa, out)
    }

    fn vmrun(&mut self, vmcb: &mut Vmcb<'_>, g: &mut Gprs) {
        self.cpu.vmrun(vmcb, g);
        self.surface.record(vmcb, g);
    }

    fn host_cpuid(&mut self, leaf: u32, subleaf: u32) -> [u32; 4] {
        self.cpu.host_cpuid(leaf, subleaf)
    }
}

/// One line of guest output with the probe's prefix; carriage returns
/// dropped, other non-printable bytes escaped.
fn line(l: &[u8]) {
    out!("NANOX:SVM-PROBE:LINUX ");
    for &b in l {
        match b {
            b'\r' => {}
            0x20..=0x7E => Serial::byte(b),
            _ => out!("\\x{b:02x}"),
        }
    }
    out!("\n");
}

/// Prints the complete lines of `serial` from `from` on (with `all`, also a
/// trailing partial line); returns where the next call starts.
fn stream(serial: &[u8], from: usize, all: bool) -> usize {
    let mut start = from;
    for (i, &b) in serial.iter().enumerate().skip(from) {
        if b == b'\n' {
            line(&serial[start..i]);
            start = i + 1;
        }
    }
    if all && start < serial.len() {
        line(&serial[start..]);
        start = serial.len();
    }
    start
}

/// `n` pixels of firmware memory, or None if they cannot be had.
fn allocate_pixels(system: *mut u8, n: usize) -> Option<&'static mut [u32]> {
    let bytes = allocate(system, n.checked_mul(4)?)?;
    // SAFETY: fresh page-aligned firmware pages of `4 * n` bytes, owned by
    // the probe; the byte slice is consumed here, so this is the only
    // reference to them.
    Some(unsafe { core::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, n) })
}

/// The guest's framebuffer as pixels. Only for use while the guest does not
/// run, and dropped before it runs again.
fn guest_screen(fb_host: u64) -> View<'static> {
    // SAFETY: the probe's framebuffer pages (allocated in `case`, owned by the
    // probe, identity-mapped, page-aligned); the guest writes them only while
    // it runs, and the caller drops the view before it runs again.
    let px = unsafe { core::slice::from_raw_parts(fb_host as *const u32, (FB_BYTES / 4) as usize) };
    View::new(
        px,
        usize::from(FB.width),
        usize::from(FB.height),
        usize::from(FB.stride) / 4,
    )
    .expect("the framebuffer holds its rows")
}

/// What the NANOX screen shows about the run.
fn status<'a>(o: &Outcome, m: &Machine, serial: &'a [u8], running: bool) -> Status<'a> {
    const VERSION: &[u8] = b"Linux version ";
    let kernel = serial
        .windows(VERSION.len())
        .position(|w| w == VERSION)
        .map(|at| &serial[at + VERSION.len()..])
        .map(|rest| &rest[..rest.iter().position(|&b| b == b' ').unwrap_or(rest.len())]);
    Status {
        kernel,
        running,
        virtual_ms: o.virtual_ns / 1_000_000,
        exits: o.exits,
        irqs: o.irqs,
        disk_requests: m.blk.requests,
        net: (m.net.sent, m.net.received),
        agent: (m.console.bytes_out, m.console.bytes_in),
    }
}

/// `bytes` of firmware memory as a slice, or None if it cannot be had.
fn allocate(system: *mut u8, bytes: usize) -> Option<&'static mut [u8]> {
    let pages = bytes.div_ceil(PAGE_SIZE as usize).max(1);
    let at = hw::allocate_pages(system, pages)?;
    // SAFETY: fresh pages from the firmware's allocator, owned by the probe
    // from now on (it never frees them or returns to the firmware),
    // identity-mapped; this is the only reference to them.
    Some(unsafe { core::slice::from_raw_parts_mut(at as *mut u8, bytes) })
}

fn fail(env: &mut Env, why: &str) {
    env.report("CASE", "linux", false);
    out!(" setup={why}\n");
}

/// Boots the kernel; absent fw_cfg files skip the case.
pub fn case(env: &mut Env, page: &mut [u8; 4096], msrpm: &mut [u8; MSRPM_BYTES], system: *mut u8) {
    let image_file = match fwcfg::find("opt/nanox/bzimage") {
        Ok(f) => f,
        Err(e) => {
            out!("NANOX:SVM-PROBE:LINUX-KERNEL absent fw_cfg={e}\n");
            return;
        }
    };
    let Ok(initrd_file) = fwcfg::find("opt/nanox/initrd") else {
        return fail(env, "no-initrd");
    };
    let mut cmd_buf = [0u8; CMDLINE_MAX];
    let cmdline = match fwcfg::read_file("opt/nanox/cmdline", &mut cmd_buf) {
        Ok(c) => c,
        Err(_) => DEFAULT_CMDLINE,
    };
    let tick = host_tick();
    // An optional disk image for virtio-blk.
    let mut disk_image: Option<&'static mut [u8]> = None;
    if let Ok(f) = fwcfg::find("opt/nanox/disk") {
        let Some(buf) = allocate(system, f.size) else {
            return fail(env, "allocate-disk");
        };
        if fwcfg::read_into(f, buf).is_err() {
            return fail(env, "fw-cfg-disk");
        }
        out!("NANOX:SVM-PROBE:LINUX-DISK bytes={}\n", f.size);
        disk_image = Some(buf);
    }
    let ram = ram_bytes();
    // Nested tables of 4 KiB pages: a table per 2 MiB, a directory per GiB, spare.
    let npt_pages = (ram >> 21) as usize + (ram >> 30) as usize + 64;
    let (Some(npt_buf), Some(image_buf), Some(initrd_buf)) = (
        allocate(system, npt_pages * PAGE_SIZE as usize),
        allocate(system, image_file.size),
        allocate(system, initrd_file.size),
    ) else {
        return fail(env, "allocate-pages");
    };
    let npt_base = npt_buf.as_mut_ptr() as u64;
    // Guest RAM, zeroed, piece by piece.
    let chunks_ptr: *mut RamChunks = &raw mut CHUNKS;
    // SAFETY: a static of this module, borrowed once (the case runs once).
    let chunks = unsafe { &mut *chunks_ptr };
    chunks.size = ram;
    chunks.low = ram.min(LOW_RAM_LIMIT);
    for (i, host) in chunks
        .host
        .iter_mut()
        .enumerate()
        .take(ram.div_ceil(RAM_CHUNK) as usize)
    {
        let len = RAM_CHUNK.min(ram - i as u64 * RAM_CHUNK) as usize;
        let Some(piece) = allocate(system, len) else {
            return fail(env, "allocate-ram");
        };
        piece.fill(0);
        *host = piece.as_mut_ptr() as u64;
    }
    let chunks: &'static RamChunks = chunks;
    let Some(fb_buf) = allocate(system, FB_BYTES as usize) else {
        return fail(env, "allocate-framebuffer");
    };
    fb_buf.fill(0);
    // The NANOX screen on the firmware's display, if there is one.
    let gop = hw::gop(system, DISPLAY_MAX.0, DISPLAY_MAX.1);
    let mut display = gop.and_then(|g| {
        out!(
            "NANOX:SVM-PROBE:DISPLAY width={} height={} stride={} format={} base={:#x}\n",
            g.width,
            g.height,
            g.stride,
            g.format,
            g.base
        );
        let shadow = allocate_pixels(system, g.width as usize * g.height as usize)?;
        Display::open(g, shadow)
    });
    if display.is_none() {
        out!("NANOX:SVM-PROBE:DISPLAY none\n");
    }
    let fb_host = fb_buf.as_mut_ptr() as u64;
    let (Ok(image), Ok(initrd)) = (
        fwcfg::read_into(image_file, image_buf),
        fwcfg::read_into(initrd_file, initrd_buf),
    ) else {
        return fail(env, "fw-cfg-read");
    };

    // Guest RAM mapped piece by piece in the nested tables.
    let mut phys = Phys {
        lo: npt_base,
        hi: npt_base + (npt_pages as u64) * PAGE_SIZE,
    };
    let mut frames = Frames::new(npt_base, npt_pages);
    let mut npt = Npt::new(&mut phys, &mut frames, 48).expect("nested root");
    let mut off = 0;
    while off < ram {
        let len = RAM_CHUNK.min(ram - off);
        let gpa = if off < chunks.low {
            off
        } else {
            HIGH_RAM_BASE + (off - chunks.low)
        };
        let host = chunks.host[(off / RAM_CHUNK) as usize];
        npt.map(&mut phys, &mut frames, gpa, host, len, NptPerms::RWX)
            .expect("map guest RAM");
        off += len;
    }
    npt.map(
        &mut phys,
        &mut frames,
        FB.gpa,
        fb_host,
        FB_BYTES,
        NptPerms::RW,
    )
    .expect("map the framebuffer");

    // ACPI tables, then the kernel with them.
    let mut tables = [0u8; ACPI_MAX];
    let layout = acpi::build(&mut tables, ACPI_GPA, &Platform::default()).expect("ACPI tables");
    let mut loader_ram = LoaderRam(chunks);
    loader_ram.write(ACPI_GPA, &tables[..layout.len]);
    let cfg = LinuxConfig {
        ram_bytes: ram,
        cmdline,
        acpi: Some(Acpi {
            rsdp_gpa: layout.rsdp,
            region_gpa: ACPI_GPA,
            region_len: (layout.len as u64).next_multiple_of(PAGE_SIZE),
        }),
        // Linux uses an ECAM window only when it finds it reserved; the
        // framebuffer is not RAM.
        reserved: &[(ECAM_BASE, ECAM_SIZE), (FB.gpa, FB_BYTES)],
        framebuffer: Some(FB),
    };
    let entry = match linux::load_linux(&mut loader_ram, image, initrd, &cfg) {
        Ok(e) => e,
        Err(e) => {
            env.report("CASE", "linux", false);
            out!(" load_linux={e:?}\n");
            return;
        }
    };
    out!(
        "NANOX:SVM-PROBE:LINUX-BOOT image={} initrd={} ram_mib={} load={:#x} entry={:#x} initrd_gpa={:#x} e820={} acpi={:#x}+{:#x} rsdp={:#x} cmdline=\"",
        image.len(),
        initrd.len(),
        ram >> 20,
        entry.load_address,
        entry.rip,
        entry.initrd_gpa,
        entry.e820_entries,
        ACPI_GPA,
        layout.len,
        layout.rsdp
    );
    for &b in cmdline {
        Serial::byte(b);
    }
    out!("\"\n");

    PlatformVcpu::msr_policy(&mut MsrPermissionMap::intercept_all(msrpm));
    let mut vcfg = VmConfig::new(1, env.msrpm, env.iopm, npt.root(), env.nrips);
    vcfg.flush_by_asid = env.flush_by_asid;
    vcfg.exit_quantum_ns = QUANTUM_NS;
    vcfg.max_exits = MAX_EXITS;
    vcfg.max_time_us = SLICE_EXITS;
    vcfg.max_virtual_ns = MAX_VIRTUAL_NS;
    // An interactive run lasts as long as the user types, in shorter slices.
    let interactive = fwcfg::find("opt/nanox/interactive").is_ok();
    if interactive {
        vcfg.max_exits = u64::MAX;
        vcfg.max_virtual_ns = u64::MAX;
        vcfg.max_time_us = INTERACTIVE_SLICE_EXITS;
        out!("NANOX:SVM-PROBE:LINUX-INTERACTIVE the probe's keyboard goes to the guest\n");
    }
    let pace = interactive.then(|| Pace::start(0));
    if let Some(p) = &pace {
        out!("NANOX:SVM-PROBE:LINUX-PACE tsc_hz={}\n", p.tsc_hz);
    }
    let mut next_draw = 0u64;
    if let Some(ns) = tick {
        vcfg.intr_exit_ns = ns;
    }
    let serial_buf: *mut [u8; SERIAL_BYTES] = &raw mut SERIAL;
    // SAFETY: a static of this module, borrowed once (the case runs once).
    let serial = unsafe { &mut *serial_buf };
    let mut vcpu = PlatformVcpu::new(vcfg, Machine::new(RTC_EPOCH, LAPIC_BUS_HZ), serial);
    {
        let mut v = Vmcb::new(page);
        vcpu.prepare(&mut v);
        v.setup_linux_boot(entry.rip, entry.cr3, entry.gdt_base, entry.gdt_limit);
    }
    let mut gprs = Gprs {
        rsi: entry.rsi,
        ..Gprs::default()
    };
    env.cpu.ram = GuestMap::Chunked(chunks);

    let mut no_disk = NoDisk;
    let mut mem_disk = disk_image.map(|data| MemDisk {
        data,
        reads: 0,
        writes: 0,
    });
    let disk: &mut dyn BlockBackend = match mem_disk.as_mut() {
        Some(d) => d,
        None => &mut no_disk,
    };
    let typing = Cell::new(false);
    let (mut mem, mut net, mut screen, mut agent, mut input) = (
        Dma(chunks),
        Net::new(),
        NoScreen::default(),
        Agent::new(&typing),
        Typist {
            start: &typing,
            typed: 0,
            host: interactive.then(HostKeyboard::open),
        },
    );
    let mut host = Host {
        mem: &mut mem,
        disk,
        net: &mut net,
        display: &mut screen,
        console: &mut agent,
        input: &mut input,
    };
    if tick.is_some() {
        env.host_tick.start(HOST_TICK_COUNT);
        env.cpu.host_irq = true;
    }
    let mut clock = Slices(0);
    let mut shown = 0;
    let mut slices = 0u64;
    let mut surface = Surface::new();
    let o = loop {
        if pace.is_some() {
            vcpu.set_virtual_limit(vcpu.now_ns().saturating_add(PACE_NS));
        }
        let mut cpu = Traced {
            cpu: &mut env.cpu,
            surface: &mut surface,
        };
        let o = vcpu.run(
            &mut cpu,
            &mut clock,
            &mut host,
            &mut Vmcb::wrap(page),
            &mut gprs,
        );
        shown = stream(vcpu.serial(), shown, false);
        let paced = pace.is_some() && o.verdict == Verdict::VirtualTimeout;
        if o.verdict != Verdict::Timeout && !paced {
            break o;
        }
        slices += 1;
        if !interactive {
            progress(env, page, &o, slices, surface.exits.reads_of(code::INTR));
        }
        if let Some(p) = &pace {
            p.wait_for(vcpu.now_ns());
            if p.elapsed_ns() < next_draw {
                continue;
            }
            next_draw = p.elapsed_ns() + REDRAW_NS;
        }
        if let Some(d) = display.as_mut() {
            d.draw(
                &guest_screen(fb_host),
                &status(&o, vcpu.machine(), vcpu.serial(), true),
            );
        }
    };
    env.cpu.host_irq = false;
    env.host_tick.stop();
    stream(vcpu.serial(), shown, true);

    let s = vcpu.serial();
    let ended = matches!(
        o.verdict,
        Verdict::PowerOff | Verdict::Halted | Verdict::Reset
    );
    let ok = has(s, b"NANOX_GUEST_REPORT_END") && ended;
    env.report("CASE", "linux", ok);
    report(&o, vcpu.machine(), slices, &net, &screen, &agent);
    out!(
        "NANOX:SVM-PROBE:LINUX-KEYBOARD text={TYPED_TEXT} keys={}/{} host_bytes={} dropped={}\n",
        input.typed,
        TYPED.len(),
        input.host.as_ref().map_or(0, |h| h.bytes),
        vcpu.machine().kbd.dropped
    );
    surface.print();
    dump_screen("LINUX", &guest_screen(fb_host));
    if let Some(d) = display.as_mut() {
        d.draw(
            &guest_screen(fb_host),
            &status(&o, vcpu.machine(), s, false),
        );
        dump_screen("NANOX", &d.view());
    }
    if !ok {
        state(env, page, s);
    }
}

/// fw_cfg `opt/nanox/host-tick-ns`: the virtual time a host tick exit (~1 ms of
/// host time) counts, or None (`off`, the default). With a tick the run depends
/// on host timing, and Linux's TSC calibration against the PIT fails (a tick in
/// the polling loop makes one step far longer than the others); without one a
/// guest that spins without exits never ends its slice, but with the TSC
/// intercepted a Linux guest always exits.
fn host_tick() -> Option<u64> {
    let mut buf = [0u8; 24];
    match fwcfg::read_file("opt/nanox/host-tick-ns", &mut buf) {
        Ok(b"off") => None,
        Ok(text) => Some(text.iter().fold(0u64, |n, &c| {
            assert!(c.is_ascii_digit(), "host-tick-ns: not a number");
            n * 10 + u64::from(c - b'0')
        })),
        Err(_) => None,
    }
}

/// A slice ended: where the guest is.
fn progress(env: &mut Env, page: &mut [u8; 4096], o: &Outcome, slice: u64, ticks: u64) {
    let v = Vmcb::wrap(page);
    let mut insn = [0u8; hw_svm::guest::MAX_INSN];
    let n = hw_svm::guest::fetch(&mut env.cpu, &v, &mut insn);
    out!(
        "NANOX:SVM-PROBE:LINUX-PROGRESS slice={slice} exits={} host_ticks={ticks} virtual_ms={} irqs={} mmio={} msr_faults={} rip={:#x} rsp={:#x} rflags={:#x} insn=",
        o.exits,
        o.virtual_ns / 1_000_000,
        o.irqs,
        o.mmio,
        o.msr_faults,
        v.rip(),
        v.read_u64(hw_svm::vmcb::save::RSP),
        v.rflags()
    );
    for b in &insn[..n] {
        out!("{b:02x}");
    }
    out!("\n");
}

fn report(o: &Outcome, m: &Machine, slices: u64, net: &Net, screen: &NoScreen, agent: &Agent<'_>) {
    out!(
        " verdict={:?} exits={} slices={} msr_faults={} ud={} irqs={} mmio={} virtual_ms={} serial_bytes={} truncated={} unclaimed_in={} unclaimed_out={} unclaimed_mmio={} other_messages={} pit_coalesced={} blk_requests={} net_sent={} gpu_resources={} agent_bytes={}\n",
        o.verdict,
        o.exits,
        slices,
        o.msr_faults,
        o.ud_injected,
        o.irqs,
        o.mmio,
        o.virtual_ns / 1_000_000,
        o.serial_len,
        o.serial_truncated,
        m.unclaimed_in,
        m.unclaimed_out,
        m.unclaimed_mmio,
        m.other_messages,
        m.pit_coalesced,
        m.blk.requests,
        net.sent,
        screen.resources,
        agent.bytes
    );
    out!(
        "NANOX:SVM-PROBE:LINUX-NET from_guest={} arp_replies={} echo_replies={} ignored={} dropped={}\n",
        net.sent,
        net.host.arp_replies,
        net.host.echo_replies,
        net.host.ignored,
        net.dropped
    );
    for (i, k) in net.kept.iter().enumerate().take(net.sent as usize) {
        let len = net.kept_lens[i];
        out!("NANOX:SVM-PROBE:LINUX-NET-FRAME {i} len={len} ");
        for b in &k[..len.min(NET_KEEP_BYTES)] {
            out!("{b:02x}");
        }
        out!("\n");
    }
    out!(
        "NANOX:SVM-PROBE:LINUX-AGENT from_guest={} to_guest={} text=\"",
        agent.bytes,
        agent.answered.unwrap_or(0)
    );
    for &b in agent.kept() {
        match b {
            b'\n' => out!("\\n"),
            b' '..=b'~' if b != b'"' && b != b'\\' => Serial::byte(b),
            _ => out!("\\x{b:02x}"),
        }
    }
    out!("\"\n");
}

/// A screen of XRGB pixels in the log (`tag` is LINUX for the guest's,
/// NANOX for the probe's display): `NANOX:SVM-PROBE:<tag>-SCREEN width=
/// height=`, then one `NANOX:SVM-PROBE:<tag>-FB` line per pixel row, its runs
/// of equal pixels as ` <count>:<rrggbb>` (hex) or ` =` for a row equal to
/// the one before, then `NANOX:SVM-PROBE:<tag>-SCREEN-END fnv=` with the
/// FNV-1a hash of every pixel's red, green and blue byte. run.py makes a PNG
/// of it.
fn dump_screen(tag: &str, v: &View<'_>) {
    let (w, h) = (v.width(), v.height());
    let rgb = |row: &[u32], x: usize| row[x] & 0xFF_FFFF;
    out!("NANOX:SVM-PROBE:{tag}-SCREEN width={w} height={h}\n");
    let mut hash = FNV_OFFSET;
    let mut prev: Option<&[u32]> = None;
    for y in 0..h {
        let row = v.row(y);
        for &p in row {
            for shift in [16, 8, 0] {
                hash = (hash ^ u64::from((p >> shift) & 0xFF)).wrapping_mul(FNV_PRIME);
            }
        }
        out!("NANOX:SVM-PROBE:{tag}-FB");
        if prev.is_some_and(|p| (0..w).all(|x| rgb(p, x) == rgb(row, x))) {
            out!(" =\n");
            continue;
        }
        let mut x = 0;
        while x < w {
            let px = rgb(row, x);
            let n = (x..w).take_while(|&i| rgb(row, i) == px).count();
            out!(" {n:x}:{px:06x}");
            x += n;
        }
        out!("\n");
        prev = Some(row);
    }
    out!("NANOX:SVM-PROBE:{tag}-SCREEN-END fnv={hash:016x}\n");
}

/// The vCPU state at the end and the last guest output.
fn state(env: &mut Env, page: &mut [u8; 4096], serial: &[u8]) {
    let v = Vmcb::wrap(page);
    let mut insn = [0u8; hw_svm::guest::MAX_INSN];
    let n = hw_svm::guest::fetch(&mut env.cpu, &v, &mut insn);
    out!(
        "NANOX:SVM-PROBE:STATE linux rip={:#x} rsp={:#x} cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x} rflags={:#x} exit={:#x} info1={:#x} info2={:#x} exitintinfo={:#x} insn=",
        v.rip(),
        v.read_u64(hw_svm::vmcb::save::RSP),
        v.read_u64(hw_svm::vmcb::save::CR0),
        v.read_u64(hw_svm::vmcb::save::CR3),
        v.read_u64(hw_svm::vmcb::save::CR4),
        v.read_u64(hw_svm::vmcb::save::EFER),
        v.rflags(),
        v.exit_code(),
        v.exit_info1(),
        v.exit_info2(),
        v.read_u64(ctl::EXIT_INT_INFO)
    );
    for b in &insn[..n] {
        out!("{b:02x}");
    }
    out!("\n");
    let tail = serial.len().saturating_sub(1024);
    out!("NANOX:SVM-PROBE:LINUX-LAST\n");
    stream(&serial[tail..], 0, true);
}
