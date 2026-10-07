//! What the two vCPU loops ([`crate::vmm`] for NANOX candidates,
//! [`crate::platform_vm`] for a guest on the whole emulated platform) have in
//! common:
//!
//! * [`run`] — the entry/exit cycle: TLB control, the VMRUN checks, virtual
//!   time charged per exit, and the exit, virtual-time and wall-time budgets;
//! * [`Core`] — the counters, the captured serial output, the control area
//!   ([`Core::prepare`]) and RIP advance;
//! * [`common_exit`] — the exits both handle alike: host interrupts, the
//!   virtual-interrupt window, INIT and shutdown, CPUID (with a per-loop
//!   adjustment), PAUSE, SVM instructions, the debug-exit port and string
//!   I/O; the rest come back as [`Own`];
//! * [`deliver`] — EVENTINJ when the guest can take an interrupt, otherwise
//!   the virtual-interrupt window (V_IRQ + VINTR intercept);
//! * [`emulate`] — MMIO emulated from the faulting instruction (decode
//!   assists, or fetched through the guest's page tables) over a [`Bus`]
//!   that each loop provides.

use crate::exit::{code, Exit, IoExit};
use crate::guest;
use crate::vmcb::{bits, ctl, misc1, misc2, save, tlb, vintr, Gprs, Vmcb};
use crate::vmm::{Outcome, Verdict, VmConfig};
use crate::{Clock, Error, SvmCpu};
use vmm_devices::decode::{self, Operation, Source};

pub(crate) const GP: u64 = 13;
pub(crate) const UD: u64 = 6;
pub(crate) const EVENT_VALID: u64 = 1 << 31;

/// EVENTINJ for a hardware exception (type 3), with error code 0 when
/// `with_error`.
pub(crate) fn exception(vector: u64, with_error: bool) -> u64 {
    vector | 3 << 8 | u64::from(with_error) << 11 | EVENT_VALID
}

/// The bits of an access of `size` bytes (1, 2, 4 or 8).
pub(crate) fn size_mask(size: u8) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * size)) - 1
    }
}

/// RAX after IN of `size` bytes reading `v`: IN AL/AX merge into RAX, IN EAX
/// zero-extends like every 32-bit register write in 64-bit mode.
pub(crate) fn in_result(rax: u64, size: u8, v: u64) -> u64 {
    let mask = size_mask(size);
    if size == 4 {
        v & mask
    } else {
        (rax & !mask) | (v & mask)
    }
}

/// EDX:EAX of a WRMSR.
pub(crate) fn msr_operand(vmcb: &Vmcb<'_>, g: &Gprs) -> u64 {
    (g.rdx << 32) | (vmcb.rax() & 0xFFFF_FFFF)
}

/// An RDMSR result into EDX:EAX.
pub(crate) fn msr_result(vmcb: &mut Vmcb<'_>, g: &mut Gprs, v: u64) {
    vmcb.set_rax(v & 0xFFFF_FFFF);
    g.rdx = v >> 32;
}

pub(crate) fn get_reg(vmcb: &Vmcb<'_>, g: &Gprs, index: u8) -> u64 {
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

pub(crate) fn set_reg(vmcb: &mut Vmcb<'_>, g: &mut Gprs, index: u8, v: u64) {
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
pub(crate) fn skip_to(vmcb: &mut Vmcb<'_>, rip: u64) {
    vmcb.set_rip(rip);
    let shadow = vmcb.read_u64(ctl::INTERRUPT_SHADOW);
    vmcb.write_u64(ctl::INTERRUPT_SHADOW, shadow & !1);
}

/// State and counters of a vCPU loop.
pub(crate) struct Core<'s> {
    pub cfg: VmConfig,
    serial: &'s mut [u8],
    serial_len: usize,
    truncated: bool,
    /// TLB_CONTROL for the next VMRUN.
    pub flush: u8,
    pub exits: u64,
    pub msr_faults: u32,
    pub ud: u32,
    pub irqs: u64,
    pub mmio: u64,
    /// Virtual time in nanoseconds.
    pub now: u64,
}

impl<'s> Core<'s> {
    pub fn new(cfg: VmConfig, serial: &'s mut [u8]) -> Self {
        Self {
            cfg,
            serial,
            serial_len: 0,
            truncated: false,
            flush: tlb::FLUSH_ALL,
            exits: 0,
            msr_faults: 0,
            ud: 0,
            irqs: 0,
            mmio: 0,
            now: 0,
        }
    }

    pub fn serial(&self) -> &[u8] {
        &self.serial[..self.serial_len]
    }

    /// Captures one byte of serial output; a full buffer drops it and says so.
    pub fn put_serial(&mut self, byte: u8) {
        if self.serial_len < self.serial.len() {
            self.serial[self.serial_len] = byte;
            self.serial_len += 1;
        } else {
            self.truncated = true;
        }
    }

    pub fn outcome(&self, verdict: Verdict) -> Outcome {
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

    /// Programs the control area of `vmcb`: intercepts, ASID, nested paging,
    /// permission maps.
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

    /// Nested mappings were removed: the next VMRUN flushes this guest's TLB.
    pub fn note_unmap(&mut self) {
        if self.flush == tlb::NOTHING {
            self.flush = if self.cfg.flush_by_asid {
                tlb::FLUSH_ASID
            } else {
                tlb::FLUSH_ALL
            };
        }
    }

    /// Moves RIP past the intercepted instruction (`len` bytes without NRIP
    /// save).
    pub fn advance(&self, vmcb: &mut Vmcb<'_>, len: u64) {
        let next = if self.cfg.nrips {
            vmcb.nrip()
        } else {
            vmcb.rip().wrapping_add(len)
        };
        skip_to(vmcb, next);
    }

    /// #GP for an MSR access outside the policy.
    pub fn msr_fault(&mut self, vmcb: &mut Vmcb<'_>) {
        self.msr_faults += 1;
        vmcb.set_event_inj(exception(GP, true));
    }
}

/// The part of the loop that differs between the vCPUs.
pub(crate) trait Guest<'s> {
    fn core(&mut self) -> &mut Core<'s>;
    /// Before VMRUN: bring the devices to the current virtual time and
    /// inject an interrupt or ask for a window ([`deliver`]).
    fn enter(&mut self, vmcb: &mut Vmcb<'_>);
    /// Handles the exit in `vmcb`; a verdict ends the run.
    fn handle<C: SvmCpu + ?Sized>(
        &mut self,
        cpu: &mut C,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Option<Verdict>;
}

/// Runs the guest until a verdict.
pub(crate) fn run<'s, G, C, K>(
    g: &mut G,
    cpu: &mut C,
    clock: &mut K,
    vmcb: &mut Vmcb<'_>,
    gprs: &mut Gprs,
) -> Outcome
where
    G: Guest<'s> + ?Sized,
    C: SvmCpu + ?Sized,
    K: Clock + ?Sized,
{
    let start = clock.now_us();
    loop {
        g.enter(vmcb);
        let c = g.core();
        vmcb.set_tlb_control(c.flush);
        if let Err(Error::InvalidState(e)) = vmcb.check() {
            return c.outcome(Verdict::Invalid(e));
        }
        cpu.vmrun(vmcb, gprs);
        vmcb.set_tlb_control(tlb::NOTHING);
        c.flush = tlb::NOTHING;
        c.exits += 1;
        let charge = if vmcb.exit_code() == code::INTR {
            c.cfg.intr_exit_ns
        } else {
            c.cfg.exit_quantum_ns
        };
        c.now = c.now.saturating_add(charge);
        if let Some(v) = g.handle(cpu, vmcb, gprs) {
            return g.core().outcome(v);
        }
        let c = g.core();
        if c.exits >= c.cfg.max_exits {
            return c.outcome(Verdict::ExitBudget);
        }
        if c.now >= c.cfg.max_virtual_ns {
            return c.outcome(Verdict::VirtualTimeout);
        }
        if clock.now_us().saturating_sub(start) >= c.cfg.max_time_us {
            return c.outcome(Verdict::Timeout);
        }
    }
}

/// Injects an interrupt if one is `pending` and the guest can take it now
/// (`take` acknowledges it at the interrupt controller and gives its vector),
/// else asks for an exit as soon as it can (virtual interrupt window).
pub(crate) fn deliver(
    core: &mut Core<'_>,
    vmcb: &mut Vmcb<'_>,
    pending: bool,
    take: impl FnOnce() -> Option<u8>,
) {
    if !pending {
        window(vmcb, false);
        return;
    }
    let ready = vmcb.rflags() & bits::RFLAGS_IF != 0
        && vmcb.read_u64(ctl::INTERRUPT_SHADOW) & 1 == 0
        && vmcb.event_inj() & EVENT_VALID == 0;
    if ready {
        if let Some(v) = take() {
            // Type 0 (external interrupt).
            vmcb.set_event_inj(u64::from(v) | EVENT_VALID);
            core.irqs += 1;
        }
        window(vmcb, false);
    } else {
        window(vmcb, true);
    }
}

fn window(vmcb: &mut Vmcb<'_>, open: bool) {
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

/// Exits a vCPU handles itself.
pub(crate) enum Own {
    Msr { write: bool },
    Rdtsc,
    Io(IoExit),
    Hlt,
    Npf { gpa: u64, error: u64 },
}

/// A CPUID adjustment of one loop on top of [`cpuid`]: (leaf, subleaf, result).
pub(crate) type CpuidExtra = fn(u32, u32, &mut [u32; 4]);

/// No adjustment.
pub(crate) fn no_extra(_: u32, _: u32, _: &mut [u32; 4]) {}

/// Handles the exits both loops treat alike (`Ok`, with a verdict if the run
/// ends); the others come back as `Err`.
pub(crate) fn common_exit<C: SvmCpu + ?Sized>(
    core: &mut Core<'_>,
    cpu: &mut C,
    vmcb: &mut Vmcb<'_>,
    gprs: &mut Gprs,
    extra: CpuidExtra,
) -> Result<Option<Verdict>, Own> {
    Ok(match Exit::decode(vmcb) {
        // Delivered before the next VMRUN.
        Exit::Intr | Exit::Nmi | Exit::Smi | Exit::Vintr => None,
        Exit::Init | Exit::Shutdown => Some(Verdict::Shutdown),
        Exit::Cpuid => {
            let leaf = vmcb.rax() as u32;
            let sub = gprs.rcx as u32;
            let mut r = cpuid(cpu, leaf, sub);
            extra(leaf, sub, &mut r);
            let [a, b, c, d] = r;
            vmcb.set_rax(u64::from(a));
            gprs.rbx = u64::from(b);
            gprs.rcx = u64::from(c);
            gprs.rdx = u64::from(d);
            core.advance(vmcb, 2);
            None
        }
        // A spin-wait: the exit itself charged the time quantum, so a guest
        // polling memory (no I/O) still sees its timer fire.
        Exit::Pause => {
            core.advance(vmcb, 2);
            None
        }
        Exit::Rdtsc => return Err(Own::Rdtsc),
        // RDTSCP is hidden from the guest (CPUID) where RDTSC is intercepted.
        Exit::Vmmcall | Exit::SvmInstruction(_) | Exit::Rdtscp => {
            core.ud += 1;
            vmcb.set_event_inj(exception(UD, false));
            None
        }
        Exit::Io(io) if io.string || io.rep => Some(Verdict::UnsupportedIo { port: io.port }),
        Exit::Io(io) if !io.input && io.port == core.cfg.debug_exit_port => {
            let value = (vmcb.rax() & size_mask(io.size)) as u32;
            // Past the OUT, so a caller may resume the guest.
            skip_to(vmcb, io.next_rip);
            Some(Verdict::DebugExit {
                value,
                status: value.wrapping_shl(1) | 1,
            })
        }
        Exit::Io(io) => return Err(Own::Io(io)),
        Exit::Msr { write } => return Err(Own::Msr { write }),
        Exit::Hlt => return Err(Own::Hlt),
        Exit::NestedPageFault { gpa, error } => return Err(Own::Npf { gpa, error }),
        // VMRUN refused a state our checks accepted: report the failing
        // check if there is one now, else the raw exit code.
        Exit::Invalid => Some(match vmcb.check() {
            Err(Error::InvalidState(e)) => Verdict::Invalid(e),
            _ => Verdict::UnhandledExit(code::INVALID),
        }),
        Exit::Exception { .. } | Exit::Other(_) => Some(Verdict::UnhandledExit(vmcb.exit_code())),
    })
}

/// The host's CPUID filtered for a guest: hypervisor present with the
/// "NanoxVMM" leaf; no VMX, x2APIC, MONITOR/MWAIT or TSC-deadline (the
/// emulated APIC is xAPIC with the classic timer only); no nested SVM;
/// leaves above the host's maxima read as zero (the other hypervisor leaves,
/// 4000_0001h on, among them: no basic maximum comes near them).
pub(crate) fn cpuid<C: SvmCpu + ?Sized>(cpu: &mut C, leaf: u32, sub: u32) -> [u32; 4] {
    if leaf == 0x4000_0000 {
        let v = crate::vmm::VENDOR;
        let w = |i: usize| u32::from_le_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
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
        1 => {
            r[2] |= 1 << 31;
            r[2] &= !((1 << 5) | (1 << 21) | (1 << 3) | (1 << 24));
        }
        0x8000_0001 => r[2] &= !(1 << 2),
        0x8000_000A => r = [0; 4],
        _ => {}
    }
    r
}

/// Device memory as the instruction emulation reaches it.
pub(crate) trait Bus {
    /// Reads `size` bytes at `gpa`; the value has no bits above them.
    fn read(&mut self, gpa: u64, size: u8) -> u64;
    /// Writes `value` (no bits above `size` bytes) to `gpa`.
    fn write(&mut self, gpa: u64, size: u8, value: u64);
}

/// The value of a store's or an ALU operation's source operand.
fn source(vmcb: &Vmcb<'_>, g: &Gprs, src: Source) -> u64 {
    match src {
        Source::Reg(r) => decode::source_value(get_reg(vmcb, g, r.index), r),
        Source::Imm(i) => i,
    }
}

/// Emulates the instruction at RIP that faulted on device memory at `gpa`
/// (`vmm_devices::decode` forms: MOV, MOVZX, OR/AND/XOR, CMP/TEST) and moves
/// RIP past it; `MmioUnsupported` if its bytes cannot be had or decoded.
pub(crate) fn emulate<C: SvmCpu + ?Sized, B: Bus + ?Sized>(
    core: &mut Core<'_>,
    cpu: &mut C,
    vmcb: &mut Vmcb<'_>,
    gprs: &mut Gprs,
    gpa: u64,
    bus: &mut B,
) -> Option<Verdict> {
    let rip = vmcb.rip();
    let mut bytes = [0u8; guest::MAX_INSN];
    let n = guest::fetch(cpu, vmcb, &mut bytes);
    let Ok(insn) = decode::decode(&bytes[..n]) else {
        return Some(Verdict::MmioUnsupported { gpa, rip });
    };
    match insn.op {
        Operation::Load { reg, size, dest } => {
            let v = bus.read(gpa, size);
            let old = get_reg(vmcb, gprs, reg.index);
            set_reg(vmcb, gprs, reg.index, decode::merge(old, reg, dest, v));
        }
        Operation::Store { src, size } => {
            // The processor stores `size` bytes of the register, not all of it.
            let v = source(vmcb, gprs, src) & size_mask(size);
            bus.write(gpa, size, v);
        }
        Operation::Rmw { alu, src, size } => {
            let old = bus.read(gpa, size);
            let (new, flags) = alu.apply(old, source(vmcb, gprs, src), size, vmcb.rflags());
            vmcb.write_u64(save::RFLAGS, flags);
            bus.write(gpa, size, new);
        }
        Operation::Flags {
            op,
            src,
            size,
            mem_first,
        } => {
            let m = bus.read(gpa, size);
            let x = source(vmcb, gprs, src);
            let (a, b) = if mem_first { (m, x) } else { (x, m) };
            vmcb.write_u64(save::RFLAGS, op.flags(a, b, size, vmcb.rflags()));
        }
    }
    core.mmio += 1;
    skip_to(vmcb, rip.wrapping_add(u64::from(insn.len)));
    None
}
