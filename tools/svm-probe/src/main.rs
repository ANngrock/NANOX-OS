//! svm-probe: runs `hw-svm` on a real SVM implementation (QEMU TCG with
//! `+svm,+npt` under OVMF, later the target Ryzen) instead of the scripted
//! processor of the host tests (docs/specs/M10-VMM.md).
//!
//! A UEFI application at CPL 0; it never calls firmware and ends through
//! isa-debug-exit: 0x10 (status 33) when every case matches, 0x11 (status
//! 35) otherwise or when SVM is unavailable. Output lines on COM1:
//!
//! * `NANOX:SVM-PROBE:CASE <name> PASS|FAIL ...` — a guest program run by
//!   `Vcpu` and its expected verdict;
//! * `NANOX:SVM-PROBE:CHECK <name> ...` — one VMRUN consistency violation:
//!   whether `Vmcb::check` names it (required) and what the processor does
//!   with it (recorded, compared, not required);
//! * `NANOX:SVM-PROBE:RESULT PASS|FAIL`.
//!
//! Guest layout of the small programs (guest-physical, 16 pages of RAM from
//! the probe's pool): 0 zero page (the guest IDT: every vector not
//! present), 1 code, 2..4 guest page tables identity-mapping 2 MiB, 5
//! scratch, 6 read-only in the nested tables, 7 stack, 9 remapped by the
//! remap case.
//!
//! `m0-*` cases boot a real NANOX kernel ELF (fw_cfg `opt/nanox/kernel.elf`)
//! as a guest with 4 MiB of RAM: `guest-boot` builds the M0 handoff, and
//! the kernel's own BootInfo validation decides whether it was right.
//!
//! The `linux` case (fw_cfg `opt/nanox/bzimage` and `opt/nanox/initrd`, when
//! the runner passes them) boots a Linux kernel on the whole emulated
//! platform (`hw_svm::platform_vm`): see `linux.rs`.

#![no_std]
#![no_main]

mod fwcfg;
mod guest;
mod hw;

use core::fmt::Write;
use hw::Serial;
use hw_svm::caps::{CpuidMsr, SvmCaps};
use hw_svm::perm::{IoPermissionMap, MsrPermissionMap, IOPM_BYTES, MSRPM_BYTES};
use hw_svm::vmcb::{attr, bits, ctl, misc2, save, tlb, Gprs, StateError};
use hw_svm::vmm::{Outcome, Vcpu, Verdict, VmConfig};
use hw_svm::{Clock, Error, FrameAlloc, Npt, NptPerms, PhysMem, SvmCpu, Vmcb, PAGE_SIZE};

const PAGE: usize = 4096;
/// HSAVE, host VMSAVE area, VMCB, MSRPM (2), IOPM (3).
const FIXED_PAGES: usize = 8;
/// Kernel guest RAM plus nested tables.
const FRAME_PAGES: usize = KERNEL_RAM_PAGES + 64;
const POOL_PAGES: usize = FIXED_PAGES + FRAME_PAGES;
/// One spare page so the pool can be aligned at run time.
static mut POOL: [u8; (POOL_PAGES + 1) * PAGE] = [0; (POOL_PAGES + 1) * PAGE];
/// The candidate kernel ELF from fw_cfg.
static mut ELF: [u8; 1 << 20] = [0; 1 << 20];
/// Serial output of a kernel guest (M1 trace scenarios print ~20 KiB).
static mut KERNEL_SERIAL: [u8; 64 * 1024] = [0; 64 * 1024];
const KERNEL_RAM: u64 = 4 << 20;
const KERNEL_RAM_PAGES: usize = (KERNEL_RAM / PAGE_SIZE) as usize;

const RAM_PAGES: usize = 16;
const ENTRY: u64 = 0x1000;
const GUEST_CR3: u64 = 0x2000;
const GUEST_STACK: u64 = 0x8000;
const RO_PAGE: usize = 6;
const REMAP_PAGE: usize = 9;
/// Page directory for 3..4 GiB, mapping the local APIC's 2 MiB page.
const APIC_PD_PAGE: usize = 11;

const MSR_EFER: u32 = 0xC000_0080;
const MSR_VM_CR: u32 = 0xC001_0114;
const MSR_VM_HSAVE_PA: u32 = 0xC001_0117;
const VMEXIT_HLT: u64 = 0x78;
/// VMEXIT_INVALID: -1 in the architecture, 0xFFFF_FFFF from QEMU 9.2 TCG.
fn is_invalid(exit: u64) -> bool {
    exit as u32 == u32::MAX
}

const PASS: Verdict = Verdict::DebugExit {
    value: 0x10,
    status: 33,
};

macro_rules! out {
    ($($t:tt)*) => {{
        let _ = write!(Serial, $($t)*);
    }};
}

mod linux;
mod screen;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    out!("NANOX:SVM-PROBE:PANIC {}", info.message());
    if let Some(l) = info.location() {
        out!(" at {}:{}", l.file(), l.line());
    }
    out!("\nNANOX:SVM-PROBE:RESULT FAIL\n");
    hw::exit(0x11)
}

/// Frames of the pool's frame region; `PhysMem` access is confined to it.
struct Phys {
    lo: u64,
    hi: u64,
}

impl Phys {
    fn check(&self, pa: u64, len: u64) {
        let end = pa.checked_add(len);
        assert!(
            pa >= self.lo && end.is_some_and(|e| e <= self.hi),
            "physical access outside the frame region: {pa:#x}+{len:#x}"
        );
    }

    fn fill(&mut self, pa: u64, byte: u8) {
        self.check(pa, PAGE_SIZE);
        // SAFETY: inside the probe-owned frame region (checked); identity
        // mapped; no Rust reference to frame memory exists.
        unsafe { core::ptr::write_bytes(pa as *mut u8, byte, PAGE) }
    }

    fn copy(&mut self, pa: u64, data: &[u8]) {
        self.check(pa, data.len() as u64);
        // SAFETY: as in `fill`; the source is probe memory outside the frame
        // region (.text, the ELF buffer or a local).
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), pa as *mut u8, data.len()) }
    }

    fn read_into(&mut self, pa: u64, out: &mut [u8]) {
        self.check(pa, out.len() as u64);
        // SAFETY: as in `fill`; `out` is probe memory outside the region.
        unsafe { core::ptr::copy_nonoverlapping(pa as *const u8, out.as_mut_ptr(), out.len()) }
    }
}

/// Guest RAM of a kernel guest: `size` bytes of contiguous frames from `base`.
struct GuestRam<'a> {
    phys: &'a mut Phys,
    base: u64,
    size: u64,
}

impl GuestRam<'_> {
    fn check(&self, gpa: u64, len: usize) {
        assert!(
            gpa.checked_add(len as u64).is_some_and(|e| e <= self.size),
            "guest access outside RAM: {gpa:#x}+{len:#x}"
        );
    }
}

impl guest_boot::GuestMemory for GuestRam<'_> {
    fn write(&mut self, gpa: u64, bytes: &[u8]) {
        self.check(gpa, bytes.len());
        self.phys.copy(self.base + gpa, bytes);
    }

    fn read(&mut self, gpa: u64, out: &mut [u8]) {
        self.check(gpa, out.len());
        self.phys.read_into(self.base + gpa, out);
    }
}

impl PhysMem for Phys {
    fn read_u64(&mut self, pa: u64) -> u64 {
        self.check(pa, 8);
        assert!(pa.is_multiple_of(8), "unaligned read {pa:#x}");
        // SAFETY: as in `fill`.
        unsafe { (pa as *const u64).read_volatile() }
    }

    fn write_u64(&mut self, pa: u64, value: u64) {
        self.check(pa, 8);
        assert!(pa.is_multiple_of(8), "unaligned write {pa:#x}");
        // SAFETY: as in `fill`.
        unsafe { (pa as *mut u64).write_volatile(value) }
    }
}

/// Frees the bump allocator keeps for reuse (only the error paths of
/// `Npt::map` free frames).
const FREE_SLOTS: usize = 64;

/// Bump allocator over `pages` frames from `base`, reset for every guest.
struct Frames {
    base: u64,
    pages: usize,
    next: usize,
    free: [u64; FREE_SLOTS],
    nfree: usize,
}

impl Frames {
    const fn new(base: u64, pages: usize) -> Self {
        Self {
            base,
            pages,
            next: 0,
            free: [0; FREE_SLOTS],
            nfree: 0,
        }
    }

    fn reset(&mut self) {
        self.next = 0;
        self.nfree = 0;
    }
}

impl FrameAlloc for Frames {
    fn alloc_frame(&mut self) -> Option<u64> {
        if self.nfree > 0 {
            self.nfree -= 1;
            return Some(self.free[self.nfree]);
        }
        if self.next == self.pages {
            return None;
        }
        self.next += 1;
        Some(self.base + ((self.next - 1) * PAGE) as u64)
    }

    fn free_frame(&mut self, pa: u64) {
        assert!(self.nfree < FREE_SLOTS, "too many frees ({pa:#x})");
        self.free[self.nfree] = pa;
        self.nfree += 1;
    }
}

/// Where the current guest's RAM lives, for `read_guest_phys`.
#[derive(Clone, Copy)]
enum GuestMap {
    None,
    /// Small guests: one frame per guest page.
    Pages([u64; RAM_PAGES]),
    /// The kernel guest: `size` bytes from `base`.
    Contig {
        base: u64,
        size: u64,
    },
    /// The Linux guest: pieces wherever the firmware had them.
    Chunked(&'static linux::RamChunks),
}

struct Cpu {
    host_save: u64,
    ram: GuestMap,
    /// Run the guest with host IF=1 (the host tick is running).
    host_irq: bool,
}

impl SvmCpu for Cpu {
    fn read_guest_phys(&mut self, gpa: u64, out: &mut [u8]) -> bool {
        for (i, b) in out.iter_mut().enumerate() {
            let a = gpa + i as u64;
            let hpa = match self.ram {
                GuestMap::Pages(p) => match p.get((a >> 12) as usize) {
                    Some(f) => f + (a & 0xFFF),
                    None => return false,
                },
                GuestMap::Contig { base, size } if a < size => base + a,
                GuestMap::Chunked(c) => match c.span(a, 1) {
                    Some((h, _)) => h,
                    None => return false,
                },
                _ => return false,
            };
            // SAFETY: `hpa` is in a frame of the probe's pool that backs
            // this guest's RAM; memory is identity-mapped and a byte read
            // has no alignment requirement.
            *b = unsafe { (hpa as *const u8).read_volatile() };
        }
        true
    }

    fn vmrun(&mut self, vmcb: &mut Vmcb<'_>, g: &mut Gprs) {
        let mut r = [
            g.rbx, g.rcx, g.rdx, g.rsi, g.rdi, g.rbp, g.r8, g.r9, g.r10, g.r11, g.r12, g.r13,
            g.r14, g.r15,
        ];
        // SAFETY: the VMCB and host save area are probe-owned pages, SVM
        // and VM_HSAVE_PA were enabled in `efi_main`, and every guest's
        // nested tables map only frames of the probe's frame region.
        unsafe { hw::vmrun(&mut r, vmcb.as_mut_ptr(), self.host_save, self.host_irq) };
        *g = Gprs {
            rbx: r[0],
            rcx: r[1],
            rdx: r[2],
            rsi: r[3],
            rdi: r[4],
            rbp: r[5],
            r8: r[6],
            r9: r[7],
            r10: r[8],
            r11: r[9],
            r12: r[10],
            r13: r[11],
            r14: r[12],
            r15: r[13],
        };
    }

    fn host_cpuid(&mut self, leaf: u32, subleaf: u32) -> [u32; 4] {
        hw::cpuid(leaf, subleaf)
    }
}

/// No time source: runs are bounded by `max_exits` and by the runner's
/// QEMU timeout.
struct NoClock;

impl Clock for NoClock {
    fn now_us(&mut self) -> u64 {
        0
    }
}

struct Guest {
    npt: Npt,
    ram: [u64; RAM_PAGES],
    cfg: VmConfig,
}

struct Env {
    phys: Phys,
    frames: Frames,
    cpu: Cpu,
    msrpm: u64,
    iopm: u64,
    nrips: bool,
    flush_by_asid: bool,
    failures: u32,
    host_tick: hw::HostTick,
}

impl Env {
    /// Fresh guest RAM, nested tables and guest page tables with `code` at
    /// the entry point.
    fn guest(&mut self, code: &[u8]) -> Guest {
        assert!(code.len() <= PAGE, "guest program too large");
        self.frames.reset();
        let mut npt = Npt::new(&mut self.phys, &mut self.frames, 48).expect("nested root");
        let mut ram = [0; RAM_PAGES];
        for (i, slot) in ram.iter_mut().enumerate() {
            let f = self.frames.alloc_frame().expect("guest RAM frame");
            self.phys.fill(f, 0);
            let perms = if i == RO_PAGE {
                NptPerms::RO
            } else {
                NptPerms::RWX
            };
            npt.map(
                &mut self.phys,
                &mut self.frames,
                (i * PAGE) as u64,
                f,
                PAGE_SIZE,
                perms,
            )
            .expect("map guest RAM");
            *slot = f;
        }
        // PML4 -> PDPT -> PD with one 2 MiB page at 0 (P, RW; PS in the PD).
        self.phys.write_u64(ram[2], 0x3000 | 3);
        self.phys.write_u64(ram[3], 0x4000 | 3);
        self.phys.write_u64(ram[4], 0x83);
        // PDPT[3] -> PD at page 11 with the 2 MiB page holding the local
        // APIC (0xFEE00000), which the nested tables leave unmapped.
        self.phys
            .write_u64(ram[3] + 8 * 3, (APIC_PD_PAGE * PAGE) as u64 | 3);
        self.phys
            .write_u64(ram[APIC_PD_PAGE] + 8 * 0x1F7, 0xFEE0_0000 | 0x83);
        self.phys.copy(ram[1], code);
        self.cpu.ram = GuestMap::Pages(ram);
        let mut cfg = VmConfig::new(1, self.msrpm, self.iopm, npt.root(), self.nrips);
        cfg.flush_by_asid = self.flush_by_asid;
        cfg.max_exits = 10_000;
        cfg.max_time_us = u64::MAX;
        Guest { npt, ram, cfg }
    }

    fn report(&mut self, kind: &str, name: &str, ok: bool) {
        out!(
            "NANOX:SVM-PROBE:{kind} {name} {}",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            self.failures += 1;
        }
    }
}

fn enter(page: &mut [u8; 4096], vcpu: &Vcpu<'_>) {
    let mut v = Vmcb::new(page);
    v.setup_long_mode(ENTRY, GUEST_CR3, GUEST_STACK);
    vcpu.prepare(&mut v);
}

/// FNV-1a over everything a deterministic run must reproduce: the verdict
/// and every counter, then the serial bytes. Printed per case so two runs
/// (or two machines) can be compared line by line.
struct Fnv(u64);

impl Fnv {
    fn bytes(&mut self, data: &[u8]) {
        for &b in data {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

impl core::fmt::Write for Fnv {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.bytes(s.as_bytes());
        Ok(())
    }
}

fn digest(o: &Outcome, serial: &[u8]) -> u64 {
    let mut h = Fnv(0xCBF2_9CE4_8422_2325);
    let _ = write!(
        h,
        "{:?}|{}|{}|{}|{}|{}|{}|",
        o.verdict, o.exits, o.msr_faults, o.ud_injected, o.irqs, o.mmio, o.virtual_ns
    );
    h.bytes(serial);
    h.0
}

fn print_outcome(o: &Outcome, serial: &[u8]) {
    out!(
        " verdict={:?} exits={} msr_faults={} ud={} irqs={} mmio={} virtual_us={} digest={:016x} serial=\"",
        o.verdict,
        o.exits,
        o.msr_faults,
        o.ud_injected,
        o.irqs,
        o.mmio,
        o.virtual_ns / 1000,
        digest(o, serial)
    );
    for &b in serial {
        match b {
            b'\n' => out!("\\n"),
            b'"' | b'\\' => out!("\\x{b:02x}"),
            0x20..=0x7E => Serial::byte(b),
            _ => out!("\\x{b:02x}"),
        }
    }
    out!("\"\n");
}

type Expect = fn(&Outcome, &[u8], &mut Phys, &Guest) -> bool;

fn case(env: &mut Env, page: &mut [u8; 4096], name: &str, code: &[u8], expect: Expect) {
    let guest = env.guest(code);
    let mut serial = [0u8; 128];
    let mut vcpu = Vcpu::new(guest.cfg, &mut serial);
    enter(page, &vcpu);
    let mut gprs = Gprs::default();
    let o = vcpu.run(&mut env.cpu, &mut NoClock, &mut Vmcb::wrap(page), &mut gprs);
    let ok = expect(&o, vcpu.serial(), &mut env.phys, &guest);
    env.report("CASE", name, ok);
    print_outcome(&o, vcpu.serial());
    if !ok {
        let v = Vmcb::wrap(page);
        let scratch = env.phys.read_u64(guest.ram[5]);
        out!(
            "NANOX:SVM-PROBE:STATE {name} rip={:#x} rflags={:#x} shadow={:#x} exit={:#x} info1={:#x} info2={:#x} exitintinfo={:#x} eventinj={:#x} vintr={:#x} rbx={:#x} rcx={:#x} rdx={:#x} scratch={:#x}\n",
            v.rip(),
            v.rflags(),
            v.read_u64(ctl::INTERRUPT_SHADOW),
            v.exit_code(),
            v.exit_info1(),
            v.exit_info2(),
            v.read_u64(ctl::EXIT_INT_INFO),
            v.event_inj(),
            v.read_u64(ctl::VINTR),
            gprs.rbx,
            gprs.rcx,
            gprs.rdx,
            scratch
        );
    }
}

fn cases(env: &mut Env, page: &mut [u8; 4096]) {
    case(env, page, "basic", guest::basic(), |o, s, phys, g| {
        o.verdict == PASS
            && s == b"NANOX:GUEST:HELLO\n"
            && o.msr_faults == 0
            && o.ud_injected == 0
            && phys.read_u64(g.ram[5]) == 0x5A5A_A5A5_1234_5678
    });
    case(env, page, "fail", guest::fail(), |o, _, _, _| {
        o.verdict
            == Verdict::DebugExit {
                value: 0x11,
                status: 35,
            }
    });
    case(
        env,
        page,
        "npf-absent",
        guest::npf_absent(),
        |o, _, _, _| {
            matches!(o.verdict, Verdict::NestedPageFault { gpa: 0x10_0000, error }
            if error & 1 == 0 && error & (1 << 32) != 0)
        },
    );
    case(
        env,
        page,
        "npf-readonly",
        guest::npf_readonly(),
        |o, _, _, _| {
            matches!(o.verdict, Verdict::NestedPageFault { gpa: 0x6000, error }
            if error & 3 == 3)
        },
    );
    case(
        env,
        page,
        "triple-fault",
        guest::triple_fault(),
        |o, _, _, _| o.verdict == Verdict::Shutdown,
    );
    case(env, page, "halt", guest::halt(), |o, _, _, _| {
        o.verdict == Verdict::Halted
    });
    case(
        env,
        page,
        "msr-denied",
        guest::msr_denied(),
        |o, _, _, _| o.verdict == Verdict::Shutdown && o.msr_faults == 1,
    );
    case(env, page, "vmmcall", guest::vmmcall(), |o, _, _, _| {
        o.verdict == Verdict::Shutdown && o.ud_injected == 1
    });
    // APIC MMIO (no decode assists in TCG: fetched through the guest page
    // tables), five periodic timer interrupts from HLT on virtual time,
    // the timer measured against PIT channel 2.
    case(env, page, "apic-timer", guest::timer(), |o, _, _, _| {
        o.verdict == PASS && o.irqs == 5 && o.mmio >= 12
    });
    remap(env, page);
}

/// Stops the guest, replaces the frame behind GPA 0x9000 and resumes: the
/// guest must see the new contents (unmap token -> TLB flush on VMRUN).
fn remap(env: &mut Env, page: &mut [u8; 4096]) {
    let mut g = env.guest(guest::remap());
    env.phys.fill(g.ram[REMAP_PAGE], b'A');
    let mut serial = [0u8; 16];
    let mut vcpu = Vcpu::new(g.cfg, &mut serial);
    enter(page, &vcpu);
    let mut gprs = Gprs::default();
    let first = vcpu.run(&mut env.cpu, &mut NoClock, &mut Vmcb::wrap(page), &mut gprs);
    let gpa = (REMAP_PAGE * PAGE) as u64;
    let token = g.npt.unmap(&mut env.phys, gpa, PAGE_SIZE).expect("unmap");
    let fresh = env.frames.alloc_frame().expect("frame");
    env.phys.fill(fresh, b'B');
    g.npt
        .map(
            &mut env.phys,
            &mut env.frames,
            gpa,
            fresh,
            PAGE_SIZE,
            NptPerms::RWX,
        )
        .expect("remap");
    vcpu.note_unmap(token);
    let second = vcpu.run(&mut env.cpu, &mut NoClock, &mut Vmcb::wrap(page), &mut gprs);
    let ok = first.verdict == PASS && second.verdict == PASS && vcpu.serial() == b"AB";
    env.report("CASE", "remap", ok);
    print_outcome(&second, vcpu.serial());
}

fn has(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

type KernelExpect = fn(&Outcome, &[u8]) -> bool;
type Corrupt = fn(&mut GuestRam<'_>, &guest_boot::Entry);

/// How a kernel guest is booted.
#[derive(Clone, Copy)]
struct Boot {
    test: bool,
    epoch: u64,
    protocol: guest_boot::Protocol,
    /// Virtual time per exit.
    quantum_ns: u64,
    max_exits: u64,
    /// Run a ~1 ms host timer so a guest spinning without exits still
    /// sees time pass (each host interrupt exit counts as 1 ms). Makes the
    /// run depend on host timing.
    host_tick: bool,
}

/// Host APIC timer count at bus/16 for ~1 ms under QEMU (1 GHz APIC bus).
const HOST_TICK_COUNT: u32 = 62_500;
const HOST_TICK_NS: u64 = 1_000_000;

const fn m0(test: bool, epoch: u64) -> Boot {
    Boot {
        test,
        epoch,
        protocol: guest_boot::Protocol::M0,
        quantum_ns: 1_000,
        max_exits: 100_000,
        host_tick: false,
    }
}

/// M1 calibrates against 10 ms and verifies over ten 50 ms PIT windows,
/// polled with one exit per read, and its preemption threads spin on
/// PAUSE: 50 us per exit keeps a run to ~15k exits under TCG while the
/// calibration still resolves the APIC rate to ~0.5%.
const fn m1(test: bool, epoch: u64) -> Boot {
    Boot {
        test,
        epoch,
        protocol: guest_boot::Protocol::M1,
        quantum_ns: 50_000,
        max_exits: 3_000_000,
        // Runs on virtual time alone, hence reproducible bit for bit.
        host_tick: false,
    }
}

/// The preemption test's observer thread spins on plain loads without any
/// exit, so time only passes with the host tick: this one case depends on
/// host timing and is not reproducible (docs/research/REPRODUCIBILITY.md).
const fn m1_host_timed(test: bool, epoch: u64) -> Boot {
    Boot {
        host_tick: true,
        ..m1(test, epoch)
    }
}

/// Boots `elf` as a guest with the handoff built by `guest-boot`.
fn kernel_case(
    env: &mut Env,
    page: &mut [u8; 4096],
    name: &str,
    elf: &[u8],
    boot: Boot,
    corrupt: Option<Corrupt>,
    expect: KernelExpect,
) {
    env.frames.reset();
    let base = env.frames.alloc_frame().expect("RAM frame");
    for i in 1..KERNEL_RAM_PAGES {
        let f = env.frames.alloc_frame().expect("RAM frame");
        assert_eq!(f, base + (i * PAGE) as u64, "contiguous guest RAM");
    }
    for i in 0..KERNEL_RAM_PAGES {
        env.phys.fill(base + (i * PAGE) as u64, 0);
    }
    let mut npt = Npt::new(&mut env.phys, &mut env.frames, 48).expect("nested root");
    npt.map(
        &mut env.phys,
        &mut env.frames,
        0,
        base,
        KERNEL_RAM,
        NptPerms::RWX,
    )
    .expect("map guest RAM");
    let cfg = guest_boot::Config {
        ram_bytes: KERNEL_RAM,
        test_profile: boot.test,
        boot_epoch: boot.epoch,
        protocol: boot.protocol,
    };
    let mut ram = GuestRam {
        phys: &mut env.phys,
        base,
        size: KERNEL_RAM,
    };
    let entry = guest_boot::load(elf, &mut ram, &cfg).expect("guest-boot");
    env.cpu.ram = GuestMap::Contig {
        base,
        size: KERNEL_RAM,
    };
    if let Some(c) = corrupt {
        c(&mut ram, &entry);
    }
    let mut vcfg = VmConfig::new(1, env.msrpm, env.iopm, npt.root(), env.nrips);
    vcfg.flush_by_asid = env.flush_by_asid;
    vcfg.max_exits = boot.max_exits;
    vcfg.exit_quantum_ns = boot.quantum_ns;
    vcfg.intr_exit_ns = if boot.host_tick {
        HOST_TICK_NS
    } else {
        boot.quantum_ns
    };
    vcfg.max_time_us = u64::MAX;
    let serial_buf: *mut [u8; 64 * 1024] = &raw mut KERNEL_SERIAL;
    // SAFETY: a static of the probe; each kernel case borrows it once and
    // the borrow ends with the case (cases run one after another).
    let serial = unsafe { &mut *serial_buf };
    let mut vcpu = Vcpu::new(vcfg, serial);
    {
        let mut v = Vmcb::new(page);
        v.setup_long_mode(entry.rip, entry.cr3, entry.rsp);
        vcpu.prepare(&mut v);
    }
    let mut gprs = Gprs {
        rdi: entry.rdi,
        ..Gprs::default()
    };
    if boot.host_tick {
        env.host_tick.start(HOST_TICK_COUNT);
        env.cpu.host_irq = true;
    }
    let o = vcpu.run(&mut env.cpu, &mut NoClock, &mut Vmcb::wrap(page), &mut gprs);
    env.cpu.host_irq = false;
    env.host_tick.stop();
    let ok = expect(&o, vcpu.serial());
    env.report("CASE", name, ok);
    print_outcome(&o, vcpu.serial());
    if !ok {
        let v = Vmcb::wrap(page);
        let mut insn = [0u8; hw_svm::guest::MAX_INSN];
        let n = hw_svm::guest::fetch(&mut env.cpu, &v, &mut insn);
        out!(
            "NANOX:SVM-PROBE:STATE {name} rip={:#x} exit={:#x} info1={:#x} info2={:#x} insn=",
            v.rip(),
            v.exit_code(),
            v.exit_info1(),
            v.exit_info2()
        );
        for b in &insn[..n] {
            out!("{b:02x}");
        }
        out!("\n");
    }
}

/// Marks the transition reservation as kernel memory: the kernel's
/// `validate_buffers` must then refuse the handoff (Ownership).
fn drop_transition(ram: &mut GuestRam<'_>, e: &guest_boot::Entry) {
    use guest_boot::GuestMemory;
    let mut count = [0u8; 4];
    ram.read(e.boot_info_gpa + 80, &mut count);
    let last = u64::from(u32::from_le_bytes(count)) - 1;
    let kind = e.boot_info_gpa + guest_boot::RANGES_OFFSET + 24 * last + 16;
    ram.write(kind, &boot_protocol::KIND_KERNEL.to_le_bytes());
}

/// The M0 harness's default boot epoch (tools/xtask/src/main.rs), so the
/// PASS run is comparable with the QEMU record byte for byte.
const M0_EPOCH: u64 = 20260922;

fn kernel_cases(env: &mut Env, page: &mut [u8; 4096], elf: &[u8]) {
    const VALIDATED: &[u8] = b"NANOX:KERNEL:BOOTINFO_VALIDATED\n";
    const FAIL35: Verdict = Verdict::DebugExit {
        value: 0x11,
        status: 35,
    };
    kernel_case(
        env,
        page,
        "m0-pass",
        elf,
        m0(true, M0_EPOCH),
        None,
        |o, s| o.verdict == PASS && has(s, VALIDATED) && has(s, b"NANOX:TEST:PASS\n"),
    );
    kernel_case(
        env,
        page,
        "m0-fail",
        elf,
        m0(true, u64::MAX),
        None,
        |o, s| o.verdict == FAIL35 && has(s, VALIDATED) && has(s, b"NANOX:TEST:FAIL:injected\n"),
    );
    kernel_case(
        env,
        page,
        "m0-panic",
        elf,
        m0(true, u64::MAX - 2),
        None,
        |o, s| o.verdict == FAIL35 && has(s, b"NANOX:KERNEL:PANIC:"),
    );
    kernel_case(
        env,
        page,
        "m0-hang",
        elf,
        m0(true, u64::MAX - 1),
        None,
        |o, s| o.verdict == Verdict::Halted && has(s, b"NANOX:TEST:HANG:injected\n"),
    );
    kernel_case(
        env,
        page,
        "m0-normal-profile",
        elf,
        m0(false, M0_EPOCH),
        None,
        |o, s| {
            o.verdict == Verdict::Halted && has(s, b"NANOX:KERNEL:IDLE\n") && !has(s, b"NANOX:TEST")
        },
    );
    kernel_case(
        env,
        page,
        "m0-bad-handoff",
        elf,
        m0(true, M0_EPOCH),
        Some(drop_transition),
        |o, s| o.verdict == FAIL35 && has(s, b"NANOX:KERNEL:BOOTINFO_ERROR:Ownership\n"),
    );
}

/// Markers every M1 run that reaches the timer prints before its verdict
/// (tools/xtask/src/runner.rs `expected` on codex/m1-m8-continuation).
fn m1_foundations(s: &[u8]) -> bool {
    [
        &b"NANOX:KERNEL:TABLES_PASS"[..],
        b"NANOX:KERNEL:PMM_PASS",
        b"NANOX:KERNEL:VMM_PASS",
        b"NANOX:KERNEL:VMM_PF_PASS",
        b"NANOX:KERNEL:HEAP_GUARDS_PASS",
        b"NANOX:KERNEL:HEAP_PASS",
        b"NANOX:KERNEL:APIC_MMIO_MAP_PASS",
    ]
    .iter()
    .all(|m| has(s, m))
}

fn m1_timer(s: &[u8]) -> bool {
    m1_foundations(s) && has(s, b"NANOX:KERNEL:TIMER_PASS")
}

/// The M1 kernel's scenarios (epochs as in its xtask `Scenario::epoch`).
fn m1_cases(env: &mut Env, page: &mut [u8; 4096], elf: &[u8]) {
    const FAIL35: Verdict = Verdict::DebugExit {
        value: 0x11,
        status: 35,
    };
    kernel_case(
        env,
        page,
        "m1-pass",
        elf,
        m1(true, M0_EPOCH),
        None,
        |o, s| {
            o.verdict == PASS
                && m1_timer(s)
                && has(s, b"NANOX:KERNEL:VMM_IRQ_PASS")
                && has(s, b"NANOX:TEST:PASS\n")
        },
    );
    kernel_case(
        env,
        page,
        "m1-fail",
        elf,
        m1(true, u64::MAX),
        None,
        |o, s| o.verdict == FAIL35 && m1_timer(s) && has(s, b"NANOX:TEST:FAIL:injected"),
    );
    kernel_case(
        env,
        page,
        "m1-panic",
        elf,
        m1(true, u64::MAX - 2),
        None,
        |o, s| o.verdict == FAIL35 && m1_timer(s) && has(s, b"NANOX:KERNEL:PANIC:"),
    );
    kernel_case(
        env,
        page,
        "m1-hang",
        elf,
        m1(true, u64::MAX - 1),
        None,
        |o, s| o.verdict == Verdict::Halted && m1_timer(s) && has(s, b"NANOX:TEST:HANG"),
    );
    kernel_case(
        env,
        page,
        "m1-fault",
        elf,
        m1(true, u64::MAX - 3),
        None,
        |o, s| {
            o.verdict == FAIL35
                && m1_timer(s)
                && has(s, b"NANOX:TEST:FAULT:inject-unexpected-page-fault")
                && has(s, b"NANOX:KERNEL:PF_ERROR")
        },
    );
    kernel_case(
        env,
        page,
        "m1-timer-fail",
        elf,
        m1(true, u64::MAX - 4),
        None,
        |o, s| {
            o.verdict == FAIL35
                && m1_foundations(s)
                && has(s, b"NANOX:KERNEL:TIMER_ERROR:IrqNotDelivered")
                && !has(s, b"NANOX:KERNEL:TIMER_PASS")
        },
    );
    kernel_case(
        env,
        page,
        "m1-double-fault-ist",
        elf,
        m1(true, u64::MAX - 5),
        None,
        |o, s| {
            o.verdict == PASS
                && m1_timer(s)
                && has(s, b"NANOX:TEST:DF:INJECT_DELIVERY_STACK_FAULT")
                && has(s, b"NANOX:KERNEL:DOUBLE_FAULT_IST_PASS")
        },
    );
    kernel_case(
        env,
        page,
        "m1-trace-overflow",
        elf,
        m1(true, u64::MAX - 7),
        None,
        |o, s| {
            o.verdict == FAIL35
                && m1_timer(s)
                && has(s, b"NANOX:KERNEL:TRACE_OVERFLOW count=128 queue_full=1")
                && has(s, b"NANOX:TEST:FAIL:event-trace-overflow")
        },
    );
    // Last: its observer thread spins on plain loads, so it depends on the
    // host tick for time to pass.
    kernel_case(
        env,
        page,
        "m1-preemption",
        elf,
        m1_host_timed(true, u64::MAX - 6),
        None,
        |o, s| o.verdict == PASS && m1_timer(s) && has(s, b"NANOX:KERNEL:PREEMPT_PASS"),
    );
}

type Edit = fn(&mut Vmcb<'_>);

/// Each VMRUN consistency violation from a valid baseline: `Vmcb::check`
/// must name it; the processor's answer is recorded. The last two are the
/// VMM's own requirements and are not run: the guest would reach host
/// memory.
fn checks(env: &mut Env, page: &mut [u8; 4096]) {
    let g = env.guest(guest::halt());
    let mut serial = [0u8; 1];
    let vcpu = Vcpu::new(g.cfg, &mut serial);
    enter(page, &vcpu);
    let base = *page;

    let mut v = Vmcb::wrap(page);
    v.set_tlb_control(tlb::FLUSH_ALL);
    let accepted = v.check().is_ok();
    v.write_u64(ctl::EXIT_CODE, 0xDEAD);
    env.cpu.vmrun(&mut v, &mut Gprs::default());
    let exit = v.exit_code();
    env.report("CHECK", "baseline", accepted && exit == VMEXIT_HLT);
    out!(" hw-svm=accept cpu-exit={exit:#x}\n");

    let rows: [(&str, Option<StateError>, Edit, bool); 18] = [
        (
            "efer-svme-clear",
            Some(StateError::EferSvmeClear),
            |v| v.write_u64(save::EFER, v.read_u64(save::EFER) & !bits::EFER_SVME),
            true,
        ),
        (
            "efer-reserved-bit1",
            Some(StateError::EferReserved),
            |v| v.write_u64(save::EFER, v.read_u64(save::EFER) | 1 << 1),
            true,
        ),
        (
            "cr0-nw-without-cd",
            Some(StateError::Cr0CdNw),
            |v| v.write_u64(save::CR0, v.read_u64(save::CR0) | bits::CR0_NW),
            true,
        ),
        (
            "cr0-bit32",
            Some(StateError::Cr0High),
            |v| v.write_u64(save::CR0, v.read_u64(save::CR0) | 1 << 32),
            true,
        ),
        (
            "cr3-bit52",
            Some(StateError::Cr3Reserved),
            |v| v.write_u64(save::CR3, v.read_u64(save::CR3) | 1 << 52),
            true,
        ),
        (
            "cr4-bit15",
            Some(StateError::Cr4Reserved),
            |v| v.write_u64(save::CR4, v.read_u64(save::CR4) | 1 << 15),
            true,
        ),
        (
            "dr6-bit32",
            Some(StateError::Dr6High),
            |v| v.write_u64(save::DR6, v.read_u64(save::DR6) | 1 << 32),
            true,
        ),
        (
            "dr7-bit32",
            Some(StateError::Dr7High),
            |v| v.write_u64(save::DR7, v.read_u64(save::DR7) | 1 << 32),
            true,
        ),
        (
            "long-mode-without-pae",
            Some(StateError::LongModeWithoutPae),
            |v| v.write_u64(save::CR4, v.read_u64(save::CR4) & !bits::CR4_PAE),
            true,
        ),
        (
            "long-mode-without-pe",
            Some(StateError::LongModeWithoutPe),
            |v| v.write_u64(save::CR0, v.read_u64(save::CR0) & !bits::CR0_PE),
            true,
        ),
        (
            "cs-long-and-d",
            Some(StateError::CsLongAndDefault32),
            |v| {
                let mut cs = v.segment(save::CS);
                cs.attrib |= attr::DB;
                v.set_segment(save::CS, cs);
            },
            true,
        ),
        (
            "vmrun-not-intercepted",
            Some(StateError::VmrunNotIntercepted),
            |v| {
                let m = v.read_u32(ctl::INTERCEPT_MISC2) & !misc2::VMRUN;
                v.write_u32(ctl::INTERCEPT_MISC2, m);
            },
            true,
        ),
        (
            "asid-zero",
            Some(StateError::AsidZero),
            |v| v.write_u32(ctl::ASID, 0),
            true,
        ),
        (
            "iopm-misaligned",
            Some(StateError::PermissionMapAddress),
            |v| v.write_u64(ctl::IOPM_BASE, v.read_u64(ctl::IOPM_BASE) + 8),
            true,
        ),
        (
            "inject-exception-vector-2",
            Some(StateError::EventInjection),
            |v| v.set_event_inj(1 << 31 | 3 << 8 | 2),
            true,
        ),
        (
            "inject-exception-vector-31",
            None,
            |v| v.set_event_inj(1 << 31 | 3 << 8 | 31),
            true,
        ),
        (
            "nested-paging-disabled",
            Some(StateError::NestedPagingDisabled),
            |v| v.write_u64(ctl::NP_ENABLE, 0),
            false,
        ),
        (
            "nested-cr3-zero",
            Some(StateError::NestedCr3),
            |v| v.write_u64(ctl::N_CR3, 0),
            false,
        ),
    ];
    for (name, want, edit, run) in rows {
        page.copy_from_slice(&base);
        let mut v = Vmcb::wrap(page);
        v.set_tlb_control(tlb::FLUSH_ALL);
        edit(&mut v);
        let got = v.check();
        let crate_ok = match want {
            Some(e) => got == Err(Error::InvalidState(e)),
            None => got.is_ok(),
        };
        env.report("CHECK", name, crate_ok);
        out!(" hw-svm={}", if got.is_ok() { "accept" } else { "reject" });
        if run {
            v.write_u64(ctl::EXIT_CODE, 0xDEAD);
            env.cpu.vmrun(&mut v, &mut Gprs::default());
            let exit = v.exit_code();
            let cpu_rejects = is_invalid(exit);
            out!(
                " cpu={} cpu-exit={exit:#x} agree={}\n",
                if cpu_rejects { "reject" } else { "accept" },
                if cpu_rejects == got.is_err() {
                    "yes"
                } else {
                    "no"
                }
            );
        } else {
            out!(" cpu=not-run\n");
        }
    }
}

#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(_image: *mut u8, system: *mut u8) -> usize {
    hw::interrupts_off();
    Serial::init();
    out!("NANOX:SVM-PROBE:START\n");

    let max_ext = hw::cpuid(0x8000_0000, 0)[0];
    let ext_ecx = hw::cpuid(0x8000_0001, 0)[2];
    let svm = if max_ext >= 0x8000_000A {
        hw::cpuid(0x8000_000A, 0)
    } else {
        [0; 4]
    };
    let raw = CpuidMsr {
        max_ext_leaf: max_ext,
        ext_ecx,
        svm_eax: svm[0],
        svm_ebx: svm[1],
        svm_edx: svm[3],
        // VM_CR exists only when the processor reports SVM.
        vm_cr: if ext_ecx & (1 << 2) != 0 {
            hw::rdmsr(MSR_VM_CR)
        } else {
            0
        },
    };
    let caps = match SvmCaps::decode(raw) {
        Ok(c) => c,
        Err(e) => {
            out!("NANOX:SVM-PROBE:UNAVAILABLE {e:?}\nNANOX:SVM-PROBE:RESULT FAIL\n");
            hw::exit(0x11)
        }
    };
    out!("NANOX:SVM-PROBE:CAPS {caps:?}\n");

    let base = ((&raw mut POOL) as u64).next_multiple_of(PAGE_SIZE);
    let pa = |i: usize| base + (i * PAGE) as u64;
    // SAFETY: the fixed pages are disjoint parts of the probe's own static
    // pool (aligned above, one spare page), identity-mapped, and each is
    // borrowed exactly once here for the rest of the run.
    let (vmcb, msrpm, iopm) = unsafe {
        (
            &mut *(pa(2) as *mut [u8; PAGE]),
            &mut *(pa(3) as *mut [u8; MSRPM_BYTES]),
            &mut *(pa(5) as *mut [u8; IOPM_BYTES]),
        )
    };
    Vcpu::msr_policy(&mut MsrPermissionMap::intercept_all(msrpm));
    let _ = IoPermissionMap::intercept_all(iopm);
    hw::wrmsr(MSR_EFER, hw::rdmsr(MSR_EFER) | bits::EFER_SVME);
    hw::wrmsr(MSR_VM_HSAVE_PA, pa(0));

    let mut env = Env {
        phys: Phys {
            lo: pa(FIXED_PAGES),
            hi: pa(POOL_PAGES),
        },
        frames: Frames::new(pa(FIXED_PAGES), FRAME_PAGES),
        cpu: Cpu {
            host_save: pa(1),
            ram: GuestMap::None,
            host_irq: false,
        },
        host_tick: hw::HostTick::new(),
        msrpm: pa(3),
        iopm: pa(5),
        nrips: caps.nrips,
        flush_by_asid: caps.flush_by_asid,
        failures: 0,
    };
    cases(&mut env, vmcb);
    checks(&mut env, vmcb);
    let elf_buf: *mut [u8; 1 << 20] = &raw mut ELF;
    // SAFETY: the ELF buffer is a static of the probe, borrowed once here.
    let buf = unsafe { &mut *elf_buf };
    match fwcfg::read_file("opt/nanox/kernel.elf", buf) {
        Ok(elf) => {
            out!("NANOX:SVM-PROBE:KERNEL bytes={}\n", elf.len());
            kernel_cases(&mut env, vmcb, elf);
        }
        Err(e) => {
            env.report("CASE", "m0-kernel-elf", false);
            out!(" fw_cfg={e}\n");
        }
    }
    // An M1 kernel is optional: the runner passes one when it has it.
    match fwcfg::read_file("opt/nanox/kernel-m1.elf", buf) {
        Ok(elf) => {
            out!("NANOX:SVM-PROBE:KERNEL-M1 bytes={}\n", elf.len());
            m1_cases(&mut env, vmcb, elf);
        }
        Err(e) => out!("NANOX:SVM-PROBE:KERNEL-M1 absent fw_cfg={e}\n"),
    }
    // Last: it replaces the MSR policy with the platform VMM's.
    linux::case(&mut env, vmcb, msrpm, system);
    if env.failures == 0 {
        out!("NANOX:SVM-PROBE:RESULT PASS\n");
        finish(0x10)
    }
    out!("NANOX:SVM-PROBE:RESULT FAIL failures={}\n", env.failures);
    finish(0x11)
}

/// Ends the run with `value`; with fw_cfg `opt/nanox/hold` (run.py --show)
/// it halts instead, so the last screen stays in QEMU's window until the
/// user closes it.
fn finish(value: u32) -> ! {
    if fwcfg::find("opt/nanox/hold").is_ok() {
        out!("NANOX:SVM-PROBE:HOLD close the window to end the run\n");
        hw::halt()
    }
    hw::exit(value)
}
