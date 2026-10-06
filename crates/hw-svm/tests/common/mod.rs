//! Test rig: host physical memory with a frame allocator, and a scripted
//! "processor" implementing `SvmCpu`. The processor behaves like SVM where
//! it matters to the VMM: it refuses states that fail the VMRUN checks,
//! honours TLB_CONTROL, caches translations in its own TLB, walks the
//! nested tables itself (independently of the crate's walker), consults the
//! MSR and I/O permission maps in memory at the VMCB's addresses, and
//! checks that the VMM advanced RIP (or injected an event) correctly.
//! Findings go to `violations`; tests assert the list is empty.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use hw_svm::exit::code;
use hw_svm::npt::Npt;
use hw_svm::perm::{IoPermissionMap, MsrPermissionMap, IOPM_BYTES, MSRPM_BYTES};
use hw_svm::platform_vm::{Host, PlatformVcpu};
use hw_svm::vmcb::{ctl, tlb, Gprs, Vmcb};
use hw_svm::vmm::{Outcome, Vcpu, VmConfig};
use hw_svm::{Clock, FrameAlloc, NptPerms, PhysMem, SvmCpu, PAGE_SIZE};

pub const FRAME_BASE: u64 = 0x10_0000;
pub const MSRPM_PA: u64 = 0x7000_0000;
pub const IOPM_PA: u64 = 0x7001_0000;
pub const RAM_PAGES: u64 = 16;
pub const ENTRY: u64 = 0x1000;

#[derive(Default)]
pub struct Mem {
    pages: HashMap<u64, Box<[u8; 4096]>>,
    pub violations: Vec<String>,
}

impl Mem {
    pub fn new() -> Self {
        Self::default()
    }

    fn page(&mut self, pa: u64) -> &mut [u8; 4096] {
        self.pages
            .entry(pa & !0xFFF)
            .or_insert_with(|| Box::new([0xA5; 4096]))
    }

    pub fn write_bytes(&mut self, pa: u64, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            let a = pa + i as u64;
            self.page(a)[(a & 0xFFF) as usize] = *b;
        }
    }

    pub fn read_bytes(&mut self, pa: u64, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            let a = pa + i as u64;
            *b = self.page(a)[(a & 0xFFF) as usize];
        }
    }

    /// Fills a page with garbage, as memory handed out again would be.
    pub fn scribble(&mut self, pa: u64) {
        *self.page(pa) = [0xA5; 4096];
    }
}

impl PhysMem for Mem {
    fn read_u64(&mut self, pa: u64) -> u64 {
        if !pa.is_multiple_of(8) {
            self.violations.push(format!("unaligned read {pa:#x}"));
        }
        let mut b = [0u8; 8];
        self.read_bytes(pa, &mut b);
        u64::from_le_bytes(b)
    }
    fn write_u64(&mut self, pa: u64, value: u64) {
        if !pa.is_multiple_of(8) {
            self.violations.push(format!("unaligned write {pa:#x}"));
        }
        self.write_bytes(pa, &value.to_le_bytes());
    }
}

/// Frame allocator; checks double and foreign frees.
#[derive(Default)]
pub struct Frames {
    next: u64,
    free: Vec<u64>,
    allocated: HashSet<u64>,
    /// Number of further allocations that succeed (`None` = unlimited).
    pub fail_after: Option<u32>,
    pub violations: Vec<String>,
}

impl Frames {
    pub fn new() -> Self {
        Self {
            next: FRAME_BASE,
            ..Self::default()
        }
    }
    pub fn allocated(&self) -> usize {
        self.allocated.len()
    }
}

impl FrameAlloc for Frames {
    fn alloc_frame(&mut self) -> Option<u64> {
        if let Some(n) = self.fail_after.as_mut() {
            if *n == 0 {
                return None;
            }
            *n -= 1;
        }
        let f = self.free.pop().unwrap_or_else(|| {
            let f = self.next;
            self.next += PAGE_SIZE;
            f
        });
        self.allocated.insert(f);
        Some(f)
    }
    fn free_frame(&mut self, pa: u64) {
        if !self.allocated.remove(&pa) {
            self.violations
                .push(format!("free of unallocated frame {pa:#x}"));
        }
        self.free.push(pa);
    }
}

/// One guest instruction of the script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Out {
        port: u16,
        size: u8,
        value: u32,
    },
    /// IN; the VMM's result (masked to `size`) must equal `expect`.
    In {
        port: u16,
        size: u8,
        expect: u32,
    },
    OutString {
        port: u16,
    },
    Cpuid {
        leaf: u32,
        sub: u32,
    },
    Rdmsr {
        msr: u32,
    },
    Wrmsr {
        msr: u32,
        value: u64,
    },
    Hlt,
    /// Reads one byte of guest-physical memory into `loads`.
    Load {
        gpa: u64,
    },
    Store {
        gpa: u64,
        byte: u8,
    },
    /// A timer interrupt arrives (one INTR exit, no guest progress).
    Tick,
    /// The guest spins forever; every VMRUN ends in an INTR exit.
    SpinForever,
    TripleFault,
    Vmmcall,
    /// RDMSR/WRMSR of an intercepted MSR the VMM emulates: no fault.
    MsrEmulated {
        msr: u32,
        write: bool,
        value: u64,
    },
    /// The instruction `insn[..len]` accesses unmapped `gpa` (a nested page
    /// fault). With `assist` the processor supplies the instruction bytes
    /// (decode assists); otherwise the VMM must fetch them from guest
    /// memory at RIP.
    Mmio {
        gpa: u64,
        insn: [u8; 15],
        len: u8,
        assist: bool,
    },
    /// A nested page fault with this error code at `gpa`, without progress
    /// (a fetch, a page-table walk, a permission fault).
    Npf {
        gpa: u64,
        error: u64,
    },
    /// Sets RFLAGS.IF; the next step is in the interrupt shadow.
    Sti,
    Cli,
    /// PAUSE; must be intercepted.
    Pause,
    /// RDTSC: exits if intercepted (the result goes to `rdtsc_results`), else reads the host TSC.
    Rdtsc,
    /// RDTSCP: exits if intercepted, else reads the host TSC.
    Rdtscp,
}

impl Step {
    pub fn len(self) -> u64 {
        match self {
            Step::Out { .. } | Step::In { .. } | Step::OutString { .. } => 1,
            Step::Cpuid { .. }
            | Step::Rdmsr { .. }
            | Step::Wrmsr { .. }
            | Step::MsrEmulated { .. } => 2,
            Step::Vmmcall => 3,
            Step::Hlt | Step::Sti | Step::Cli => 1,
            Step::Pause | Step::Rdtsc => 2,
            Step::Rdtscp => 3,
            Step::Load { .. } | Step::Store { .. } | Step::Npf { .. } => 3,
            Step::Tick | Step::SpinForever | Step::TripleFault => 2,
            Step::Mmio { len, .. } => u64::from(len),
        }
    }
}

/// An MMIO step with the instruction's bytes.
pub fn mmio(gpa: u64, bytes: &[u8], assist: bool) -> Step {
    let mut insn = [0x90u8; 15];
    insn[..bytes.len()].copy_from_slice(bytes);
    Step::Mmio {
        gpa,
        insn,
        len: bytes.len() as u8,
        assist,
    }
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    index: usize,
    /// The exit advances RIP when handled normally.
    advances: bool,
    /// A fault injection is the expected handling.
    expect_fault: bool,
}

pub struct FakeCpu {
    pub mem: Mem,
    pub frames: Frames,
    steps: Vec<(u64, Step)>,
    pos: usize,
    pending: Option<Pending>,
    /// gpa page -> (hpa page, writable)
    pub tlb: HashMap<u64, (u64, bool)>,
    pub tlb_controls: Vec<u8>,
    pub injected: Vec<u64>,
    pub cpuid_results: Vec<[u32; 4]>,
    pub rdmsr_results: Vec<u64>,
    /// EDX:EAX of every RDTSC/RDTSCP, intercepted or not.
    pub rdtsc_results: Vec<u64>,
    /// The host's TSC for reads the VMM does not intercept; each read adds 1000.
    pub host_tsc: u64,
    pub loads: Vec<u8>,
    pub vmruns: u64,
    pub msrs: HashMap<u32, u64>,
    pub host_cpuid: HashMap<(u32, u32), [u32; 4]>,
    pub violations: Vec<String>,
    /// External interrupts delivered: (vector, RIP at delivery).
    pub interrupts: Vec<(u8, u64)>,
    /// N_CR3 of the last VMRUN, for `read_guest_phys`.
    ncr3: u64,
    /// The next step executes in the STI interrupt shadow.
    sti_shadow: bool,
}

impl FakeCpu {
    pub fn new(mem: Mem, frames: Frames, script: &[Step]) -> Self {
        let mut rip = ENTRY;
        let mut steps = Vec::new();
        for &s in script {
            steps.push((rip, s));
            rip += s.len();
        }
        let mut host_cpuid = HashMap::new();
        // A Zen 3-like host: max basic leaf 10h, max extended 8000_0021h.
        host_cpuid.insert((0, 0), [0x10, 0x6874_7541, 0x444D_4163, 0x6974_6E65]);
        host_cpuid.insert(
            (1, 0),
            [
                0x00A5_0F00,
                0,
                0x7ED8_320B | 1 << 21 | 1 << 24 | 1 << 3,
                0x178B_FBFF,
            ],
        );
        host_cpuid.insert((0x8000_0000, 0), [0x8000_0021, 0, 0, 0]);
        host_cpuid.insert((0x8000_0001, 0), [0, 0, 0x75C2_37FF, 0x2FD3_FBFF]);
        host_cpuid.insert((0x8000_000A, 0), [1, 0x8000, 0, 0x101B_BCFF]);
        Self {
            mem,
            frames,
            steps,
            pos: 0,
            pending: None,
            tlb: HashMap::new(),
            tlb_controls: Vec::new(),
            injected: Vec::new(),
            cpuid_results: Vec::new(),
            rdmsr_results: Vec::new(),
            rdtsc_results: Vec::new(),
            host_tsc: 0x10_0000_0000,
            loads: Vec::new(),
            vmruns: 0,
            msrs: HashMap::new(),
            host_cpuid,
            violations: Vec::new(),
            interrupts: Vec::new(),
            ncr3: 0,
            sti_shadow: false,
        }
    }

    /// Lays the script out from `entry` instead of [`ENTRY`] (a guest that starts elsewhere).
    pub fn start_at(&mut self, entry: u64) {
        let mut rip = entry;
        for s in &mut self.steps {
            s.0 = rip;
            rip += s.1.len();
        }
    }

    /// RIP of script step `index`.
    pub fn rip_of(&self, index: usize) -> u64 {
        self.steps[index].0
    }

    /// Nested walk without TLB or checks (for `read_guest_phys`).
    fn npt_walk(&mut self, gpa: u64) -> Option<u64> {
        let mut table = self.ncr3;
        for level in (0..4).rev() {
            let e = self
                .mem
                .read_u64(table + 8 * ((gpa >> (12 + 9 * level)) & 511));
            if e & 1 == 0 {
                return None;
            }
            table = e & 0x000F_FFFF_FFFF_F000;
        }
        Some(table | (gpa & 0xFFF))
    }

    pub fn assert_clean(&self) {
        assert!(
            self.violations.is_empty(),
            "cpu violations: {:#?}",
            self.violations
        );
        assert!(
            self.mem.violations.is_empty(),
            "memory violations: {:#?}",
            self.mem.violations
        );
        assert!(
            self.frames.violations.is_empty(),
            "frame violations: {:#?}",
            self.frames.violations
        );
    }

    fn io_intercepted(&mut self, vmcb: &Vmcb<'_>, port: u16, size: u8) -> bool {
        let base = vmcb.read_u64(ctl::IOPM_BASE);
        (0..u64::from(size)).any(|i| {
            let p = u64::from(port) + i;
            let mut b = [0u8];
            self.mem.read_bytes(base + p / 8, &mut b);
            b[0] & (1 << (p % 8)) != 0
        })
    }

    fn msr_intercepted(&mut self, vmcb: &Vmcb<'_>, msr: u32, write: bool) -> bool {
        let (base, off) = match msr {
            0..=0x1FFF => (0, 0),
            0xC000_0000..=0xC000_1FFF => (0xC000_0000, 0x800),
            0xC001_0000..=0xC001_1FFF => (0xC001_0000, 0x1000),
            _ => return true,
        };
        let bit = off * 8 + 2 * u64::from(msr - base) + u64::from(write);
        let mut b = [0u8];
        self.mem
            .read_bytes(vmcb.read_u64(ctl::MSRPM_BASE) + bit / 8, &mut b);
        b[0] & (1 << (bit % 8)) != 0
    }

    /// Independent nested walk: Ok(hpa) or Err(NPF error code).
    fn translate(&mut self, vmcb: &Vmcb<'_>, gpa: u64, write: bool) -> Result<u64, u64> {
        let page = gpa & !0xFFF;
        if let Some(&(hpa, w)) = self.tlb.get(&page) {
            if !write || w {
                return Ok(hpa | (gpa & 0xFFF));
            }
        }
        let mut table = vmcb.read_u64(ctl::N_CR3);
        let mut writable = true;
        for level in (0..4).rev() {
            let idx = (gpa >> (12 + 9 * level)) & 511;
            let e = self.mem.read_u64(table + 8 * idx);
            if e & 1 == 0 {
                return Err(u64::from(write) << 1 | 4 | 1 << 32);
            }
            if e & 4 == 0 {
                self.violations
                    .push(format!("nested entry without U/S at level {level}: {e:#x}"));
            }
            writable &= e & 2 != 0;
            table = e & 0x000F_FFFF_FFFF_F000;
        }
        if write && !writable {
            return Err(1 | 2 | 4 | 1 << 32);
        }
        self.tlb.insert(page, (table, writable));
        Ok(table | (gpa & 0xFFF))
    }

    fn exit(vmcb: &mut Vmcb<'_>, code: u64, info1: u64, info2: u64, nrip: u64) {
        vmcb.write_u64(ctl::EXIT_CODE, code);
        vmcb.write_u64(ctl::EXIT_INFO1, info1);
        vmcb.write_u64(ctl::EXIT_INFO2, info2);
        vmcb.write_u64(ctl::NRIP, nrip);
    }

    fn settle(&mut self, vmcb: &mut Vmcb<'_>, gprs: &Gprs) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let (rip, step) = self.steps[p.index];
        let inj = vmcb.event_inj();
        let injected = inj & (1 << 31) != 0;
        if injected {
            self.injected.push(inj);
            vmcb.set_event_inj(inj & !(1 << 31));
        }
        if injected != p.expect_fault {
            self.violations
                .push(format!("step {p:?} {step:?}: injection {inj:#x}"));
        }
        let want = if injected || !p.advances {
            rip
        } else {
            rip + step.len()
        };
        if vmcb.rip() != want {
            self.violations.push(format!(
                "step {step:?}: RIP {:#x}, expected {want:#x}",
                vmcb.rip()
            ));
        }
        match step {
            Step::In { size, expect, .. } => {
                let mask = if size == 4 {
                    0xFFFF_FFFF
                } else {
                    (1u64 << (8 * size)) - 1
                };
                let v = vmcb.rax() & mask;
                if v != u64::from(expect) {
                    self.violations.push(format!("{step:?}: got {v:#x}"));
                }
                // 8/16-bit IN merges into RAX; 32-bit IN zero-extends.
                let upper = if size == 4 {
                    0
                } else {
                    0xDEAD_BEEF_DEAD_BEEF & !mask
                };
                if vmcb.rax() & !mask != upper {
                    self.violations.push(format!(
                        "{step:?}: upper RAX bits {:#x}",
                        vmcb.rax() & !mask
                    ));
                }
            }
            Step::Cpuid { .. } => self.cpuid_results.push([
                vmcb.rax() as u32,
                gprs.rbx as u32,
                gprs.rcx as u32,
                gprs.rdx as u32,
            ]),
            Step::MsrEmulated { write: false, .. } if !injected => self
                .rdmsr_results
                .push(gprs.rdx << 32 | (vmcb.rax() & 0xFFFF_FFFF)),
            Step::Rdtsc if !injected => {
                if vmcb.rax() >> 32 != 0 || gprs.rdx >> 32 != 0 {
                    self.violations.push(format!(
                        "RDTSC: upper halves {:#x} {:#x}",
                        vmcb.rax(),
                        gprs.rdx
                    ));
                }
                self.rdtsc_results.push(gprs.rdx << 32 | vmcb.rax());
            }
            _ => {}
        }
        self.pos = p.index + 1;
    }

    /// An injected external interrupt (EVENTINJ type 0) is taken at the
    /// current RIP; the script's handler returns at once.
    fn take_interrupt(&mut self, vmcb: &mut Vmcb<'_>) {
        let inj = vmcb.event_inj();
        if inj & (1 << 31) != 0 && (inj >> 8) & 7 == 0 {
            if vmcb.rflags() & (1 << 9) == 0 {
                self.violations
                    .push(format!("interrupt {inj:#x} injected with IF=0"));
            }
            if vmcb.read_u64(ctl::INTERRUPT_SHADOW) & 1 != 0 {
                self.violations
                    .push(format!("interrupt {inj:#x} injected in a shadow"));
            }
            self.interrupts.push((inj as u8, vmcb.rip()));
            vmcb.set_event_inj(0);
        }
    }

    fn window_open(vmcb: &Vmcb<'_>) -> bool {
        vmcb.read_u32(ctl::INTERCEPT_MISC1) & hw_svm::vmcb::misc1::VINTR != 0
            && vmcb.read_u64(ctl::VINTR) & hw_svm::vmcb::vintr::V_IRQ != 0
            && vmcb.rflags() & (1 << 9) != 0
    }
}

impl SvmCpu for FakeCpu {
    fn vmrun(&mut self, vmcb: &mut Vmcb<'_>, gprs: &mut Gprs) {
        self.vmruns += 1;
        if vmcb.check().is_err() {
            Self::exit(vmcb, code::INVALID, 0, 0, 0);
            return;
        }
        let tc = vmcb.tlb_control();
        self.tlb_controls.push(tc);
        if tc == tlb::FLUSH_ALL || tc == tlb::FLUSH_ASID {
            self.tlb.clear();
        }
        self.ncr3 = vmcb.read_u64(ctl::N_CR3);
        self.take_interrupt(vmcb);
        self.settle(vmcb, gprs);
        loop {
            let Some(&(rip, step)) = self.steps.get(self.pos) else {
                Self::exit(vmcb, code::INTR, 0, 0, vmcb.rip());
                return;
            };
            vmcb.set_rip(rip);
            let next = rip + step.len();
            let index = self.pos;
            let pend = |advances, expect_fault| {
                Some(Pending {
                    index,
                    advances,
                    expect_fault,
                })
            };
            // The shadow of an STI covers exactly this step; the VMCB
            // reports it if this step exits.
            let shadow = std::mem::take(&mut self.sti_shadow);
            vmcb.write_u64(ctl::INTERRUPT_SHADOW, u64::from(shadow));
            // The requested interrupt window opens at an instruction
            // boundary with IF=1 outside the STI shadow.
            if !shadow && Self::window_open(vmcb) {
                Self::exit(vmcb, code::VINTR, 0, 0, rip);
                self.pending = pend(false, false);
                return;
            }
            match step {
                Step::Sti | Step::Cli => {
                    let f = vmcb.read_u64(hw_svm::vmcb::save::RFLAGS);
                    let f = if step == Step::Sti {
                        self.sti_shadow = true;
                        f | 1 << 9
                    } else {
                        f & !(1 << 9)
                    };
                    vmcb.write_u64(hw_svm::vmcb::save::RFLAGS, f);
                    self.pos += 1;
                }
                Step::Pause => {
                    let m = vmcb.read_u32(ctl::INTERCEPT_MISC1);
                    if m & hw_svm::vmcb::misc1::PAUSE == 0 {
                        self.violations.push("PAUSE not intercepted".into());
                    }
                    Self::exit(vmcb, code::PAUSE, 0, 0, next);
                    self.pending = pend(true, false);
                    return;
                }
                Step::Rdtsc | Step::Rdtscp => {
                    let (m, bit, exit) = if step == Step::Rdtsc {
                        (
                            ctl::INTERCEPT_MISC1,
                            hw_svm::vmcb::misc1::RDTSC,
                            code::RDTSC,
                        )
                    } else {
                        (
                            ctl::INTERCEPT_MISC2,
                            hw_svm::vmcb::misc2::RDTSCP,
                            code::RDTSCP,
                        )
                    };
                    if vmcb.read_u32(m) & bit != 0 {
                        vmcb.set_rax(0xDEAD_BEEF_DEAD_BEEF);
                        gprs.rdx = 0xDEAD_BEEF_DEAD_BEEF;
                        Self::exit(vmcb, exit, 0, 0, next);
                        // RDTSCP is expected to fault (the platform hides it).
                        self.pending = pend(true, step == Step::Rdtscp);
                        return;
                    }
                    let t = self.host_tsc;
                    self.host_tsc += 1000;
                    vmcb.set_rax(t & 0xFFFF_FFFF);
                    gprs.rdx = t >> 32;
                    self.rdtsc_results.push(t);
                    self.pos += 1;
                }
                Step::MsrEmulated { msr, write, value } => {
                    gprs.rcx = u64::from(msr);
                    if write {
                        vmcb.set_rax(value & 0xFFFF_FFFF);
                        gprs.rdx = value >> 32;
                    }
                    if !self.msr_intercepted(vmcb, msr, write) {
                        self.violations
                            .push(format!("MSR {msr:#x} not intercepted"));
                    }
                    Self::exit(vmcb, code::MSR, u64::from(write), 0, next);
                    self.pending = pend(true, false);
                    return;
                }
                Step::Mmio {
                    gpa,
                    insn,
                    len,
                    assist,
                } => {
                    // Not present, final translation.
                    Self::exit(vmcb, code::NPF, 4 | 1 << 32, gpa, next);
                    let n = if assist { len } else { 0 };
                    vmcb.write_u8(ctl::INSN_LEN, n);
                    for (i, b) in insn.iter().enumerate() {
                        vmcb.write_u8(ctl::INSN_BYTES + i, if assist { *b } else { 0 });
                    }
                    self.pending = pend(true, false);
                    return;
                }
                Step::Out { port, size, value }
                | Step::In {
                    port,
                    size,
                    expect: value,
                } => {
                    let input = matches!(step, Step::In { .. });
                    if !self.io_intercepted(vmcb, port, size) {
                        self.violations
                            .push(format!("port {port:#x} not intercepted"));
                    }
                    let sz = match size {
                        1 => 1 << 4,
                        2 => 1 << 5,
                        _ => 1 << 6,
                    };
                    vmcb.set_rax(if input {
                        0xDEAD_BEEF_DEAD_BEEF
                    } else {
                        u64::from(value)
                    });
                    Self::exit(
                        vmcb,
                        code::IOIO,
                        u64::from(port) << 16 | sz | u64::from(input),
                        next,
                        next,
                    );
                    self.pending = pend(true, false);
                    return;
                }
                Step::OutString { port } => {
                    Self::exit(
                        vmcb,
                        code::IOIO,
                        u64::from(port) << 16 | 1 << 4 | 1 << 2,
                        next,
                        next,
                    );
                    self.pending = pend(true, false);
                    return;
                }
                Step::Cpuid { leaf, sub } => {
                    vmcb.set_rax(u64::from(leaf));
                    gprs.rcx = u64::from(sub);
                    Self::exit(vmcb, code::CPUID, 0, 0, next);
                    self.pending = pend(true, false);
                    return;
                }
                Step::Rdmsr { msr } | Step::Wrmsr { msr, .. } => {
                    let write = matches!(step, Step::Wrmsr { .. });
                    gprs.rcx = u64::from(msr);
                    if let Step::Wrmsr { value, .. } = step {
                        vmcb.set_rax(value & 0xFFFF_FFFF);
                        gprs.rdx = value >> 32;
                    }
                    if self.msr_intercepted(vmcb, msr, write) {
                        Self::exit(vmcb, code::MSR, u64::from(write), 0, next);
                        self.pending = pend(true, true);
                        return;
                    }
                    if let Step::Wrmsr { value, .. } = step {
                        self.msrs.insert(msr, value);
                    } else {
                        let v = self.msrs.get(&msr).copied().unwrap_or(0);
                        self.rdmsr_results.push(v);
                    }
                    self.pos += 1;
                }
                Step::Hlt => {
                    Self::exit(vmcb, code::HLT, 0, 0, next);
                    self.pending = pend(true, false);
                    return;
                }
                Step::Load { gpa } | Step::Store { gpa, .. } => {
                    let write = matches!(step, Step::Store { .. });
                    match self.translate(vmcb, gpa, write) {
                        Ok(hpa) => {
                            if let Step::Store { byte, .. } = step {
                                self.mem.write_bytes(hpa, &[byte]);
                            } else {
                                let mut b = [0u8];
                                self.mem.read_bytes(hpa, &mut b);
                                self.loads.push(b[0]);
                            }
                            self.pos += 1;
                        }
                        Err(err) => {
                            Self::exit(vmcb, code::NPF, err, gpa, rip);
                            self.pending = pend(false, false);
                            return;
                        }
                    }
                }
                Step::Npf { gpa, error } => {
                    Self::exit(vmcb, code::NPF, error, gpa, rip);
                    vmcb.write_u8(ctl::INSN_LEN, 0);
                    self.pending = pend(false, false);
                    return;
                }
                Step::Tick => {
                    Self::exit(vmcb, code::INTR, 0, 0, rip);
                    self.pending = pend(false, false);
                    return;
                }
                Step::SpinForever => {
                    Self::exit(vmcb, code::INTR, 0, 0, rip);
                    return;
                }
                Step::TripleFault => {
                    Self::exit(vmcb, code::SHUTDOWN, 0, 0, rip);
                    self.pending = pend(false, false);
                    return;
                }
                Step::Vmmcall => {
                    Self::exit(vmcb, code::VMMCALL, 0, 0, next);
                    self.pending = pend(true, true);
                    return;
                }
            }
        }
    }

    fn host_cpuid(&mut self, leaf: u32, subleaf: u32) -> [u32; 4] {
        self.host_cpuid
            .get(&(leaf, subleaf))
            .copied()
            .unwrap_or([0; 4])
    }

    fn read_guest_phys(&mut self, gpa: u64, out: &mut [u8]) -> bool {
        for (i, b) in out.iter_mut().enumerate() {
            let Some(hpa) = self.npt_walk(gpa + i as u64) else {
                return false;
            };
            let mut x = [0u8];
            self.mem.read_bytes(hpa, &mut x);
            *b = x[0];
        }
        true
    }
}

pub struct FakeClock {
    pub now: u64,
    pub step: u64,
}

impl Clock for FakeClock {
    fn now_us(&mut self) -> u64 {
        self.now += self.step;
        self.now
    }
}

/// A guest with 16 pages of RAM at GPA 0, permission maps and a VMCB.
pub struct Rig {
    pub cpu: FakeCpu,
    pub npt: Npt,
    pub page: Box<[u8; 4096]>,
    pub gprs: Gprs,
    pub cfg: VmConfig,
    pub ram: Vec<u64>,
    pub clock: FakeClock,
    prepared: bool,
}

impl Rig {
    /// A rig for `PlatformVcpu`: its MSR policy in the permission map.
    pub fn platform(script: &[Step]) -> Self {
        let mut rig = Self::new(script);
        rig.cpu.mem.write_bytes(MSRPM_PA, &platform_msr_map_bytes());
        rig
    }

    pub fn new(script: &[Step]) -> Self {
        let mut mem = Mem::new();
        let mut frames = Frames::new();
        let mut npt = Npt::new(&mut mem, &mut frames, 48).expect("npt");
        let mut ram = Vec::new();
        for i in 0..RAM_PAGES {
            let f = frames.alloc_frame().unwrap();
            mem.write_bytes(f, &[i as u8; 4096]);
            npt.map(&mut mem, &mut frames, i * PAGE_SIZE, f, PAGE_SIZE, RW)
                .expect("map RAM");
            ram.push(f);
        }
        mem.write_bytes(MSRPM_PA, &msr_map_bytes());
        mem.write_bytes(IOPM_PA, &io_map_bytes());
        let cfg = VmConfig::new(1, MSRPM_PA, IOPM_PA, npt.root(), true);
        let mut page = Box::new([0u8; 4096]);
        {
            let mut vmcb = Vmcb::new(&mut page);
            vmcb.setup_long_mode(ENTRY, 0x2000, 0x8000);
        }
        Self {
            cpu: FakeCpu::new(mem, frames, script),
            npt,
            page,
            gprs: Gprs::default(),
            cfg,
            ram,
            clock: FakeClock { now: 0, step: 1 },
            prepared: false,
        }
    }

    /// Maps more guest RAM: pages `RAM_PAGES..pages` at their identity GPAs (filled with zeros).
    pub fn map_ram(&mut self, pages: u64) {
        for i in self.ram.len() as u64..pages {
            let f = self.cpu.frames.alloc_frame().unwrap();
            self.cpu.mem.write_bytes(f, &[0; 4096]);
            self.npt
                .map(
                    &mut self.cpu.mem,
                    &mut self.cpu.frames,
                    i * PAGE_SIZE,
                    f,
                    PAGE_SIZE,
                    RW,
                )
                .expect("map RAM");
            self.ram.push(f);
        }
    }

    /// The VMCB, e.g. to corrupt guest state in a test.
    pub fn vmcb(&mut self) -> Vmcb<'_> {
        Vmcb::wrap(&mut self.page)
    }

    /// Makes the guest's code fetchable: guest page tables at GPA 0x2000
    /// (the CR3 of `setup_long_mode`) identity-mapping 2 MiB, and the
    /// bytes of every MMIO step at its RIP.
    pub fn place_code(&mut self) {
        for (page, entry) in [(2, 0x3000 | 3), (3, 0x4000 | 3), (4, 0x83)] {
            let f = self.ram[page];
            self.cpu.mem.write_bytes(f, &[0; 4096]);
            self.cpu.mem.write_u64(f, entry);
        }
        for i in 0..self.cpu.steps.len() {
            let (rip, step) = self.cpu.steps[i];
            if let Step::Mmio { insn, len, .. } = step {
                let f = self.ram[(rip >> 12) as usize] + (rip & 0xFFF);
                self.cpu.mem.write_bytes(f, &insn[..usize::from(len)]);
            }
        }
    }
}

/// Runs the rig's guest on `vcpu`.
pub fn run(rig: &mut Rig, vcpu: &mut Vcpu<'_>) -> Outcome {
    let mut vmcb = Vmcb::wrap(&mut rig.page);
    if !rig.prepared {
        vcpu.prepare(&mut vmcb);
        rig.prepared = true;
    }
    vcpu.run(&mut rig.cpu, &mut rig.clock, &mut vmcb, &mut rig.gprs)
}

/// Runs the rig's guest on the platform vCPU with `host`.
pub fn run_platform(rig: &mut Rig, vcpu: &mut PlatformVcpu<'_>, host: &mut Host<'_>) -> Outcome {
    let mut vmcb = Vmcb::wrap(&mut rig.page);
    if !rig.prepared {
        vcpu.prepare(&mut vmcb);
        rig.prepared = true;
    }
    vcpu.run(&mut rig.cpu, &mut rig.clock, host, &mut vmcb, &mut rig.gprs)
}

pub fn platform_msr_map_bytes() -> Vec<u8> {
    let mut m = [0u8; MSRPM_BYTES];
    let mut map = MsrPermissionMap::intercept_all(&mut m);
    PlatformVcpu::msr_policy(&mut map);
    m.to_vec()
}

pub fn msr_map_bytes() -> Vec<u8> {
    let mut m = [0u8; MSRPM_BYTES];
    let mut map = MsrPermissionMap::intercept_all(&mut m);
    Vcpu::msr_policy(&mut map);
    m.to_vec()
}

pub fn io_map_bytes() -> Vec<u8> {
    let mut m = [0u8; IOPM_BYTES];
    let _ = IoPermissionMap::intercept_all(&mut m);
    m.to_vec()
}

pub const RW: NptPerms = NptPerms::RWX;
