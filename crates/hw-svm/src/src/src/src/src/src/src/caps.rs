//! SVM availability (APM Vol. 2 §15.4 "Enabling SVM", CPUID Fn8000_000A).

/// Raw inputs the kernel reads with CPUID and RDMSR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuidMsr {
    /// CPUID Fn8000_0000 EAX (highest extended leaf).
    pub max_ext_leaf: u32,
    /// CPUID Fn8000_0001 ECX.
    pub ext_ecx: u32,
    /// CPUID Fn8000_000A EAX, EBX, EDX.
    pub svm_eax: u32,
    pub svm_ebx: u32,
    pub svm_edx: u32,
    /// VM_CR MSR (C001_0114h).
    pub vm_cr: u64,
}

/// Why SVM cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvmUnavailable {
    /// CPUID Fn8000_000A is not implemented.
    NoSvmLeaf,
    /// CPUID Fn8000_0001 ECX.SVM (bit 2) is clear.
    NotSupported,
    /// VM_CR.SVMDIS (bit 4) is set: disabled by firmware; with VM_CR.LOCK
    /// (bit 3) it cannot be re-enabled without the firmware key.
    DisabledByFirmware { locked: bool },
    /// Nested paging (Fn8000_000A EDX bit 0) is required by this VMM.
    NoNestedPaging,
    /// Fewer than two ASIDs: the host uses ASID 0, a guest needs another.
    NoAsids,
}

/// Decoded SVM features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SvmCaps {
    pub revision: u8,
    /// Number of ASIDs (Fn8000_000A EBX).
    pub asids: u32,
    pub nested_paging: bool,
    pub lbr_virt: bool,
    /// NRIP save: the VMCB holds the next RIP on intercepts.
    pub nrips: bool,
    pub vmcb_clean: bool,
    /// TLB_CONTROL values 3 and 7 (flush this guest's ASID).
    pub flush_by_asid: bool,
    pub decode_assists: bool,
    pub pause_filter: bool,
}

impl SvmCaps {
    /// Decides whether this VMM can run. Bit positions: Fn8000_0001
    /// ECX[2] SVM; Fn8000_000A EDX[0] NP, [1] LbrVirt, [3] NRIPS,
    /// [5] VmcbClean, [6] FlushByAsid, [7] DecodeAssists, [10] PauseFilter
    /// (verify); VM_CR[3] LOCK, [4] SVMDIS.
    pub fn decode(raw: CpuidMsr) -> Result<Self, SvmUnavailable> {
        if raw.ext_ecx & (1 << 2) == 0 {
            return Err(SvmUnavailable::NotSupported);
        }
        if raw.max_ext_leaf < 0x8000_000A {
            return Err(SvmUnavailable::NoSvmLeaf);
        }
        if raw.vm_cr & (1 << 4) != 0 {
            return Err(SvmUnavailable::DisabledByFirmware {
                locked: raw.vm_cr & (1 << 3) != 0,
            });
        }
        let edx = raw.svm_edx;
        let caps = Self {
            revision: raw.svm_eax as u8,
            asids: raw.svm_ebx,
            nested_paging: edx & 1 != 0,
            lbr_virt: edx & (1 << 1) != 0,
            nrips: edx & (1 << 3) != 0,
            vmcb_clean: edx & (1 << 5) != 0,
            flush_by_asid: edx & (1 << 6) != 0,
            decode_assists: edx & (1 << 7) != 0,
            pause_filter: edx & (1 << 10) != 0,
        };
        if !caps.nested_paging {
            return Err(SvmUnavailable::NoNestedPaging);
        }
        if caps.asids < 2 {
            return Err(SvmUnavailable::NoAsids);
        }
        Ok(caps)
    }
}
