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
//! Guest layout (guest-physical, 16 pages of RAM from the probe's pool):
//! 0 zero page (the guest IDT: every vector not present), 1 code, 2..4
//! guest page tables identity-mapping 2 MiB, 5 scratch, 6 read-only in
//! the nested tables, 7 stack, 9 remapped by the remap case.

#![no_std]
#![no_main]

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
const FRAME_PAGES: usize = 64;
const POOL_PAGES: usize = FIXED_PAGES + FRAME_PAGES;
/// One spare page so the pool can be aligned at run time.
static mut POOL: [u8; (POOL_PAGES + 1) * PAGE] = [0; (POOL_PAGES + 1) * PAGE];

const RAM_PAGES: usize = 16;
const ENTRY: u64 = 0x1000;
const GUEST_CR3: u64 = 0x2000;
const GUEST_STACK: u64 = 0x8000;
const RO_PAGE: usize = 6;
const REMAP_PAGE: usize = 9;

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
        // SAFETY: as in `fill`; the source is the probe's .text.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), pa as *mut u8, data.len()) }
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

/// Bump allocator over the frame region, reset for every guest.
struct Frames {
    base: u64,
    next: usize,
    free: [u64; FRAME_PAGES],
    nfree: usize,
}

impl Frames {
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
        if self.next == FRAME_PAGES {
            return None;
        }
        self.next += 1;
        Some(self.base + ((self.next - 1) * PAGE) as u64)
    }

    fn free_frame(&mut self, pa: u64) {
        assert!(self.nfree < FRAME_PAGES, "double free of {pa:#x}");
        self.free[self.nfree] = pa;
        self.nfree += 1;
    }
}

struct Cpu {
    host_save: u64,
}

impl SvmCpu for Cpu {
    fn vmrun(&mut self, vmcb: &mut Vmcb<'_>, g: &mut Gprs) {
        let mut r = [
            g.rbx, g.rcx, g.rdx, g.rsi, g.rdi, g.rbp, g.r8, g.r9, g.r10, g.r11, g.r12, g.r13,
            g.r14, g.r15,
        ];
        // SAFETY: the VMCB and host save area are probe-owned pages, SVM
        // and VM_HSAVE_PA were enabled in `efi_main`, and every guest's
        // nested tables map only frames of the probe's frame region.
        unsafe { hw::vmrun(&mut r, vmcb.as_mut_ptr(), self.host_save) };
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
        self.phys.copy(ram[1], code);
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

fn print_outcome(o: &Outcome, serial: &[u8]) {
    out!(
        " verdict={:?} exits={} msr_faults={} ud={} serial=\"",
        o.verdict,
        o.exits,
        o.msr_faults,
        o.ud_injected
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
pub extern "efiapi" fn efi_main(_image: *mut u8, _system: *mut u8) -> usize {
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
        frames: Frames {
            base: pa(FIXED_PAGES),
            next: 0,
            free: [0; FRAME_PAGES],
            nfree: 0,
        },
        cpu: Cpu { host_save: pa(1) },
        msrpm: pa(3),
        iopm: pa(5),
        nrips: caps.nrips,
        flush_by_asid: caps.flush_by_asid,
        failures: 0,
    };
    cases(&mut env, vmcb);
    checks(&mut env, vmcb);
    if env.failures == 0 {
        out!("NANOX:SVM-PROBE:RESULT PASS\n");
        hw::exit(0x10)
    }
    out!("NANOX:SVM-PROBE:RESULT FAIL failures={}\n", env.failures);
    hw::exit(0x11)
}
