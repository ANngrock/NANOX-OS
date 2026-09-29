//! #VMEXIT decoding (APM Vol. 2 Appendix C "SVM Intercept Exit Codes",
//! §15.10.2 IOIO EXITINFO1, §15.25.6 nested page fault).

use crate::vmcb::Vmcb;

/// Exit codes used by this VMM.
pub mod code {
    pub const EXCEPTION_BASE: u64 = 0x40;
    pub const INTR: u64 = 0x60;
    pub const NMI: u64 = 0x61;
    pub const SMI: u64 = 0x62;
    pub const INIT: u64 = 0x63;
    /// Virtual interrupt window (V_IRQ became deliverable).
    pub const VINTR: u64 = 0x64;
    pub const CPUID: u64 = 0x72;
    pub const PAUSE: u64 = 0x77;
    pub const HLT: u64 = 0x78;
    pub const IOIO: u64 = 0x7B;
    pub const MSR: u64 = 0x7C;
    pub const SHUTDOWN: u64 = 0x7F;
    pub const VMRUN: u64 = 0x80;
    pub const VMMCALL: u64 = 0x81;
    pub const VMLOAD: u64 = 0x82;
    pub const VMSAVE: u64 = 0x83;
    pub const STGI: u64 = 0x84;
    pub const CLGI: u64 = 0x85;
    pub const SKINIT: u64 = 0x86;
    pub const NPF: u64 = 0x400;
    /// VMRUN refused the guest state.
    pub const INVALID: u64 = u64::MAX;
    /// The same with only the low 32 bits set, as QEMU 9.2 TCG writes it
    /// (svm-probe run 2026-09-28); Linux KVM also compares only the low
    /// half of EXITCODE.
    pub const INVALID_32: u64 = 0xFFFF_FFFF;
}

/// An intercepted IN/OUT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoExit {
    pub port: u16,
    /// IN (true) or OUT.
    pub input: bool,
    /// 1, 2 or 4 bytes.
    pub size: u8,
    /// INS/OUTS.
    pub string: bool,
    pub rep: bool,
    /// RIP of the next instruction (EXITINFO2).
    pub next_rip: u64,
}

/// A decoded #VMEXIT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Intr,
    Nmi,
    Smi,
    Init,
    /// The guest can take an interrupt now (window requested by the VMM).
    Vintr,
    Cpuid,
    /// PAUSE (spin-wait hint): intercepted so busy-waiting guests advance
    /// virtual time.
    Pause,
    Hlt,
    Io(IoExit),
    /// RDMSR/WRMSR; the MSR is in ECX.
    Msr {
        write: bool,
    },
    /// Triple fault or other shutdown condition.
    Shutdown,
    Vmmcall,
    /// VMRUN/VMLOAD/VMSAVE/STGI/CLGI/SKINIT: the guest tried SVM itself.
    SvmInstruction(u64),
    NestedPageFault {
        gpa: u64,
        error: u64,
    },
    Exception {
        vector: u8,
        error: u64,
    },
    Invalid,
    /// Malformed IOIO information or an exit this VMM does not intercept.
    Other(u64),
}

impl Exit {
    pub fn decode(vmcb: &Vmcb<'_>) -> Self {
        let c = vmcb.exit_code();
        let i1 = vmcb.exit_info1();
        let i2 = vmcb.exit_info2();
        match c {
            code::INTR => Exit::Intr,
            code::NMI => Exit::Nmi,
            code::SMI => Exit::Smi,
            code::INIT => Exit::Init,
            code::VINTR => Exit::Vintr,
            code::CPUID => Exit::Cpuid,
            code::PAUSE => Exit::Pause,
            code::HLT => Exit::Hlt,
            code::IOIO => {
                let size = match (i1 >> 4) & 7 {
                    1 => 1,
                    2 => 2,
                    4 => 4,
                    _ => return Exit::Other(c),
                };
                Exit::Io(IoExit {
                    port: (i1 >> 16) as u16,
                    input: i1 & 1 != 0,
                    size,
                    string: i1 & (1 << 2) != 0,
                    rep: i1 & (1 << 3) != 0,
                    next_rip: i2,
                })
            }
            code::MSR => match i1 {
                0 => Exit::Msr { write: false },
                1 => Exit::Msr { write: true },
                _ => Exit::Other(c),
            },
            code::SHUTDOWN => Exit::Shutdown,
            code::VMMCALL => Exit::Vmmcall,
            code::VMRUN | code::VMLOAD | code::VMSAVE | code::STGI | code::CLGI | code::SKINIT => {
                Exit::SvmInstruction(c)
            }
            code::NPF => Exit::NestedPageFault { gpa: i2, error: i1 },
            code::INVALID | code::INVALID_32 => Exit::Invalid,
            c if (code::EXCEPTION_BASE..code::EXCEPTION_BASE + 32).contains(&c) => {
                Exit::Exception {
                    vector: (c - code::EXCEPTION_BASE) as u8,
                    error: i1,
                }
            }
            c => Exit::Other(c),
        }
    }
}
