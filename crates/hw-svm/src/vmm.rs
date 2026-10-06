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
//! spins without exits. The loop, the exits handled alike and the MMIO
//! emulation are shared with [`crate::platform_vm`] (module `shared`).

use crate::exit::IoExit;
use crate::npt::NeedsFlush;
use crate::perm::MsrPermissionMap;
use crate::shared::{self, Bus, Core, Guest, Own};
use crate::vmcb::{bits, Gprs, StateError, Vmcb};
use crate::{Clock, SvmCpu};
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
    /// A device-memory access the VMM cannot emulate: the instruction could
    /// not be fetched or is not one of the decoded forms.
    MmioUnsupported { gpa: u64, rip: u64 },
    /// String or REP I/O, which this VMM does not emulate.
    UnsupportedIo { port: u16 },
    /// An exit this VMM does not handle.
    UnhandledExit(u64),
    /// The guest state failed the VMRUN consistency checks.
    Invalid(StateError),
    /// The guest reset the machine (port 0x92 or the keyboard controller;
    /// platform VMM only).
    Reset,
    /// The guest entered ACPI S5, soft-off (platform VMM only).
    PowerOff,
    /// The guest asked for another ACPI sleep state: `slp_typ` as written to
    /// PM1a_CNT (platform VMM only; sleep is not modeled).
    Sleep { slp_typ: u8 },
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
    /// Interrupts injected.
    pub irqs: u64,
    /// Device MMIO accesses emulated.
    pub mmio: u64,
    /// Virtual time at the end of the run.
    pub virtual_ns: u64,
}

/// One vCPU.
pub struct Vcpu<'s> {
    core: Core<'s>,
    /// Last write to the line control register; with DLAB (bit 7) set,
    /// `serial_base` is the divisor latch, not the transmitter.
    lcr: u8,
    lapic: Lapic,
    pit: Pit,
}

/// The APIC page: only aligned 32-bit accesses reach registers; others read
/// 0 and are dropped, like reserved fields.
struct ApicPage<'a> {
    lapic: &'a mut Lapic,
    now: u64,
}

impl ApicPage<'_> {
    fn register(gpa: u64, size: u8) -> Option<u32> {
        let offset = (gpa & 0xFFF) as u32;
        (size == 4 && offset.is_multiple_of(16)).then_some(offset)
    }
}

impl Bus for ApicPage<'_> {
    fn read(&mut self, gpa: u64, size: u8) -> u64 {
        Self::register(gpa, size).map_or(0, |o| u64::from(self.lapic.read(o, self.now)))
    }

    fn write(&mut self, gpa: u64, size: u8, value: u64) {
        if let Some(o) = Self::register(gpa, size) {
            self.lapic.write(o, value as u32, self.now);
        }
    }
}

impl<'s> Vcpu<'s> {
    /// `serial` receives the guest's serial output.
    pub fn new(cfg: VmConfig, serial: &'s mut [u8]) -> Self {
        Self {
            core: Core::new(cfg, serial),
            lcr: 0,
            lapic: Lapic::new(cfg.lapic_bus_hz.max(1)),
            pit: Pit::new(),
        }
    }

    /// Programs the control area of `vmcb`: intercepts, ASID, nested
    /// paging, permission maps. Guest state is set separately (e.g.
    /// [`Vmcb::setup_long_mode`]).
    pub fn prepare(&self, vmcb: &mut Vmcb<'_>) {
        self.core.prepare(vmcb);
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
        self.core.note_unmap();
    }

    /// Serial output captured so far.
    pub fn serial(&self) -> &[u8] {
        self.core.serial()
    }

    /// The emulated local APIC.
    pub fn lapic(&self) -> &Lapic {
        &self.lapic
    }

    /// Virtual time in nanoseconds.
    pub fn now_ns(&self) -> u64 {
        self.core.now
    }

    /// Runs the guest until a verdict.
    pub fn run<C: SvmCpu + ?Sized, K: Clock + ?Sized>(
        &mut self,
        cpu: &mut C,
        clock: &mut K,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Outcome {
        shared::run(self, cpu, clock, vmcb, gprs)
    }

    fn msr(&mut self, vmcb: &mut Vmcb<'_>, gprs: &mut Gprs, write: bool) -> Option<Verdict> {
        if gprs.rcx as u32 == lapic::MSR_APIC_BASE {
            if !write {
                shared::msr_result(vmcb, gprs, self.lapic.read_msr());
                self.core.advance(vmcb, 2);
                return None;
            }
            if self
                .lapic
                .write_msr(shared::msr_operand(vmcb, gprs))
                .is_ok()
            {
                self.core.advance(vmcb, 2);
                return None;
            }
        }
        // Outside the allowlist and not emulated.
        self.core.msr_fault(vmcb);
        None
    }

    fn hlt(&mut self, vmcb: &mut Vmcb<'_>) -> Option<Verdict> {
        // A HLT with interrupts enabled waits for the APIC timer: skip
        // virtual time to its deadline. Anything else would never wake up.
        let can_wake = vmcb.rflags() & bits::RFLAGS_IF != 0;
        let deadline = self.lapic.next_deadline();
        if can_wake && (self.lapic.pending().is_some() || deadline.is_some()) {
            if self.lapic.pending().is_none() {
                self.core.now = self.core.now.max(deadline.unwrap_or(self.core.now));
            }
            self.core.advance(vmcb, 1);
            None
        } else {
            Some(Verdict::Halted)
        }
    }

    fn io(&mut self, vmcb: &mut Vmcb<'_>, io: IoExit) {
        let mask = shared::size_mask(io.size);
        let base = self.core.cfg.serial_base;
        let now = self.core.now;
        if io.input {
            let v: u64 = if io.port == base + 5 {
                0x60 // LSR: THRE | TEMT
            } else if (base..base + 8).contains(&io.port) {
                0
            } else if let Some(b) = self.pit.read(io.port, now) {
                u64::from(b)
            } else if matches!(io.port, 0x21 | 0xA1) {
                0xFF // PIC masks: everything masked
            } else if matches!(io.port, 0x20 | 0xA0) {
                0
            } else {
                mask // nothing decodes the port
            };
            vmcb.set_rax(shared::in_result(vmcb.rax(), io.size, v));
        } else if io.port == base + 3 {
            self.lcr = vmcb.rax() as u8;
        } else if io.port == base && self.lcr & 0x80 == 0 {
            // Found by booting the M0 kernel under svm-probe: its UART init
            // writes the divisor (1) to the base port with DLAB set.
            self.core.put_serial(vmcb.rax() as u8);
        } else {
            // The PIT and port 0x61; other ports ignore writes.
            let _ = self.pit.write(io.port, vmcb.rax() as u8, now);
        }
        shared::skip_to(vmcb, io.next_rip);
    }
}

impl<'s> Guest<'s> for Vcpu<'s> {
    fn core(&mut self) -> &mut Core<'s> {
        &mut self.core
    }

    fn enter(&mut self, vmcb: &mut Vmcb<'_>) {
        self.lapic.update(self.core.now);
        let lapic = &mut self.lapic;
        shared::deliver(&mut self.core, vmcb, lapic.pending().is_some(), || {
            let v = lapic.pending()?;
            lapic.accept(v);
            Some(v)
        });
    }

    fn handle<C: SvmCpu + ?Sized>(
        &mut self,
        cpu: &mut C,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Option<Verdict> {
        match shared::common_exit(&mut self.core, cpu, vmcb, gprs, shared::no_extra) {
            Ok(v) => v,
            Err(Own::Msr { write }) => self.msr(vmcb, gprs, write),
            Err(Own::Io(io)) => {
                self.io(vmcb, io);
                None
            }
            Err(Own::Hlt) => self.hlt(vmcb),
            // Not intercepted by this vCPU.
            Err(Own::Rdtsc) => Some(Verdict::UnhandledExit(vmcb.exit_code())),
            Err(Own::Npf { gpa, error }) => {
                let base = self.lapic.base();
                let enabled = self.lapic.read_msr() & lapic::MSR_ENABLE != 0;
                if enabled && (base..base + 0x1000).contains(&gpa) {
                    let mut page = ApicPage {
                        lapic: &mut self.lapic,
                        now: self.core.now,
                    };
                    return shared::emulate(&mut self.core, cpu, vmcb, gprs, gpa, &mut page);
                }
                Some(Verdict::NestedPageFault { gpa, error })
            }
        }
    }
}
