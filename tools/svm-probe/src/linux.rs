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
//! guest-physical address is device memory.
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
//! (`NANOX:SVM-PROBE:LINUX-SCREEN`, see [`dump_screen`]).

use crate::hw::Serial;
use crate::{fwcfg, has, hw, Cpu, Env, Frames, GuestMap, GuestRam, Phys, HOST_TICK_COUNT};
use core::fmt::Write;
use guest_boot::linux::{self, Acpi, Framebuffer, LinuxConfig};
use guest_boot::GuestMemory as _;
use hw_svm::exit::code;
use hw_svm::perm::{MsrPermissionMap, MSRPM_BYTES};
use hw_svm::platform_vm::{Host, InputHost, PlatformVcpu};
use hw_svm::vmcb::{ctl, Gprs};
use hw_svm::vmm::{Outcome, Verdict, VmConfig};
use hw_svm::{Clock, Npt, NptPerms, SvmCpu, Vmcb, PAGE_SIZE};
use vmm_devices::acpi::{self, Platform};
use vmm_devices::machine::{Machine, ECAM_BASE, ECAM_SIZE};
use vmm_devices::virtio::GuestMemory;
use vmm_devices::virtio_blk::{BlockBackend, SECTOR};
use vmm_devices::virtio_console::ConsoleBackend;
use vmm_devices::virtio_gpu::{Rect, Scanout};
use vmm_devices::virtio_net::{NetBackend, MAX_FRAME};

/// Guest RAM: enough for the kernel (init_size ~62 MiB from 16 MiB), its
/// memory map and the initramfs at the top.
pub const RAM: u64 = 256 << 20;
/// Frames for the nested tables of `RAM` in 4 KiB pages (128 page tables and
/// three upper levels) with room to spare.
const NPT_PAGES: usize = 256;
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
/// FNV-1a, 64 bits: the screen dump's check.
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;
/// COM1 output kept for the verdict (the boot log is ~100 KiB).
const SERIAL_BYTES: usize = 1 << 20;
static mut SERIAL: [u8; SERIAL_BYTES] = [0; SERIAL_BYTES];

/// Guest RAM for the devices' DMA: `size` bytes from host-physical `base`.
struct Dma {
    base: u64,
    size: u64,
}

impl Dma {
    fn inside(&self, gpa: u64, len: usize) -> bool {
        gpa.checked_add(len as u64).is_some_and(|e| e <= self.size)
    }
}

impl GuestMemory for Dma {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        if !self.inside(gpa, buf.len()) {
            return false;
        }
        // SAFETY: inside this case's guest RAM (checked), firmware memory the
        // probe allocated and identity-mapped; `buf` is probe memory outside
        // it; no Rust reference to guest RAM exists during the run.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (self.base + gpa) as *const u8,
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        true
    }

    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        if !self.inside(gpa, data.len()) {
            return false;
        }
        // SAFETY: as in `read`.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), (self.base + gpa) as *mut u8, data.len())
        };
        true
    }
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

/// No network: frames the guest sends are counted and dropped.
#[derive(Default)]
struct NoNet {
    sent: u64,
}

impl NetBackend for NoNet {
    fn send(&mut self, _: &[u8]) {
        self.sent += 1;
    }
    fn recv(&mut self, _: &mut [u8; MAX_FRAME]) -> Option<usize> {
        None
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
/// answers with [`AGENT_ANSWER`] (the init's greeting, tools/hostguest/init).
struct Agent {
    bytes: u64,
    kept: [u8; AGENT_KEEP],
    /// How much of the answer the guest has taken; None before the first line.
    answered: Option<usize>,
}

const AGENT_KEEP: usize = 256;
const AGENT_ANSWER: &[u8] = b"NANOX_HOST_HELLO from the NANOX VMM\n";

impl Agent {
    fn new() -> Self {
        Self {
            bytes: 0,
            kept: [0; AGENT_KEEP],
            answered: None,
        }
    }

    fn kept(&self) -> &[u8] {
        &self.kept[..(self.bytes as usize).min(AGENT_KEEP)]
    }
}

impl ConsoleBackend for Agent {
    fn write(&mut self, data: &[u8]) {
        for &b in data {
            if let Some(k) = self.kept.get_mut(self.bytes as usize) {
                *k = b;
            }
            self.bytes += 1;
            if b == b'\n' && self.answered.is_none() {
                self.answered = Some(0);
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

/// No host input.
struct NoInput;

impl InputHost for NoInput {
    fn poll(&mut self, _: &mut Machine, _: u64) {}
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
    let ram_pages = (RAM / PAGE_SIZE) as usize;
    let region = (ram_pages + NPT_PAGES) * PAGE_SIZE as usize;
    let (Some(region_buf), Some(image_buf), Some(initrd_buf)) = (
        allocate(system, region),
        allocate(system, image_file.size),
        allocate(system, initrd_file.size),
    ) else {
        return fail(env, "allocate-pages");
    };
    let base = region_buf.as_mut_ptr() as u64;
    let Some(fb_buf) = allocate(system, FB_BYTES as usize) else {
        return fail(env, "allocate-framebuffer");
    };
    fb_buf.fill(0);
    let fb_host = fb_buf.as_mut_ptr() as u64;
    let (Ok(image), Ok(initrd)) = (
        fwcfg::read_into(image_file, image_buf),
        fwcfg::read_into(initrd_file, initrd_buf),
    ) else {
        return fail(env, "fw-cfg-read");
    };

    // Guest RAM zeroed and mapped; the nested tables after it.
    let mut phys = Phys {
        lo: base,
        hi: base + region as u64,
    };
    for i in 0..ram_pages {
        phys.fill(base + (i as u64) * PAGE_SIZE, 0);
    }
    let mut frames = Frames::new(base + RAM, NPT_PAGES);
    let mut npt = Npt::new(&mut phys, &mut frames, 48).expect("nested root");
    npt.map(&mut phys, &mut frames, 0, base, RAM, NptPerms::RWX)
        .expect("map guest RAM");
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
    let mut ram = GuestRam {
        phys: &mut phys,
        base,
        size: RAM,
    };
    ram.write(ACPI_GPA, &tables[..layout.len]);
    let cfg = LinuxConfig {
        ram_bytes: RAM,
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
    let entry = match linux::load_linux(&mut ram, image, initrd, &cfg) {
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
        RAM >> 20,
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
    env.cpu.ram = GuestMap::Contig { base, size: RAM };

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
    let (mut mem, mut net, mut screen, mut agent, mut input) = (
        Dma { base, size: RAM },
        NoNet::default(),
        NoScreen::default(),
        Agent::new(),
        NoInput,
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
        if o.verdict != Verdict::Timeout {
            break o;
        }
        slices += 1;
        progress(env, page, &o, slices, surface.exits.reads_of(code::INTR));
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
    surface.print();
    // SAFETY: the probe's framebuffer pages (allocated above, owned by the
    // probe, identity-mapped); the guest no longer runs, and no other
    // reference to them exists.
    dump_screen(unsafe { core::slice::from_raw_parts(fb_host as *const u8, FB_BYTES as usize) });
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

fn report(o: &Outcome, m: &Machine, slices: u64, net: &NoNet, screen: &NoScreen, agent: &Agent) {
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

/// The guest's screen: `NANOX:SVM-PROBE:LINUX-SCREEN width= height=`, then
/// one `NANOX:SVM-PROBE:LINUX-FB` line per pixel row, its runs of equal
/// pixels as ` <count>:<rrggbb>` (hex) or ` =` for a row equal to the one
/// before, then `NANOX:SVM-PROBE:LINUX-SCREEN-END fnv=` with the FNV-1a hash
/// of every pixel's red, green and blue byte. run.py makes a PNG of it.
fn dump_screen(fb: &[u8]) {
    let (w, h) = (usize::from(FB.width), usize::from(FB.height));
    let stride = usize::from(FB.stride);
    let rgb =
        |row: &[u8], x: usize| u32::from_le_bytes([row[4 * x], row[4 * x + 1], row[4 * x + 2], 0]);
    out!("NANOX:SVM-PROBE:LINUX-SCREEN width={w} height={h}\n");
    let mut hash = FNV_OFFSET;
    let mut prev: Option<&[u8]> = None;
    for y in 0..h {
        let row = &fb[y * stride..y * stride + 4 * w];
        for x in 0..w {
            for b in [2, 1, 0] {
                hash = (hash ^ u64::from(row[4 * x + b])).wrapping_mul(FNV_PRIME);
            }
        }
        out!("NANOX:SVM-PROBE:LINUX-FB");
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
    out!("NANOX:SVM-PROBE:LINUX-SCREEN-END fnv={hash:016x}\n");
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
