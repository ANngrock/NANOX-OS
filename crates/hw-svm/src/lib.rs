//! AMD-V (SVM) virtualization core for NANOX M10: a limited VMM that runs
//! a candidate NANOX build as a guest and reports a verdict, so candidates
//! can be tested inside NANOX instead of QEMU (ROADMAP M10, criterion 4;
//! docs/specs/M10-VMM.md).
//!
//! The target laptop has an AMD Ryzen 7 5800H, hence SVM rather than VMX.
//! The crate is OS-independent and never executes privileged instructions:
//!
//! * [`caps`] — decides from CPUID/MSR values whether SVM is usable.
//! * [`vmcb`] — the 4 KiB VMCB (control area + state save area) with typed
//!   accessors and the VMRUN consistency checks, so an invalid guest state
//!   is an [`Error`] before VMRUN rather than `VMEXIT_INVALID`.
//! * [`perm`] — MSR and I/O permission maps.
//! * [`npt`] — nested page tables (guest-physical → host-physical), built
//!   in caller frames, without partial effect on error.
//! * [`exit`] — #VMEXIT decoding.
//! * [`vmm`] — a vCPU loop: CPUID/MSR policy, a 16550 transmit path, the
//!   `isa-debug-exit` port used by the M0 harness, the local APIC and PIT
//!   channel 2 from `vmm-devices` on a virtual clock, budgets and a
//!   verdict.
//! * [`guest`] — guest page walk and instruction fetch for MMIO emulation.
//!
//! The kernel implements [`SvmCpu::vmrun`] (VMLOAD/VMRUN/VMSAVE and the
//! GPR save/restore in assembly); tests implement it with a scripted CPU.
//! References are to the AMD64 Architecture Programmer's Manual Vol. 2
//! (24593), chapter 15 and appendix B; items marked "(verify)" were written
//! from memory and must be checked against the PDF.

#![no_std]
#![forbid(unsafe_code)]

pub mod caps;
pub mod exit;
pub mod guest;
pub mod npt;
pub mod perm;
pub mod vmcb;
pub mod vmm;

pub use caps::{SvmCaps, SvmUnavailable};
pub use exit::{Exit, IoExit};
pub use npt::{Npt, NptPerms};
pub use vmcb::{Gprs, Vmcb};
pub use vmm::{Vcpu, Verdict, VmConfig};

/// 4 KiB.
pub const PAGE_SIZE: u64 = 4096;
/// Physical addresses must end at or below this (52-bit), as in the other
/// NANOX hardware crates.
pub const PHYS_ADDRESS_LIMIT: u64 = 1 << 52;

/// Errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A frame or address is misaligned or beyond [`PHYS_ADDRESS_LIMIT`].
    BadAddress,
    /// A range is empty, overflows or exceeds the guest address space.
    BadRange,
    /// The range is already mapped (no silent remap).
    AlreadyMapped,
    /// The range is not mapped.
    NotMapped,
    /// [`FrameAlloc`] returned `None`.
    OutOfFrames,
    /// A page table entry has bits this crate never writes.
    Corrupt,
    /// Guest state violates a VMRUN consistency check (APM 15.5.1).
    InvalidState(vmcb::StateError),
    /// A buffer handed in by the caller has the wrong size.
    BufferSize,
}

/// Physical memory access for the page tables the crate builds.
pub trait PhysMem {
    fn read_u64(&mut self, pa: u64) -> u64;
    fn write_u64(&mut self, pa: u64, value: u64);
}

/// 4 KiB frames for page tables.
pub trait FrameAlloc {
    /// A 4 KiB-aligned frame, contents unspecified (the crate zeroes it).
    fn alloc_frame(&mut self) -> Option<u64>;
    fn free_frame(&mut self, pa: u64);
}

/// The privileged part, implemented by the kernel: enter the guest
/// described by `vmcb` (VMRUN with the VMCB's physical address) with the
/// GPRs not held in the VMCB, and return on #VMEXIT with the VMCB and
/// `gprs` updated by the processor.
pub trait SvmCpu {
    fn vmrun(&mut self, vmcb: &mut Vmcb<'_>, gprs: &mut Gprs);
    /// CPUID of the host for `leaf`/`subleaf` as (EAX, EBX, ECX, EDX); the
    /// VMM filters it before the guest sees it.
    fn host_cpuid(&mut self, leaf: u32, subleaf: u32) -> [u32; 4];
    /// Reads guest-physical memory (resolved through the nested tables) so
    /// the VMM can fetch an instruction it emulates; false if `gpa` is not
    /// guest RAM. The default has no access: MMIO emulation then ends the
    /// run with `Verdict::MmioUnsupported` unless the processor saved the
    /// instruction bytes (decode assists).
    fn read_guest_phys(&mut self, _gpa: u64, _out: &mut [u8]) -> bool {
        false
    }
}

/// Monotonic time in microseconds for VMM budgets.
pub trait Clock {
    fn now_us(&mut self) -> u64;
}
