//! A limited VMM for testing candidate builds: one vCPU, nested paging,
//! every I/O port and every MSR outside a small allowlist intercepted, and
//! just enough devices for the M0 test protocol:
//!
//! * a 16550 transmit path at `serial_base` (THR writes are captured unless
//!   LCR.DLAB selects the divisor latch, LSR reads report an empty
//!   transmitter, other registers read 0);
//! * the `isa-debug-exit` port: a write of `v` ends the run with exit
//!   status `(v << 1) | 1`, exactly as QEMU reports it to the M0 harness,
//!   so a candidate's test mode behaves the same under this VMM.
//!
//! The run ends with a [`Verdict`]; the host's timer interrupt (intercepted
//! INTR) lets the loop enforce a time budget even for a guest that spins.

use crate::exit::{Exit, IoExit};
use crate::npt::NeedsFlush;
use crate::perm::MsrPermissionMap;
use crate::vmcb::{ctl, misc1, misc2, tlb, Gprs, StateError, Vmcb};
use crate::{Clock, Error, SvmCpu};

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
    /// Exits handled before the run is stopped.
    pub max_exits: u64,
    /// Wall time before the run is stopped.
    pub max_time_us: u64,
}

impl VmConfig {
    /// COM1 and the M0 harness debug-exit port.
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
            max_exits: 1_000_000,
            max_time_us: 30_000_000,
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
    /// Exit budget exhausted.
    ExitBudget,
    /// Access to guest-physical memory with no or insufficient mapping.
    NestedPageFault { gpa: u64, error: u64 },
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
}

const GP: u64 = 13;
const UD: u64 = 6;

/// EVENTINJ for a hardware exception (type 3), with error code 0 when
/// `with_error`.
fn exception(vector: u64, with_error: bool) -> u64 {
    vector | 3 << 8 | u64::from(with_error) << 11 | 1 << 31
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

    fn outcome(&self, verdict: Verdict) -> Outcome {
        Outcome {
            verdict,
            exits: self.exits,
            serial_len: self.serial_len,
            serial_truncated: self.truncated,
            msr_faults: self.msr_faults,
            ud_injected: self.ud,
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
            vmcb.set_tlb_control(self.flush);
            if let Err(Error::InvalidState(e)) = vmcb.check() {
                return self.outcome(Verdict::Invalid(e));
            }
            cpu.vmrun(vmcb, gprs);
            vmcb.set_tlb_control(tlb::NOTHING);
            self.flush = tlb::NOTHING;
            self.exits += 1;
            if let Some(v) = self.handle(cpu, vmcb, gprs) {
                return self.outcome(v);
            }
            if self.exits >= self.cfg.max_exits {
                return self.outcome(Verdict::ExitBudget);
            }
            if clock.now_us().saturating_sub(start) >= self.cfg.max_time_us {
                return self.outcome(Verdict::Timeout);
            }
        }
    }

    fn advance(&self, vmcb: &mut Vmcb<'_>, len: u64) {
        let next = if self.cfg.nrips {
            vmcb.nrip()
        } else {
            vmcb.rip().wrapping_add(len)
        };
        vmcb.set_rip(next);
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
            Exit::Msr { .. } => {
                // Everything that reaches here is outside the allowlist.
                self.msr_faults += 1;
                self.inject(vmcb, GP, true);
                None
            }
            Exit::Io(io) => self.io(vmcb, io),
            Exit::Hlt => Some(Verdict::Halted),
            Exit::Shutdown => Some(Verdict::Shutdown),
            Exit::Vmmcall | Exit::SvmInstruction(_) => {
                self.ud += 1;
                self.inject(vmcb, UD, false);
                None
            }
            Exit::NestedPageFault { gpa, error } => Some(Verdict::NestedPageFault { gpa, error }),
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
            // TSC-deadline without an emulated APIC.
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
            vmcb.set_rip(io.next_rip);
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
        }
        vmcb.set_rip(io.next_rip);
        None
    }
}
