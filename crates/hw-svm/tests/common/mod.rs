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
}

impl Step {
    fn len(self) -> u64 {
        match self {
            Step::Out { .. } | Step::In { .. } | Step::OutString { .. } => 1,
            Step::Cpuid { .. } | Step::Rdmsr { .. } | Step::Wrmsr { .. } => 2,
            Step::Vmmcall => 3,
            Step::Hlt => 1,
            Step::Load { .. } | Step::Store { .. } => 3,
            Step::Tick | Step::SpinForever | Step::TripleFault => 2,
        }
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
    pub loads: Vec<u8>,
    pub vmruns: u64,
    pub msrs: HashMap<u32, u64>,
    pub host_cpuid: HashMap<(u32, u32), [u32; 4]>,
    pub violations: Vec<String>,
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
            loads: Vec::new(),
            vmruns: 0,
            msrs: HashMap::new(),
            host_cpuid,
            violations: Vec::new(),
        }
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
            _ => {}
        }
        self.pos = p.index + 1;
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
            match step {
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

    /// The VMCB, e.g. to corrupt guest state in a test.
    pub fn vmcb(&mut self) -> Vmcb<'_> {
        Vmcb::wrap(&mut self.page)
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
