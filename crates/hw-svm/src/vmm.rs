//! A limited VMM for testing candidate builds: one vCPU, nested paging,
//! every I/O port and every MSR outside a small allowlist intercepted, and
//! the devices a NANOX kernel test run needs:
//!
//! * a 16550 transmit path at `serial_base` (THR writes are captured unless
//!   LCR.DLAB selects the divisor latch, LSR reads report an empty
//!   transmitter, other registers read 0);
//! * the `isa-debug-exit` port: a write of `v` ends the run with exit
//!   status `(v << 1) | 1`, exactly as QEMU reports it to the M0 harness,
//!   so a candidate's test mode behaves the same under this VMM;
//! * a local APIC (xAPIC MMIO page, IA32_APIC_BASE) and the PIT with port
//!   0x61 (its IRQ0 is not wired here), from `vmm-devices`; the 8259 PICs only
//!   accept their masks.
//!
//! Time is virtual and deterministic: every exit advances it by
//! `exit_quantum_ns`, PAUSE is intercepted so spin-waits exit too, and a
//! HLT with interrupts enabled skips to the next APIC timer deadline. APIC interrupts are injected through EVENTINJ when
//! the guest can take them, otherwise a virtual-interrupt window (V_IRQ +
//! VINTR intercept) brings the VMM back as soon as it can. APIC MMIO is
//! emulated from the faulting instruction (decode assists if present,
//! otherwise fetched through the guest's page tables).
//!
//! The run ends with a [`Verdict`]; the host's timer interrupt (intercepted
//! INTR) lets the loop enforce a wall-time budget even for a guest that
//! spins without exits.

use crate::exit::{Exit, IoExit};
use crate::guest;
use crate::npt::NeedsFlush;
use crate::perm::MsrPermissionMap;
use crate::vmcb::{bits, ctl, misc1, misc2, save, tlb, vintr, Gprs, StateError, Vmcb};
use crate::{Clock, Error, SvmCpu};
use vmm_devices::decode::{self, Operation, Source};
use vmm_devices::lapic::{self, Lapic};
use vmm_devices::pit::Pit;

/// Hypervisor vendor signature returned in CPUID 4000_0000h (EBX, ECX,
/// EDX).
pub const VENDOR: &[u8; 12] = b"NanoxVMM\0\0\0\0";

/// MSRs the guest may access directly: their state is part of the VMCB
/// save area (loaded/saved by VMLOAD/VMSAVE and VMRUN) or harmless.
pub const PASSTHROUGH_MSRS: [u32; 9] = [
    0x0000_0277, // PAT
    0xC000_0081, // STAR
    0xC000_0082, // LSTAR
    0xC000_0083, // CSTAR
    0xC000_0084, // SFMASK
    0xC000_0100, // FS.base
    0xC000_0101, // GS.base
    0xC000_0102, // KernelGSbase
    0xC000_0103, // TSC_AUX
];

/// Run settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmConfig {
    /// Guest ASID (1..number of ASIDs).
    pub asid: u32,
    /// Physical addresses of the permission maps (4 KiB aligned).
    pub msrpm_pa: u64,
    pub iopm_pa: u64,
    /// N_CR3.
    pub npt_root: u64,
    /// Processor saves the next RIP (SvmCaps::nrips).
    pub nrips: bool,
    /// TLB_CONTROL 3 (flush this ASID) is supported (SvmCaps::flush_by_asid);
    /// otherwise an unmap flushes the whole TLB (TLB_CONTROL 1). QEMU 9.2
    /// TCG does not offer it.
    pub flush_by_asid: bool,
    pub serial_base: u16,
    pub debug_exit_port: u16,
    /// Input clock of the emulated local APIC timer.
    pub lapic_bus_hz: u64,
    /// Virtual time charged per exit.
    pub exit_quantum_ns: u64,
    /// Virtual time charged for an exit caused by a host interrupt. A host
    /// that ticks a timer while the guest runs (IF=1 at VMRUN) lets a guest
    /// that spins without exits still see time pass: set this to the
    /// host's tick period. Such runs depend on host timing.
    pub intr_exit_ns: u64,
    /// Exits handled before the run is stopped.
    pub max_exits: u64,
    /// Wall time before the run is stopped.
    pub max_time_us: u64,
    /// Virtual time before the run is stopped (deterministic).
    pub max_virtual_ns: u64,
}

impl VmConfig {
    /// COM1, the M0 harness debug-exit port, a 1 GHz APIC bus and 1 us of
    /// virtual time per exit.
    pub fn new(asid: u32, msrpm_pa: u64, iopm_pa: u64, npt_root: u64, nrips: bool) -> Self {
        Self {
            asid,
            msrpm_pa,
            iopm_pa,
            npt_root,
            nrips,
            flush_by_asid: false,
            serial_base: 0x3F8,
            debug_exit_port: 0xF4,
            lapic_bus_hz: 1_000_000_000,
            exit_quantum_ns: 1_000,
            intr_exit_ns: 1_000,
            max_exits: 1_000_000,
            max_time_us: 30_000_000,
            max_virtual_ns: 60_000_000_000,
        }
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The guest wrote `value` to the debug-exit port; `status` is what
    /// QEMU would report, `(value << 1) | 1` (M0: 33 PASS, 35 FAIL).
    DebugExit { value: u32, status: u32 },
    /// Triple fault or other shutdown.
    Shutdown,
    /// HLT with no interrupt source that could wake the vCPU.
    Halted,
    /// Wall-time budget exhausted.
    Timeout,
    /// Virtual-time budget exhausted.
    VirtualTimeout,
    /// Exit budget exhausted.
    ExitBudget,
    /// Access to guest-physical memory with no or insufficient mapping.
    NestedPageFault { gpa: u64, error: u64 },
    /// An APIC access the VMM cannot emulate: the instruction could not be
    /// fetched or is not one of the decoded MOV forms.
    MmioUnsupported { gpa: u64, rip: u64 },
    /// String or REP I/O, which this VMM does not emulate.
    UnsupportedIo { port: u16 },
    /// An exit this VMM does not handle.
    UnhandledExit(u64),
    /// The guest state failed the VMRUN consistency checks.
    Invalid(StateError),
}

/// Result of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub verdict: Verdict,
    /// Exits handled.
    pub exits: u64,
    /// Bytes captured from the serial port.
    pub serial_len: usize,
    /// Serial output did not fit into the buffer.
    pub serial_truncated: bool,
    /// #GP injected for MSR accesses outside the policy.
    pub msr_faults: u32,
    /// #UD injected for VMMCALL and SVM instructions.
    pub ud_injected: u32,
    /// APIC interrupts injected.
    pub irqs: u64,
    /// APIC MMIO accesses emulated.
    pub mmio: u64,
    /// Virtual time at the end of the run.
    pub virtual_ns: u64,
}

/// One vCPU.
pub struct Vcpu<'s> {
    cfg: VmConfig,
    serial: &'s mut [u8],
    serial_len: usize,
    truncated: bool,
    /// Last write to the line control register; with DLAB (bit 7) set,
    /// `serial_base` is the divisor latch, not the transmitter.
    lcr: u8,
    flush: u8,
    exits: u64,
    msr_faults: u32,
    ud: u32,
    lapic: Lapic,
    pit: Pit,
    now: u64,
    irqs: u64,
    mmio: u64,
}

const GP: u64 = 13;
const UD: u64 = 6;
const EVENT_VALID: u64 = 1 << 31;

/// EVENTINJ for a hardware exception (type 3), with error code 0 when
/// `with_error`.
fn exception(vector: u64, with_error: bool) -> u64 {
    vector | 3 << 8 | u64::from(with_error) << 11 | EVENT_VALID
}

fn get_reg(vmcb: &Vmcb<'_>, g: &Gprs, index: u8) -> u64 {
    match index {
        0 => vmcb.rax(),
        1 => g.rcx,
        2 => g.rdx,
        3 => g.rbx,
        4 => vmcb.read_u64(save::RSP),
        5 => g.rbp,
        6 => g.rsi,
        7 => g.rdi,
        8 => g.r8,
        9 => g.r9,
        10 => g.r10,
        11 => g.r11,
        12 => g.r12,
        13 => g.r13,
        14 => g.r14,
        _ => g.r15,
    }
}

fn set_reg(vmcb: &mut Vmcb<'_>, g: &mut Gprs, index: u8, v: u64) {
    match index {
        0 => vmcb.set_rax(v),
        1 => g.rcx = v,
        2 => g.rdx = v,
        3 => g.rbx = v,
        4 => vmcb.write_u64(save::RSP, v),
        5 => g.rbp = v,
        6 => g.rsi = v,
        7 => g.rdi = v,
        8 => g.r8 = v,
        9 => g.r9 = v,
        10 => g.r10 = v,
        11 => g.r11 = v,
        12 => g.r12 = v,
        13 => g.r13 = v,
        14 => g.r14 = v,
        _ => g.r15 = v,
    }
}

/// Moves RIP past an emulated instruction; the interrupt shadow of a
/// preceding STI/MOV SS covered only that instruction.
fn skip_to(vmcb: &mut Vmcb<'_>, rip: u64) {
    vmcb.set_rip(rip);
    let shadow = vmcb.read_u64(ctl::INTERRUPT_SHADOW);
    vmcb.write_u64(ctl::INTERRUPT_SHADOW, shadow & !1);
}

impl<'s> Vcpu<'s> {
    /// `serial` receives the guest's serial output.
    pub fn new(cfg: VmConfig, serial: &'s mut [u8]) -> Self {
        Self {
            cfg,
            serial,
            serial_len: 0,
            truncated: false,
            lcr: 0,
            flush: tlb::FLUSH_ALL,
            exits: 0,
            msr_faults: 0,
            ud: 0,
            lapic: Lapic::new(cfg.lapic_bus_hz.max(1)),
            pit: Pit::new(),
            now: 0,
            irqs: 0,
            mmio: 0,
        }
    }

    /// Programs the control area of `vmcb`: intercepts, ASID, nested
    /// paging, permission maps. Guest state is set separately (e.g.
    /// [`Vmcb::setup_long_mode`]).
    pub fn prepare(&self, vmcb: &mut Vmcb<'_>) {
        vmcb.write_u32(
            ctl::INTERCEPT_MISC1,
            misc1::INTR
                | misc1::NMI
                | misc1::SMI
                | misc1::INIT
                | misc1::CPUID
                | misc1::PAUSE
                | misc1::HLT
                | misc1::IOIO_PROT
                | misc1::MSR_PROT
                | misc1::SHUTDOWN,
        );
        vmcb.write_u32(
            ctl::INTERCEPT_MISC2,
            misc2::VMRUN
                | misc2::VMMCALL
                | misc2::VMLOAD
                | misc2::VMSAVE
                | misc2::STGI
                | misc2::CLGI
                | misc2::SKINIT,
        );
        vmcb.write_u32(ctl::ASID, self.cfg.asid);
        vmcb.write_u64(ctl::NP_ENABLE, 1);
        vmcb.write_u64(ctl::N_CR3, self.cfg.npt_root);
        vmcb.write_u64(ctl::IOPM_BASE, self.cfg.iopm_pa);
        vmcb.write_u64(ctl::MSRPM_BASE, self.cfg.msrpm_pa);
        vmcb.write_u64(ctl::VINTR, vintr::V_INTR_MASKING);
        vmcb.set_event_inj(0);
    }

    /// Fills an MSR permission map with this VMM's policy.
    pub fn msr_policy(map: &mut MsrPermissionMap<'_>) {
        for msr in PASSTHROUGH_MSRS {
            map.allow(msr, true, true);
        }
    }

    /// Records that nested mappings were removed: the next VMRUN flushes
    /// this guest's TLB, after which the unmapped frames may be reused.
    pub fn note_unmap(&mut self, _token: NeedsFlush) {
        if self.flush == tlb::NOTHING {
            self.flush = if self.cfg.flush_by_asid {
                tlb::FLUSH_ASID
            } else {
                tlb::FLUSH_ALL
            };
        }
    }

    /// Serial output captured so far.
    pub fn serial(&self) -> &[u8] {
        &self.serial[..self.serial_len]
    }

    /// The emulated local APIC.
    pub fn lapic(&self) -> &Lapic {
        &self.lapic
    }

    /// Virtual time in nanoseconds.
    pub fn now_ns(&self) -> u64 {
        self.now
    }

    fn outcome(&self, verdict: Verdict) -> Outcome {
        Outcome {
            verdict,
            exits: self.exits,
            serial_len: self.serial_len,
            serial_truncated: self.truncated,
            msr_faults: self.msr_faults,
            ud_injected: self.ud,
            irqs: self.irqs,
            mmio: self.mmio,
            virtual_ns: self.now,
        }
    }

    /// Runs the guest until a verdict.
    pub fn run<C: SvmCpu + ?Sized, K: Clock + ?Sized>(
        &mut self,
        cpu: &mut C,
        clock: &mut K,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Outcome {
        let start = clock.now_us();
        loop {
            self.lapic.update(self.now);
            self.deliver(vmcb);
            vmcb.set_tlb_control(self.flush);
            if let Err(Error::InvalidState(e)) = vmcb.check() {
                return self.outcome(Verdict::Invalid(e));
            }
            cpu.vmrun(vmcb, gprs);
            vmcb.set_tlb_control(tlb::NOTHING);
            self.flush = tlb::NOTHING;
            self.exits += 1;
            let charge = if vmcb.exit_code() == crate::exit::code::INTR {
                self.cfg.intr_exit_ns
            } else {
                self.cfg.exit_quantum_ns
            };
            self.now = self.now.saturating_add(charge);
            if let Some(v) = self.handle(cpu, vmcb, gprs) {
                return self.outcome(v);
            }
            if self.exits >= self.cfg.max_exits {
                return self.outcome(Verdict::ExitBudget);
            }
            if self.now >= self.cfg.max_virtual_ns {
                return self.outcome(Verdict::VirtualTimeout);
            }
            if clock.now_us().saturating_sub(start) >= self.cfg.max_time_us {
                return self.outcome(Verdict::Timeout);
            }
        }
    }

    /// Injects the highest pending APIC interrupt if the guest can take it
    /// now, else asks for an exit when it can (virtual interrupt window).
    fn deliver(&mut self, vmcb: &mut Vmcb<'_>) {
        let Some(v) = self.lapic.pending() else {
            self.window(vmcb, false);
            return;
        };
        let ready = vmcb.rflags() & bits::RFLAGS_IF != 0
            && vmcb.read_u64(ctl::INTERRUPT_SHADOW) & 1 == 0
            && vmcb.event_inj() & EVENT_VALID == 0;
        if ready {
            // Type 0 (external interrupt).
            vmcb.set_event_inj(u64::from(v) | EVENT_VALID);
            self.lapic.accept(v);
            self.irqs += 1;
            self.window(vmcb, false);
        } else {
            self.window(vmcb, true);
        }
    }

    fn window(&self, vmcb: &mut Vmcb<'_>, open: bool) {
        let v = vmcb.read_u64(ctl::VINTR);
        let m = vmcb.read_u32(ctl::INTERCEPT_MISC1);
        let bits = vintr::V_IRQ | vintr::V_INTR_PRIO_MAX | vintr::V_IGN_TPR;
        if open {
            vmcb.write_u64(ctl::VINTR, v | bits);
            vmcb.write_u32(ctl::INTERCEPT_MISC1, m | misc1::VINTR);
        } else {
            vmcb.write_u64(ctl::VINTR, v & !bits);
            vmcb.write_u32(ctl::INTERCEPT_MISC1, m & !misc1::VINTR);
        }
    }

    fn advance(&self, vmcb: &mut Vmcb<'_>, len: u64) {
        let next = if self.cfg.nrips {
            vmcb.nrip()
        } else {
            vmcb.rip().wrapping_add(len)
        };
        skip_to(vmcb, next);
    }

    fn inject(&mut self, vmcb: &mut Vmcb<'_>, vector: u64, with_error: bool) {
        vmcb.set_event_inj(exception(vector, with_error));
    }

    fn handle<C: SvmCpu + ?Sized>(
        &mut self,
        cpu: &mut C,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Option<Verdict> {
        match Exit::decode(vmcb) {
            Exit::Intr | Exit::Nmi | Exit::Smi => None,
            // Delivered before the next VMRUN.
            Exit::Vintr => None,
            Exit::Init => Some(Verdict::Shutdown),
            Exit::Cpuid => {
                let leaf = vmcb.rax() as u32;
                let sub = gprs.rcx as u32;
                let [a, b, c, d] = self.cpuid(cpu, leaf, sub);
                vmcb.set_rax(u64::from(a));
                gprs.rbx = u64::from(b);
                gprs.rcx = u64::from(c);
                gprs.rdx = u64::from(d);
                self.advance(vmcb, 2);
                None
            }
            Exit::Msr { write } => {
                if gprs.rcx as u32 == lapic::MSR_APIC_BASE {
                    if !write {
                        let v = self.lapic.read_msr();
                        vmcb.set_rax(v & 0xFFFF_FFFF);
                        gprs.rdx = v >> 32;
                        self.advance(vmcb, 2);
                        return None;
                    }
                    let v = (gprs.rdx << 32) | (vmcb.rax() & 0xFFFF_FFFF);
                    if self.lapic.write_msr(v).is_ok() {
                        self.advance(vmcb, 2);
                        return None;
                    }
                }
                // Outside the allowlist and not emulated.
                self.msr_faults += 1;
                self.inject(vmcb, GP, true);
                None
            }
            Exit::Io(io) => self.io(vmcb, io),
            // A spin-wait: the exit itself charged the time quantum, so a
            // guest polling memory (no I/O) still sees its timer fire.
            Exit::Pause => {
                self.advance(vmcb, 2);
                None
            }
            Exit::Hlt => {
                // A HLT with interrupts enabled waits for the APIC timer:
                // skip virtual time to its deadline. Anything else would
                // never wake up.
                let can_wake = vmcb.rflags() & bits::RFLAGS_IF != 0;
                let deadline = self.lapic.next_deadline();
                if can_wake && (self.lapic.pending().is_some() || deadline.is_some()) {
                    if self.lapic.pending().is_none() {
                        self.now = self.now.max(deadline.unwrap_or(self.now));
                    }
                    self.advance(vmcb, 1);
                    None
                } else {
                    Some(Verdict::Halted)
                }
            }
            Exit::Shutdown => Some(Verdict::Shutdown),
            Exit::Vmmcall | Exit::SvmInstruction(_) => {
                self.ud += 1;
                self.inject(vmcb, UD, false);
                None
            }
            Exit::NestedPageFault { gpa, error } => {
                let base = self.lapic.base();
                let enabled = self.lapic.read_msr() & lapic::MSR_ENABLE != 0;
                if enabled && (base..base + 0x1000).contains(&gpa) {
                    return self.mmio(cpu, vmcb, gprs, gpa);
                }
                Some(Verdict::NestedPageFault { gpa, error })
            }
            // VMRUN refused a state our checks accepted: report the
            // failing check if there is one now, else the raw exit code.
            Exit::Invalid => Some(match vmcb.check() {
                Err(Error::InvalidState(e)) => Verdict::Invalid(e),
                _ => Verdict::UnhandledExit(crate::exit::code::INVALID),
            }),
            Exit::Exception { vector, .. } => {
                Some(Verdict::UnhandledExit(0x40 + u64::from(vector)))
            }
            Exit::Other(c) => Some(Verdict::UnhandledExit(c)),
        }
    }

    /// Emulates one access to the APIC page. Only aligned 32-bit accesses
    /// reach registers; others read 0 and are dropped, like reserved
    /// fields.
    fn mmio<C: SvmCpu + ?Sized>(
        &mut self,
        cpu: &mut C,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
        gpa: u64,
    ) -> Option<Verdict> {
        let rip = vmcb.rip();
        let mut bytes = [0u8; guest::MAX_INSN];
        let n = guest::fetch(cpu, vmcb, &mut bytes);
        let Ok(insn) = decode::decode(&bytes[..n]) else {
            return Some(Verdict::MmioUnsupported { gpa, rip });
        };
        let offset = (gpa & 0xFFF) as u32;
        let register = offset.is_multiple_of(16);
        match insn.op {
            Operation::Load { reg, size, dest } => {
                let v = if size == 4 && register {
                    u64::from(self.lapic.read(offset, self.now))
                } else {
                    0
                };
                let old = get_reg(vmcb, gprs, reg.index);
                set_reg(vmcb, gprs, reg.index, decode::merge(old, reg, dest, v));
            }
            Operation::Store { src, size } => {
                let v = match src {
                    Source::Reg(r) => decode::source_value(get_reg(vmcb, gprs, r.index), r),
                    Source::Imm(i) => i,
                };
                if size == 4 && register {
                    self.lapic.write(offset, v as u32, self.now);
                }
            }
            Operation::Rmw { alu, src, size } => {
                let old = if size == 4 && register {
                    u64::from(self.lapic.read(offset, self.now))
                } else {
                    0
                };
                let b = match src {
                    Source::Reg(r) => decode::source_value(get_reg(vmcb, gprs, r.index), r),
                    Source::Imm(i) => i,
                };
                let (new, flags) = alu.apply(old, b, size, vmcb.rflags());
                vmcb.write_u64(save::RFLAGS, flags);
                if size == 4 && register {
                    self.lapic.write(offset, new as u32, self.now);
                }
            }
            Operation::Flags {
                op,
                src,
                size,
                mem_first,
            } => {
                let m = if size == 4 && register {
                    u64::from(self.lapic.read(offset, self.now))
                } else {
                    0
                };
                let x = match src {
                    Source::Reg(r) => decode::source_value(get_reg(vmcb, gprs, r.index), r),
                    Source::Imm(i) => i,
                };
                let (a, b) = if mem_first { (m, x) } else { (x, m) };
                vmcb.write_u64(save::RFLAGS, op.flags(a, b, size, vmcb.rflags()));
            }
        }
        self.mmio += 1;
        skip_to(vmcb, rip.wrapping_add(u64::from(insn.len)));
        None
    }

    fn cpuid<C: SvmCpu + ?Sized>(&self, cpu: &mut C, leaf: u32, sub: u32) -> [u32; 4] {
        if (0x4000_0000..=0x4000_00FF).contains(&leaf) {
            if leaf != 0x4000_0000 {
                return [0; 4];
            }
            let w = |i: usize| {
                u32::from_le_bytes([VENDOR[i], VENDOR[i + 1], VENDOR[i + 2], VENDOR[i + 3]])
            };
            return [0x4000_0000, w(0), w(4), w(8)];
        }
        let max_basic = cpu.host_cpuid(0, 0)[0];
        let max_ext = cpu.host_cpuid(0x8000_0000, 0)[0];
        let supported = if leaf >= 0x8000_0000 {
            leaf <= max_ext
        } else {
            leaf <= max_basic
        };
        if !supported {
            return [0; 4];
        }
        let mut r = cpu.host_cpuid(leaf, sub);
        match leaf {
            // Hypervisor present; no VMX; no x2APIC, MONITOR/MWAIT or
            // TSC-deadline: the emulated APIC is xAPIC with the classic
            // timer only.
            1 => {
                r[2] |= 1 << 31;
                r[2] &= !((1 << 5) | (1 << 21) | (1 << 3) | (1 << 24));
            }
            // No nested SVM.
            0x8000_0001 => r[2] &= !(1 << 2),
            0x8000_000A => r = [0; 4],
            _ => {}
        }
        r
    }

    fn io(&mut self, vmcb: &mut Vmcb<'_>, io: IoExit) -> Option<Verdict> {
        if io.string || io.rep {
            return Some(Verdict::UnsupportedIo { port: io.port });
        }
        let mask = match io.size {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => 0xFFFF_FFFF,
        };
        let base = self.cfg.serial_base;
        if !io.input && io.port == self.cfg.debug_exit_port {
            let value = (vmcb.rax() & mask) as u32;
            // Past the OUT, so a caller may resume the guest.
            skip_to(vmcb, io.next_rip);
            return Some(Verdict::DebugExit {
                value,
                status: value.wrapping_shl(1) | 1,
            });
        }
        if io.input {
            let v: u64 = if io.port == base + 5 {
                0x60 // LSR: THRE | TEMT
            } else if (base..base + 8).contains(&io.port) {
                0
            } else if let Some(b) = self.pit.read(io.port, self.now) {
                u64::from(b)
            } else if matches!(io.port, 0x21 | 0xA1) {
                0xFF // PIC masks: everything masked
            } else if matches!(io.port, 0x20 | 0xA0) {
                0
            } else {
                mask // nothing decodes the port
            };
            // IN AL/AX merge into RAX; IN EAX zero-extends like every
            // 32-bit register write in 64-bit mode.
            let rax = if io.size == 4 {
                v & mask
            } else {
                (vmcb.rax() & !mask) | (v & mask)
            };
            vmcb.set_rax(rax);
        } else if io.port == base + 3 {
            self.lcr = vmcb.rax() as u8;
        } else if io.port == base && self.lcr & 0x80 == 0 {
            // Found by booting the M0 kernel under svm-probe: its UART init
            // writes the divisor (1) to the base port with DLAB set.
            let byte = vmcb.rax() as u8;
            if self.serial_len < self.serial.len() {
                self.serial[self.serial_len] = byte;
                self.serial_len += 1;
            } else {
                self.truncated = true;
            }
        } else {
            // The PIT and port 0x61; other ports ignore writes.
            let _ = self.pit.write(io.port, vmcb.rax() as u8, self.now);
        }
        skip_to(vmcb, io.next_rip);
        None
    }
}
