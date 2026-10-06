//! A vCPU loop over the whole emulated platform, [`vmm_devices::machine::Machine`]:
//! the step towards a Linux guest (docs/specs/M11-WINDOW.md §5). Where
//! [`crate::vmm::Vcpu`] carries the few devices a NANOX candidate needs, this
//! one hands every port, every access to device memory and every interrupt to
//! the machine, on one virtual clock.
//!
//! * **Ports.** Every IN/OUT goes to `Machine::io_in`/`io_out` with its size
//!   (1, 2, 4); IN AL/AX change only their bytes of RAX, IN EAX zero-extends.
//!   The debug-exit port ends the run as in the candidate VMM. String and REP
//!   I/O end it with `Verdict::UnsupportedIo`: the devices Linux boots with
//!   here (8250, i8042, PCI CF8/CFC, RTC, PM, PIT) take single IN/OUT; the
//!   users of INS/OUTS (ATA PIO, QEMU's fw_cfg) are not modeled.
//! * **Device memory.** A nested page fault on an address with no RAM behind
//!   it (not present, a data access, not during the guest's own page-table
//!   walk) is emulated from the faulting instruction, as for the APIC page,
//!   and goes to `Machine::mmio_read`/`mmio_write`: the local APIC, I/O APIC,
//!   HPET, ECAM and the virtio BARs; anything else reads all ones and is
//!   counted by the machine (`unclaimed_mmio`). The caller maps all guest RAM
//!   in the nested tables; any other fault ends the run.
//! * **Interrupts.** Before each VMRUN `Machine::pending` brings every device
//!   to the current virtual time; if the guest can take the interrupt (IF,
//!   no interrupt shadow, no event pending) `Machine::acknowledge` gives the
//!   vector for EVENTINJ, otherwise the virtual-interrupt window brings the
//!   VMM back as soon as it can.
//! * **Time.** One virtual clock for the LAPIC timer, HPET, PIT, RTC and ACPI
//!   PM timer: `exit_quantum_ns` per exit, `intr_exit_ns` per host interrupt,
//!   and a HLT with IF=1 and nothing pending skips to `Machine::next_event`;
//!   with nothing pending or scheduled (or IF=0) the run ends `Halted`.
//! * **Platform outputs.** A reset request (port 0x92, keyboard controller)
//!   ends the run with `Verdict::Reset`, ACPI S5 with `PowerOff`, another
//!   sleep state with `Sleep`. COM1's output is captured like the candidate
//!   VMM's serial.
//! * **Virtio.** A write to the notification registers of a virtio BAR
//!   serves that device at once with the host's backend ([`Host`]). The host
//!   side is polled at the poll points, every HLT and every host interrupt:
//!   [`InputHost::poll`] hands input to the machine, then the input devices,
//!   the network and the agent channel are served (what arrived for the guest).
//! * **CPUID and MSRs** for Linux: the candidate policy plus what Linux needs
//!   before it can survive a #GP or with state the VMRUN checks refuse:
//!   [`linux_cpuid`] and [`PlatformVcpu::msr_policy`] say what and why.
//!
//! One vCPU; no MSI, no firmware: the guest's ACPI tables and boot handoff are
//! the caller's.

use crate::exit::{code, IoExit};
use crate::npt::NeedsFlush;
use crate::perm::MsrPermissionMap;
use crate::shared::{self, Bus, Core, Guest, Own};
use crate::vmcb::{bits, save, Gprs, Vmcb};
use crate::vmm::{Outcome, Vcpu, Verdict, VmConfig};
use crate::{Clock, SvmCpu};
use vmm_devices::acpi_pm;
use vmm_devices::lapic;
use vmm_devices::machine::{slot, Machine};
use vmm_devices::virtio::{self, GuestMemory};
use vmm_devices::virtio_blk::BlockBackend;
use vmm_devices::virtio_console::ConsoleBackend;
use vmm_devices::virtio_gpu::Scanout;
use vmm_devices::virtio_net::NetBackend;

pub const MSR_TSC: u32 = 0x10;
pub const MSR_MTRR_CAP: u32 = 0xFE;
pub const MSR_MTRR_DEF_TYPE: u32 = 0x2FF;
pub const MSR_EFER: u32 = 0xC000_0080;
/// IA32_MTRR_DEF_TYPE as firmware leaves it: MTRRs enabled, default type
/// write-back. With no variable or fixed ranges offered it is the type of
/// all memory.
pub const MTRR_DEF_TYPE_RESET: u64 = 0x806;
/// EFER bits a guest may write: SCE, LME, LMA (ignored, the processor owns
/// it) and NXE. SVME stays set for VMRUN but is hidden from the guest.
const EFER_GUEST: u64 = bits::EFER_SCE | bits::EFER_LME | bits::EFER_LMA | bits::EFER_NXE;
/// Nested page fault error code: the page is present (a permission fault),
/// the access was an instruction fetch, the fault came while translating
/// the guest's own page tables (APM 15.25.6).
const NPF_PRESENT: u64 = 1;
const NPF_FETCH: u64 = 1 << 4;
const NPF_TABLE_WALK: u64 = 1 << 33;

/// The host's input for the guest, asked at every poll point of the loop.
pub trait InputHost {
    /// Moves pending host events into the machine: `Machine::key` (a PS/2
    /// scancode byte), `machine.keyboard.key`, `machine.tablet.move_to` or
    /// `button`, `Machine::press_power_button`, `machine.uart.push_rx`.
    /// `now` is the virtual time.
    fn poll(&mut self, machine: &mut Machine, now: u64);
}

/// What the platform's devices reach on the host side during a run.
pub struct Host<'h> {
    /// Guest RAM for the devices' DMA (virtqueues and buffers).
    pub mem: &'h mut dyn GuestMemory,
    /// virtio-blk's medium; its size is the device's capacity.
    pub disk: &'h mut dyn BlockBackend,
    pub net: &'h mut dyn NetBackend,
    pub display: &'h mut dyn Scanout,
    /// The agent channel (virtio-console).
    pub console: &'h mut dyn ConsoleBackend,
    pub input: &'h mut dyn InputHost,
}

/// One vCPU on the emulated platform.
pub struct PlatformVcpu<'s> {
    core: Core<'s>,
    machine: Machine,
    mtrr_def_type: u64,
}

/// CPUID for a Linux guest on top of the candidate policy:
///
/// * leaf 7.0: no UMIP, LA57 or CET (shadow stacks, IBT): Linux would set
///   CR4.UMIP, CR4.LA57 or CR4.CET, which `Vmcb::check` refuses (the run
///   would end `Invalid` at the next VMRUN), and 5-level paging is also beyond
///   the guest page walk of the MMIO emulation;
/// * leaf 8000_001Fh reads zero (no SME/SEV): where it offers either, Linux
///   reads MSR_AMD64_SEV with a plain RDMSR before it has an IDT, and the #GP
///   would be a triple fault.
pub fn linux_cpuid(leaf: u32, sub: u32, r: &mut [u32; 4]) {
    const UMIP: u32 = 1 << 2;
    const CET_SS: u32 = 1 << 7;
    const LA57: u32 = 1 << 16;
    const CET_IBT: u32 = 1 << 20;
    match leaf {
        7 if sub == 0 => {
            r[2] &= !(UMIP | CET_SS | LA57);
            r[3] &= !CET_IBT;
        }
        0x8000_001F => *r = [0; 4],
        _ => {}
    }
}

/// The machine's device memory as the instruction emulation reaches it; the
/// last write's address, to serve a virtio device that was notified.
struct DeviceMemory<'a> {
    machine: &'a mut Machine,
    now: u64,
    wrote: Option<u64>,
}

impl Bus for DeviceMemory<'_> {
    fn read(&mut self, gpa: u64, size: u8) -> u64 {
        self.machine.mmio_read(gpa, size, self.now)
    }

    fn write(&mut self, gpa: u64, size: u8, value: u64) {
        self.machine.mmio_write(gpa, size, value, self.now);
        self.wrote = Some(gpa);
    }
}

impl<'s> PlatformVcpu<'s> {
    /// A vCPU on `machine`; `serial` receives COM1's output. Of `cfg`,
    /// `serial_base` and `lapic_bus_hz` are not used: the UART is the
    /// machine's COM1 and its local APIC has the bus clock it was built with.
    pub fn new(cfg: VmConfig, machine: Machine, serial: &'s mut [u8]) -> Self {
        Self {
            core: Core::new(cfg, serial),
            machine,
            mtrr_def_type: MTRR_DEF_TYPE_RESET,
        }
    }

    /// Programs the control area of `vmcb` (the same intercepts as
    /// [`Vcpu::prepare`]).
    pub fn prepare(&self, vmcb: &mut Vmcb<'_>) {
        self.core.prepare(vmcb);
    }

    /// Fills an MSR permission map: the candidate policy, plus reads of the
    /// TSC without an exit (TSC_OFFSET applies to RDMSR as to RDTSC, so the
    /// two agree; a write would set the host's TSC and stays intercepted).
    /// Emulated on exit: IA32_APIC_BASE (the machine's local APIC), EFER
    /// (Linux sets SCE and NXE with RDMSR/WRMSR before it has an IDT),
    /// MTRRcap (no ranges, no fixed ranges, no WC) and MTRRdefType (a
    /// register, [`MTRR_DEF_TYPE_RESET`] at first). Every other MSR gets #GP,
    /// which Linux turns into a warning for its plain RDMSR/WRMSR.
    pub fn msr_policy(map: &mut MsrPermissionMap<'_>) {
        Vcpu::msr_policy(map);
        map.allow(MSR_TSC, true, false);
    }

    /// Records that nested mappings were removed: the next VMRUN flushes
    /// this guest's TLB.
    pub fn note_unmap(&mut self, _token: NeedsFlush) {
        self.core.note_unmap();
    }

    /// COM1's output captured so far.
    pub fn serial(&self) -> &[u8] {
        self.core.serial()
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// The machine, e.g. to set it up before a run or give it host input
    /// between runs.
    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    /// Virtual time in nanoseconds.
    pub fn now_ns(&self) -> u64 {
        self.core.now
    }

    /// Runs the guest until a verdict. The disk's size becomes virtio-blk's
    /// capacity first.
    pub fn run<C: SvmCpu + ?Sized, K: Clock + ?Sized>(
        &mut self,
        cpu: &mut C,
        clock: &mut K,
        host: &mut Host<'_>,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Outcome {
        self.machine.blk.set_capacity(host.disk.sectors());
        let mut running = Running { v: self, host };
        shared::run(&mut running, cpu, clock, vmcb, gprs)
    }

    fn rdmsr(&self, vmcb: &Vmcb<'_>, msr: u32) -> Option<u64> {
        match msr {
            lapic::MSR_APIC_BASE => Some(self.machine.lapic.read_msr()),
            MSR_EFER => Some(vmcb.read_u64(save::EFER) & !bits::EFER_SVME),
            MSR_MTRR_CAP => Some(0),
            MSR_MTRR_DEF_TYPE => Some(self.mtrr_def_type),
            _ => None,
        }
    }

    /// False: #GP.
    fn wrmsr(&mut self, vmcb: &mut Vmcb<'_>, msr: u32, v: u64) -> bool {
        match msr {
            lapic::MSR_APIC_BASE => self.machine.lapic.write_msr(v).is_ok(),
            MSR_EFER => {
                let old = vmcb.read_u64(save::EFER);
                let paging = vmcb.read_u64(save::CR0) & bits::CR0_PG != 0;
                // LME may not change while paging is on.
                if v & !EFER_GUEST != 0 || (paging && (v ^ old) & bits::EFER_LME != 0) {
                    return false;
                }
                let lma = old & bits::EFER_LMA;
                vmcb.write_u64(save::EFER, (v & !bits::EFER_LMA) | lma | bits::EFER_SVME);
                true
            }
            MSR_MTRR_DEF_TYPE => {
                // Type (7:0) UC, WC, WT, WP or WB; FE (10) and E (11).
                let ok = v & !0xCFF == 0 && matches!(v & 0xFF, 0 | 1 | 4 | 5 | 6);
                if ok {
                    self.mtrr_def_type = v;
                }
                ok
            }
            _ => false,
        }
    }

    fn msr(&mut self, vmcb: &mut Vmcb<'_>, gprs: &mut Gprs, write: bool) {
        let msr = gprs.rcx as u32;
        let ok = if write {
            self.wrmsr(vmcb, msr, shared::msr_operand(vmcb, gprs))
        } else if let Some(v) = self.rdmsr(vmcb, msr) {
            shared::msr_result(vmcb, gprs, v);
            true
        } else {
            false
        };
        if ok {
            self.core.advance(vmcb, 2);
        } else {
            self.core.msr_fault(vmcb);
        }
    }

    fn io(&mut self, vmcb: &mut Vmcb<'_>, io: IoExit) {
        let now = self.core.now;
        if io.input {
            let v = self.machine.io_in(io.port, io.size, now);
            vmcb.set_rax(shared::in_result(vmcb.rax(), io.size, u64::from(v)));
        } else {
            let v = vmcb.rax() & shared::size_mask(io.size);
            self.machine.io_out(io.port, io.size, v as u32, now);
        }
        shared::skip_to(vmcb, io.next_rip);
    }

    /// Moves what the guest wrote to COM1 into the serial capture.
    fn drain_serial(&mut self) {
        let mut b = [0u8];
        while self.machine.uart.take_tx(&mut b) == 1 {
            self.core.put_serial(b[0]);
        }
    }

    /// A reset or sleep request the guest made.
    fn platform_verdict(&mut self) -> Option<Verdict> {
        if self.machine.take_reset() {
            return Some(Verdict::Reset);
        }
        self.machine.take_sleep().map(|s| {
            if s == acpi_pm::SLEEP_S5 {
                Verdict::PowerOff
            } else {
                Verdict::Sleep { slp_typ: s }
            }
        })
    }
}

/// A run in progress: the vCPU and the host's backends.
struct Running<'a, 's, 'h> {
    v: &'a mut PlatformVcpu<'s>,
    host: &'a mut Host<'h>,
}

impl Running<'_, '_, '_> {
    /// Serves the virtio function in `slot` with its backend.
    fn serve(&mut self, dev: u8) {
        let (m, h, now) = (&mut self.v.machine, &mut *self.host, self.v.core.now);
        match dev {
            slot::BLK => {
                m.service_blk(h.mem, h.disk, now);
            }
            slot::NET => {
                m.service_net(h.mem, h.net, now);
            }
            slot::GPU => {
                m.service_gpu(h.mem, h.display, now);
            }
            slot::CONSOLE => {
                m.service_console(h.mem, h.console, now);
            }
            // The keyboard and the tablet.
            _ => {
                m.service_input(h.mem, now);
            }
        }
    }

    /// A poll point: what the host has for the guest.
    fn poll(&mut self) {
        let now = self.v.core.now;
        self.host.input.poll(&mut self.v.machine, now);
        for dev in [slot::KEYBOARD, slot::NET, slot::CONSOLE] {
            self.serve(dev);
        }
    }

    fn hlt(&mut self, vmcb: &mut Vmcb<'_>) -> Option<Verdict> {
        if vmcb.rflags() & bits::RFLAGS_IF == 0 {
            // No NMI is modeled: nothing can wake it.
            return Some(Verdict::Halted);
        }
        self.poll();
        let now = self.v.core.now;
        let m = &mut self.v.machine;
        if m.pending(now).is_none() {
            // Wait for the next device event; with none, forever.
            match m.next_event(now) {
                Some(t) => self.v.core.now = now.max(t),
                None => return Some(Verdict::Halted),
            }
        }
        self.v.core.advance(vmcb, 1);
        None
    }

    fn npf<C: SvmCpu + ?Sized>(
        &mut self,
        cpu: &mut C,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
        gpa: u64,
        error: u64,
    ) -> Option<Verdict> {
        // Device memory is where no RAM is mapped, reached by a data access;
        // a permission fault on RAM, a fetch or a fault in the guest's page
        // table walk is not an access to emulate.
        if error & (NPF_PRESENT | NPF_FETCH | NPF_TABLE_WALK) != 0 {
            return Some(Verdict::NestedPageFault { gpa, error });
        }
        let mut dm = DeviceMemory {
            machine: &mut self.v.machine,
            now: self.v.core.now,
            wrote: None,
        };
        let r = shared::emulate(&mut self.v.core, cpu, vmcb, gprs, gpa, &mut dm);
        if let Some((dev, off)) = dm.wrote.and_then(|a| self.v.machine.virtio_hit(a)) {
            if off >= virtio::NOTIFY {
                self.serve(dev);
            }
        }
        r
    }
}

impl<'s> Guest<'s> for Running<'_, 's, '_> {
    fn core(&mut self) -> &mut Core<'s> {
        &mut self.v.core
    }

    fn enter(&mut self, vmcb: &mut Vmcb<'_>) {
        let now = self.v.core.now;
        let m = &mut self.v.machine;
        let pending = m.pending(now).is_some();
        shared::deliver(&mut self.v.core, vmcb, pending, || m.acknowledge(now));
    }

    fn handle<C: SvmCpu + ?Sized>(
        &mut self,
        cpu: &mut C,
        vmcb: &mut Vmcb<'_>,
        gprs: &mut Gprs,
    ) -> Option<Verdict> {
        if vmcb.exit_code() == code::INTR {
            // The host's tick: a poll point.
            self.poll();
        }
        let v = match shared::common_exit(&mut self.v.core, cpu, vmcb, gprs, linux_cpuid) {
            Ok(v) => v,
            Err(Own::Msr { write }) => {
                self.v.msr(vmcb, gprs, write);
                None
            }
            Err(Own::Io(io)) => {
                self.v.io(vmcb, io);
                None
            }
            Err(Own::Hlt) => self.hlt(vmcb),
            Err(Own::Npf { gpa, error }) => self.npf(cpu, vmcb, gprs, gpa, error),
        };
        self.v.drain_serial();
        v.or_else(|| self.v.platform_verdict())
    }
}
