//! The Virtual Machine Control Block (APM Vol. 2 Appendix B, "Layout of
//! VMCB"): control area at 000h, state save area at 400h. Offsets follow
//! the APM tables (verify); every access is bounds-checked by construction
//! (fixed offsets inside a 4 KiB page).

use crate::{Error, PHYS_ADDRESS_LIMIT};

/// Control area offsets.
pub mod ctl {
    pub const INTERCEPT_CR: usize = 0x000;
    pub const INTERCEPT_DR: usize = 0x004;
    pub const INTERCEPT_EXCEPTIONS: usize = 0x008;
    pub const INTERCEPT_MISC1: usize = 0x00C;
    pub const INTERCEPT_MISC2: usize = 0x010;
    pub const PAUSE_FILTER_THRESHOLD: usize = 0x03C;
    pub const PAUSE_FILTER_COUNT: usize = 0x03E;
    pub const IOPM_BASE: usize = 0x040;
    pub const MSRPM_BASE: usize = 0x048;
    pub const TSC_OFFSET: usize = 0x050;
    pub const ASID: usize = 0x058;
    pub const TLB_CONTROL: usize = 0x05C;
    pub const VINTR: usize = 0x060;
    pub const INTERRUPT_SHADOW: usize = 0x068;
    pub const EXIT_CODE: usize = 0x070;
    pub const EXIT_INFO1: usize = 0x078;
    pub const EXIT_INFO2: usize = 0x080;
    pub const EXIT_INT_INFO: usize = 0x088;
    pub const NP_ENABLE: usize = 0x090;
    pub const EVENT_INJ: usize = 0x0A8;
    pub const N_CR3: usize = 0x0B0;
    pub const LBR_VIRT: usize = 0x0B8;
    pub const CLEAN_BITS: usize = 0x0C0;
    pub const NRIP: usize = 0x0C8;
    pub const INSN_LEN: usize = 0x0D0;
    pub const INSN_BYTES: usize = 0x0D1;
}

/// State save area offsets (absolute, i.e. 400h + table offset).
pub mod save {
    pub const ES: usize = 0x400;
    pub const CS: usize = 0x410;
    pub const SS: usize = 0x420;
    pub const DS: usize = 0x430;
    pub const FS: usize = 0x440;
    pub const GS: usize = 0x450;
    pub const GDTR: usize = 0x460;
    pub const LDTR: usize = 0x470;
    pub const IDTR: usize = 0x480;
    pub const TR: usize = 0x490;
    pub const CPL: usize = 0x4CB;
    pub const EFER: usize = 0x4D0;
    pub const CR4: usize = 0x548;
    pub const CR3: usize = 0x550;
    pub const CR0: usize = 0x558;
    pub const DR7: usize = 0x560;
    pub const DR6: usize = 0x568;
    pub const RFLAGS: usize = 0x570;
    pub const RIP: usize = 0x578;
    pub const RSP: usize = 0x5D8;
    pub const RAX: usize = 0x5F8;
    pub const STAR: usize = 0x600;
    pub const LSTAR: usize = 0x608;
    pub const CSTAR: usize = 0x610;
    pub const SFMASK: usize = 0x618;
    pub const KERNEL_GS_BASE: usize = 0x620;
    pub const CR2: usize = 0x640;
    pub const G_PAT: usize = 0x668;
}

/// Misc intercept vector 3 (offset 00Ch) bits.
pub mod misc1 {
    pub const INTR: u32 = 1 << 0;
    pub const NMI: u32 = 1 << 1;
    pub const SMI: u32 = 1 << 2;
    pub const INIT: u32 = 1 << 3;
    pub const CPUID: u32 = 1 << 18;
    pub const HLT: u32 = 1 << 24;
    pub const IOIO_PROT: u32 = 1 << 27;
    pub const MSR_PROT: u32 = 1 << 28;
    pub const SHUTDOWN: u32 = 1 << 31;
}

/// Misc intercept vector 4 (offset 010h) bits.
pub mod misc2 {
    pub const VMRUN: u32 = 1 << 0;
    pub const VMMCALL: u32 = 1 << 1;
    pub const VMLOAD: u32 = 1 << 2;
    pub const VMSAVE: u32 = 1 << 3;
    pub const STGI: u32 = 1 << 4;
    pub const CLGI: u32 = 1 << 5;
    pub const SKINIT: u32 = 1 << 6;
    pub const XSETBV: u32 = 1 << 13;
}

/// TLB_CONTROL values (APM 15.16.2).
pub mod tlb {
    pub const NOTHING: u8 = 0;
    pub const FLUSH_ALL: u8 = 1;
    pub const FLUSH_ASID: u8 = 3;
}

/// Register bits used by the checks.
pub mod bits {
    pub const CR0_PE: u64 = 1 << 0;
    pub const CR0_ET: u64 = 1 << 4;
    pub const CR0_NE: u64 = 1 << 5;
    pub const CR0_WP: u64 = 1 << 16;
    pub const CR0_NW: u64 = 1 << 29;
    pub const CR0_CD: u64 = 1 << 30;
    pub const CR0_PG: u64 = 1 << 31;
    pub const CR4_PAE: u64 = 1 << 5;
    pub const CR4_PGE: u64 = 1 << 7;
    pub const CR4_OSFXSR: u64 = 1 << 9;
    pub const EFER_SCE: u64 = 1 << 0;
    pub const EFER_LME: u64 = 1 << 8;
    pub const EFER_LMA: u64 = 1 << 10;
    pub const EFER_NXE: u64 = 1 << 11;
    pub const EFER_SVME: u64 = 1 << 12;
    /// EFER bits that may be set (SCE, LME, LMA, NXE, SVME, LMSLE, FFXSR,
    /// TCE); everything else is MBZ.
    pub const EFER_VALID: u64 = 1 | 0xFF00;
    /// CR4 bits this VMM lets a guest start with (VME..PKE subset); the
    /// real CPU accepts more, the check here is deliberately narrower.
    pub const CR4_VALID: u64 = 0x0000_0000_007F_07FF;
    pub const RFLAGS_FIXED1: u64 = 1 << 1;
    pub const RFLAGS_IF: u64 = 1 << 9;
}

/// Segment attribute encoding (VMCB "attrib" field: type, S, DPL, P in
/// bits 7:0; AVL, L, D/B, G in bits 11:8).
pub mod attr {
    pub const CODE_64: u16 = 0x029B; // type 0xB, S, DPL0, P | L
    pub const DATA: u16 = 0x0093; // type 3, S, DPL0, P
    pub const TSS_64: u16 = 0x008B; // busy 64-bit TSS, P
    pub const L: u16 = 1 << 9;
    pub const DB: u16 = 1 << 10;
}

/// A segment register in the state save area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub selector: u16,
    pub attrib: u16,
    pub limit: u32,
    pub base: u64,
}

/// Guest GPRs not held in the VMCB (RAX and RSP are in the save area).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gprs {
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// Which VMRUN consistency check failed (APM 15.5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    EferSvmeClear,
    EferReserved,
    Cr0CdNw,
    Cr0High,
    Cr3Reserved,
    Cr4Reserved,
    Dr6High,
    Dr7High,
    LongModeWithoutPae,
    LongModeWithoutPe,
    CsLongAndDefault32,
    VmrunNotIntercepted,
    AsidZero,
    PermissionMapAddress,
    EventInjection,
    NestedPagingDisabled,
    NestedCr3,
}

/// A VMCB in a caller-provided page. The kernel passes the kernel view of
/// the page whose physical address it gives to VMRUN.
pub struct Vmcb<'a> {
    page: &'a mut [u8; 4096],
}

impl<'a> Vmcb<'a> {
    /// Wraps and zeroes `page`.
    pub fn new(page: &'a mut [u8; 4096]) -> Self {
        page.fill(0);
        Self { page }
    }

    /// Wraps `page` without clearing it (e.g. after VMRUN).
    pub fn wrap(page: &'a mut [u8; 4096]) -> Self {
        Self { page }
    }

    pub fn bytes(&self) -> &[u8; 4096] {
        self.page
    }

    pub fn read_u8(&self, off: usize) -> u8 {
        self.page[off]
    }
    pub fn write_u8(&mut self, off: usize, v: u8) {
        self.page[off] = v;
    }
    pub fn read_u16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.page[off], self.page[off + 1]])
    }
    pub fn write_u16(&mut self, off: usize, v: u16) {
        self.page[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    pub fn read_u32(&self, off: usize) -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.page[off..off + 4]);
        u32::from_le_bytes(b)
    }
    pub fn write_u32(&mut self, off: usize, v: u32) {
        self.page[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    pub fn read_u64(&self, off: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.page[off..off + 8]);
        u64::from_le_bytes(b)
    }
    pub fn write_u64(&mut self, off: usize, v: u64) {
        self.page[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    pub fn segment(&self, off: usize) -> Segment {
        Segment {
            selector: self.read_u16(off),
            attrib: self.read_u16(off + 2),
            limit: self.read_u32(off + 4),
            base: self.read_u64(off + 8),
        }
    }
    pub fn set_segment(&mut self, off: usize, s: Segment) {
        self.write_u16(off, s.selector);
        self.write_u16(off + 2, s.attrib);
        self.write_u32(off + 4, s.limit);
        self.write_u64(off + 8, s.base);
    }

    // Frequently used fields.
    pub fn rip(&self) -> u64 {
        self.read_u64(save::RIP)
    }
    pub fn set_rip(&mut self, v: u64) {
        self.write_u64(save::RIP, v);
    }
    pub fn rax(&self) -> u64 {
        self.read_u64(save::RAX)
    }
    pub fn set_rax(&mut self, v: u64) {
        self.write_u64(save::RAX, v);
    }
    pub fn rflags(&self) -> u64 {
        self.read_u64(save::RFLAGS)
    }
    pub fn exit_code(&self) -> u64 {
        self.read_u64(ctl::EXIT_CODE)
    }
    pub fn exit_info1(&self) -> u64 {
        self.read_u64(ctl::EXIT_INFO1)
    }
    pub fn exit_info2(&self) -> u64 {
        self.read_u64(ctl::EXIT_INFO2)
    }
    pub fn nrip(&self) -> u64 {
        self.read_u64(ctl::NRIP)
    }
    pub fn event_inj(&self) -> u64 {
        self.read_u64(ctl::EVENT_INJ)
    }
    pub fn set_event_inj(&mut self, v: u64) {
        self.write_u64(ctl::EVENT_INJ, v);
    }
    pub fn tlb_control(&self) -> u8 {
        self.read_u8(ctl::TLB_CONTROL)
    }
    pub fn set_tlb_control(&mut self, v: u8) {
        self.write_u8(ctl::TLB_CONTROL, v);
    }

    /// 64-bit guest at CPL 0 with paging on, as a loader leaves a kernel:
    /// flat 64-bit code and data segments, CR3 = `cr3` (guest-physical),
    /// RIP = `entry`, RSP = `stack`, interrupts off.
    pub fn setup_long_mode(&mut self, entry: u64, cr3: u64, stack: u64) {
        use bits::*;
        let code = Segment {
            selector: 0x08,
            attrib: attr::CODE_64,
            limit: 0xFFFF_FFFF,
            base: 0,
        };
        let data = Segment {
            selector: 0x10,
            attrib: attr::DATA,
            limit: 0xFFFF_FFFF,
            base: 0,
        };
        self.set_segment(save::CS, code);
        for off in [save::DS, save::ES, save::SS, save::FS, save::GS] {
            self.set_segment(off, data);
        }
        self.set_segment(
            save::TR,
            Segment {
                selector: 0,
                attrib: attr::TSS_64,
                limit: 0x67,
                base: 0,
            },
        );
        self.set_segment(
            save::GDTR,
            Segment {
                limit: 0xFFFF,
                ..Segment::default()
            },
        );
        self.set_segment(
            save::IDTR,
            Segment {
                limit: 0xFFFF,
                ..Segment::default()
            },
        );
        self.write_u8(save::CPL, 0);
        self.write_u64(save::EFER, EFER_LME | EFER_LMA | EFER_NXE | EFER_SVME);
        self.write_u64(save::CR0, CR0_PE | CR0_ET | CR0_NE | CR0_WP | CR0_PG);
        self.write_u64(save::CR3, cr3);
        self.write_u64(save::CR4, CR4_PAE | CR4_PGE | CR4_OSFXSR);
        self.write_u64(save::DR6, 0xFFFF_0FF0);
        self.write_u64(save::DR7, 0x400);
        self.write_u64(save::RFLAGS, RFLAGS_FIXED1);
        self.write_u64(save::RIP, entry);
        self.write_u64(save::RSP, stack);
        // PAT power-on default.
        self.write_u64(save::G_PAT, 0x0007_0406_0007_0406);
    }

    /// The VMRUN consistency checks (APM 15.5.1) plus this VMM's own
    /// requirements (nested paging on, valid nCR3). Run before every VMRUN.
    pub fn check(&self) -> Result<(), Error> {
        use bits::*;
        let fail = |e| Err(Error::InvalidState(e));
        let efer = self.read_u64(save::EFER);
        let cr0 = self.read_u64(save::CR0);
        let cr3 = self.read_u64(save::CR3);
        let cr4 = self.read_u64(save::CR4);
        if efer & EFER_SVME == 0 {
            return fail(StateError::EferSvmeClear);
        }
        if efer & !EFER_VALID != 0 {
            return fail(StateError::EferReserved);
        }
        if cr0 & CR0_CD == 0 && cr0 & CR0_NW != 0 {
            return fail(StateError::Cr0CdNw);
        }
        if cr0 >> 32 != 0 {
            return fail(StateError::Cr0High);
        }
        if cr3 >> 52 != 0 {
            return fail(StateError::Cr3Reserved);
        }
        if cr4 & !CR4_VALID != 0 {
            return fail(StateError::Cr4Reserved);
        }
        if self.read_u64(save::DR6) >> 32 != 0 {
            return fail(StateError::Dr6High);
        }
        if self.read_u64(save::DR7) >> 32 != 0 {
            return fail(StateError::Dr7High);
        }
        let long = efer & EFER_LME != 0 && cr0 & CR0_PG != 0;
        if long && cr4 & CR4_PAE == 0 {
            return fail(StateError::LongModeWithoutPae);
        }
        if long && cr0 & CR0_PE == 0 {
            return fail(StateError::LongModeWithoutPe);
        }
        let cs = self.segment(save::CS);
        if long && cs.attrib & attr::L != 0 && cs.attrib & attr::DB != 0 {
            return fail(StateError::CsLongAndDefault32);
        }
        if self.read_u32(ctl::INTERCEPT_MISC2) & misc2::VMRUN == 0 {
            return fail(StateError::VmrunNotIntercepted);
        }
        if self.read_u32(ctl::ASID) == 0 {
            return fail(StateError::AsidZero);
        }
        for off in [ctl::IOPM_BASE, ctl::MSRPM_BASE] {
            let pa = self.read_u64(off);
            if !pa.is_multiple_of(crate::PAGE_SIZE)
                || pa
                    .checked_add(3 * crate::PAGE_SIZE)
                    .is_none_or(|e| e > PHYS_ADDRESS_LIMIT)
            {
                return fail(StateError::PermissionMapAddress);
            }
        }
        let inj = self.event_inj();
        if inj & (1 << 31) != 0 {
            let ty = (inj >> 8) & 7;
            // Types: 0 INTR, 2 NMI, 3 exception, 4 software interrupt;
            // 1, 5, 6, 7 are reserved.
            if !matches!(ty, 0 | 2 | 3 | 4) || (ty == 2 && inj & 0xFF != 2) {
                return fail(StateError::EventInjection);
            }
        }
        if self.read_u64(ctl::NP_ENABLE) & 1 == 0 {
            return fail(StateError::NestedPagingDisabled);
        }
        let ncr3 = self.read_u64(ctl::N_CR3);
        if ncr3 == 0 || !ncr3.is_multiple_of(crate::PAGE_SIZE) || ncr3 >= PHYS_ADDRESS_LIMIT {
            return fail(StateError::NestedCr3);
        }
        Ok(())
    }
}
