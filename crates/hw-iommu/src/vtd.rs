//! Intel VT-d (legacy translation mode) structures.
//!
//! Section numbers refer to the Intel Virtualization Technology for
//! Directed I/O Architecture Specification, rev 3.x/4.x:
//!
//! * §11.4.2 Capability Register (`CAP_REG`, offset 008h): ND 2:0,
//!   RWBF 4, CM 7, SAGAW 12:8, MGAW 21:16, FRO 33:24, PSI 39, NFR 47:40,
//!   MAMV 53:48, DWD 54, DRD 55.
//! * §11.4.3 Extended Capability Register (`ECAP_REG`, offset 010h):
//!   C 0, QI 1, DT 2, IR 3, EIM 4, PT 6, SC 7, IRO 17:8, MHMV 23:20.
//! * §9.1 Root Entry: P 0, CTP 63:12; bits 127:64 reserved.
//! * §9.3 Context Entry: P 0, FPD 1, TT 3:2, SLPTPTR 63:12, AW 66:64,
//!   IGN 70:67, DID 87:72; bits 11:4, 71 and 127:88 reserved.
//! * §9.8 Second-Level Paging Entries (called second-stage in rev 4.x):
//!   R 0, W 1, PS 7 (non-leaf), ADDR 51:12.
//! * §6.5.2.1 Context-cache Invalidate, §6.5.2.3 IOTLB Invalidate,
//!   §6.5.2.8 Invalidation Wait descriptors.
//!
//! Second-level entries are decoded strictly: besides architecturally
//! reserved bits, bits this crate never writes (X, EMT, IPAT, SNP, TM,
//! ignored fields) are rejected as [`Error::ReservedBits`] and large pages
//! as [`Error::Unsupported`].

use crate::inval::{CommandRing, Descriptor, QueueFormat};
use crate::paging::{
    alloc_zeroed, covering_block, frame_ok, Domain, DomainConfig, Entry, PagingLevels, PteFormat,
    ADDR_MASK, MAX_PHYS_BITS,
};
use crate::{Bdf, Error, FrameAlloc, Perms, PhysMem, PAGE_SHIFT};

/// Register offsets from the DRHD register base (§11.4; the
/// IOTLB registers live at `ECAP.IRO`).
pub mod regs {
    /// Version.
    pub const VER: u64 = 0x000;
    /// Capability.
    pub const CAP: u64 = 0x008;
    /// Extended capability.
    pub const ECAP: u64 = 0x010;
    /// Global command.
    pub const GCMD: u64 = 0x018;
    /// Global status (32-bit).
    pub const GSTS: u64 = 0x01C;
    /// Root table address.
    pub const RTADDR: u64 = 0x020;
    /// Context command.
    pub const CCMD: u64 = 0x028;
    /// Fault status (32-bit).
    pub const FSTS: u64 = 0x034;
    /// Invalidation queue head.
    pub const IQH: u64 = 0x080;
    /// Invalidation queue tail.
    pub const IQT: u64 = 0x088;
    /// Invalidation queue address.
    pub const IQA: u64 = 0x090;
    /// Invalidation completion status (32-bit).
    pub const ICS: u64 = 0x09C;
}

/// `CAP_REG` (§11.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability(pub u64);

impl Capability {
    const fn bit(self, n: u32) -> bool {
        self.0 >> n & 1 != 0
    }

    const fn field(self, lo: u32, width: u32) -> u64 {
        (self.0 >> lo) & ((1 << width) - 1)
    }

    /// ND: number of supported domain ids, `2^(4 + 2*ND)`; ND = 7 is reserved.
    pub const fn num_domains(self) -> Result<u32, Error> {
        let nd = self.field(0, 3) as u32;
        if nd == 7 {
            return Err(Error::ReservedBits);
        }
        Ok(1 << (4 + 2 * nd))
    }

    /// RWBF: software must flush the write buffer after table updates.
    #[must_use]
    pub const fn required_write_buffer_flush(self) -> bool {
        self.bit(4)
    }

    /// CM: caching mode; not-present entries may be cached, so a map also
    /// needs an invalidation, and domain id 0 is reserved.
    #[must_use]
    pub const fn caching_mode(self) -> bool {
        self.bit(7)
    }

    /// SAGAW: bit 1 = 39-bit (3-level), bit 2 = 48-bit (4-level),
    /// bit 3 = 57-bit (5-level).
    #[must_use]
    pub const fn sagaw(self) -> u8 {
        self.field(8, 5) as u8
    }

    /// Maximum guest address width, MGAW + 1.
    #[must_use]
    pub const fn max_guest_address_width(self) -> u32 {
        self.field(16, 6) as u32 + 1
    }

    /// Byte offset of the fault recording registers (FRO * 16).
    #[must_use]
    pub const fn fault_recording_offset(self) -> u64 {
        self.field(24, 10) * 16
    }

    /// PSI: page-selective IOTLB invalidation supported.
    #[must_use]
    pub const fn page_selective_invalidation(self) -> bool {
        self.bit(39)
    }

    /// Number of fault recording registers (NFR + 1).
    #[must_use]
    pub const fn fault_recording_count(self) -> u32 {
        self.field(40, 8) as u32 + 1
    }

    /// MAMV: largest address mask for page-selective invalidation.
    #[must_use]
    pub const fn max_address_mask(self) -> u32 {
        self.field(48, 6) as u32
    }

    /// DWD: write draining supported.
    #[must_use]
    pub const fn write_draining(self) -> bool {
        self.bit(54)
    }

    /// DRD: read draining supported.
    #[must_use]
    pub const fn read_draining(self) -> bool {
        self.bit(55)
    }

    /// True when SAGAW reports support for `levels`.
    #[must_use]
    pub const fn supports(self, levels: PagingLevels) -> bool {
        match levels {
            PagingLevels::Three => self.sagaw() & 0b0010 != 0,
            PagingLevels::Four => self.sagaw() & 0b0100 != 0,
        }
    }

    /// Deepest supported table layout this crate implements (4 over 3).
    pub const fn select_levels(self) -> Result<PagingLevels, Error> {
        if self.supports(PagingLevels::Four) {
            Ok(PagingLevels::Four)
        } else if self.supports(PagingLevels::Three) {
            Ok(PagingLevels::Three)
        } else {
            Err(Error::Unsupported)
        }
    }

    /// Domain geometry: selected levels, IOVA width limited by MGAW, and
    /// `host_address_width` from the DMAR table (HAW + 1).
    pub const fn domain_config(self, host_address_width: u32) -> Result<DomainConfig, Error> {
        let levels = match self.select_levels() {
            Ok(l) => l,
            Err(e) => return Err(e),
        };
        let mgaw = self.max_guest_address_width();
        let iova_bits = if mgaw < levels.address_bits() {
            mgaw
        } else {
            levels.address_bits()
        };
        DomainConfig::new(levels, iova_bits, host_address_width)
    }

    /// Checks `did` against ND and the CM reservation of id 0.
    pub const fn validate_domain_id(self, did: u16) -> Result<(), Error> {
        let n = match self.num_domains() {
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if did as u32 >= n || (self.caching_mode() && did == 0) {
            return Err(Error::InvalidDomainId);
        }
        Ok(())
    }
}

/// `ECAP_REG` (§11.4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtCapability(pub u64);

impl ExtCapability {
    const fn bit(self, n: u32) -> bool {
        self.0 >> n & 1 != 0
    }

    /// C: hardware page walks snoop the CPU caches.
    #[must_use]
    pub const fn coherent(self) -> bool {
        self.bit(0)
    }

    /// QI: queued invalidation supported.
    #[must_use]
    pub const fn queued_invalidation(self) -> bool {
        self.bit(1)
    }

    /// DT: device-TLB supported.
    #[must_use]
    pub const fn device_tlb(self) -> bool {
        self.bit(2)
    }

    /// IR: interrupt remapping supported.
    #[must_use]
    pub const fn interrupt_remapping(self) -> bool {
        self.bit(3)
    }

    /// EIM: extended (x2APIC) interrupt mode supported.
    #[must_use]
    pub const fn extended_interrupt_mode(self) -> bool {
        self.bit(4)
    }

    /// PT: pass-through translation type supported.
    #[must_use]
    pub const fn pass_through(self) -> bool {
        self.bit(6)
    }

    /// SC: snoop control supported.
    #[must_use]
    pub const fn snoop_control(self) -> bool {
        self.bit(7)
    }

    /// Byte offset of the IOTLB registers (IRO * 16).
    #[must_use]
    pub const fn iotlb_register_offset(self) -> u64 {
        ((self.0 >> 8) & 0x3ff) * 16
    }

    /// MHMV: maximum handle mask value for interrupt entry invalidation.
    #[must_use]
    pub const fn max_handle_mask(self) -> u32 {
        ((self.0 >> 20) & 0xf) as u32
    }
}

/// Second-level paging entry format (§9.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecondLevel;

/// VT-d second-level domain.
pub type VtdDomain = Domain<SecondLevel>;

const SL_R: u64 = 1 << 0;
const SL_W: u64 = 1 << 1;
const SL_PS: u64 = 1 << 7;

impl PteFormat for SecondLevel {
    fn encode_table(next: u64, _level: u32) -> u64 {
        next | SL_R | SL_W
    }

    fn encode_page(pa: u64, perms: Perms) -> u64 {
        pa | if perms.read { SL_R } else { 0 } | if perms.write { SL_W } else { 0 }
    }

    fn decode(raw: u64, level: u32, phys_bits: u32) -> Result<Entry, Error> {
        let perms = Perms {
            read: raw & SL_R != 0,
            write: raw & SL_W != 0,
        };
        // Hardware treats R = W = 0 as not present; other bits are ignored.
        if perms.is_empty() {
            return Ok(Entry::NotPresent);
        }
        if level > 1 && raw & SL_PS != 0 {
            return Err(Error::Unsupported);
        }
        if raw & !(ADDR_MASK | SL_R | SL_W) != 0 {
            return Err(Error::ReservedBits);
        }
        let pa = raw & ADDR_MASK;
        if pa >> phys_bits != 0 {
            return Err(Error::ReservedBits);
        }
        Ok(if level == 1 {
            Entry::Page { pa, perms }
        } else {
            Entry::Table { pa, perms }
        })
    }
}

/// Context-entry translation type (TT, bits 3:2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranslationType {
    /// 00b: untranslated requests use second-level tables; no device-TLB.
    Untranslated,
    /// 01b: as 00b plus ATS translated/translation requests.
    DeviceTlb,
    /// 10b: untranslated requests pass through (SLPTPTR ignored).
    PassThrough,
}

/// Legacy-mode context entry (§9.3); only present entries are represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextEntry {
    /// FPD: suppress fault recording for this device.
    pub fault_processing_disable: bool,
    /// TT.
    pub translation: TranslationType,
    /// SLPTPTR: root of the second-level tables.
    pub slpt_root: u64,
    /// AW: 001b = 39-bit (3 levels), 010b = 48-bit (4 levels).
    pub levels: PagingLevels,
    /// DID.
    pub domain_id: u16,
}

impl ContextEntry {
    /// Translating entry for `domain`, validated against `caps`.
    pub fn for_domain(domain: &VtdDomain, caps: Capability) -> Result<Self, Error> {
        caps.validate_domain_id(domain.id())?;
        let levels = domain.config().levels();
        if !caps.supports(levels) {
            return Err(Error::Unsupported);
        }
        Ok(Self {
            fault_processing_disable: false,
            translation: TranslationType::Untranslated,
            slpt_root: domain.root(),
            levels,
            domain_id: domain.id(),
        })
    }

    /// Encodes as `[bits 63:0, bits 127:64]` with P = 1.
    pub fn encode(&self, phys_bits: u32) -> Result<[u64; 2], Error> {
        if !frame_ok(self.slpt_root, phys_bits) {
            return Err(Error::Unaligned);
        }
        let tt = match self.translation {
            TranslationType::Untranslated => 0,
            TranslationType::DeviceTlb => 1,
            TranslationType::PassThrough => 2,
        };
        let aw = match self.levels {
            PagingLevels::Three => 1,
            PagingLevels::Four => 2,
        };
        let lo = self.slpt_root | tt << 2 | u64::from(self.fault_processing_disable) << 1 | 1;
        let hi = aw | u64::from(self.domain_id) << 8;
        Ok([lo, hi])
    }

    /// Decodes; `Ok(None)` when P = 0.
    pub fn decode(raw: [u64; 2], phys_bits: u32) -> Result<Option<Self>, Error> {
        let [lo, hi] = raw;
        if lo & 1 == 0 {
            return Ok(None);
        }
        if lo & 0xff0 != 0 || hi & (1 << 7) != 0 || hi >> 24 != 0 {
            return Err(Error::ReservedBits);
        }
        let translation = match (lo >> 2) & 3 {
            0 => TranslationType::Untranslated,
            1 => TranslationType::DeviceTlb,
            2 => TranslationType::PassThrough,
            _ => return Err(Error::ReservedBits),
        };
        let levels = match hi & 7 {
            1 => PagingLevels::Three,
            2 => PagingLevels::Four,
            3 => return Err(Error::Unsupported),
            _ => return Err(Error::ReservedBits),
        };
        let slpt_root = lo & !0xfff;
        if slpt_root >> phys_bits != 0 {
            return Err(Error::ReservedBits);
        }
        Ok(Some(Self {
            fault_processing_disable: lo & 2 != 0,
            translation,
            slpt_root,
            levels,
            domain_id: (hi >> 8) as u16,
        }))
    }
}

/// Decodes a root entry (§9.1): the context-table address if present.
pub fn decode_root_entry(raw: [u64; 2], phys_bits: u32) -> Result<Option<u64>, Error> {
    let [lo, hi] = raw;
    if lo & 1 == 0 {
        return Ok(None);
    }
    let ctp = lo & !0xfff;
    if lo & 0xffe != 0 || hi != 0 || ctp >> phys_bits != 0 {
        return Err(Error::ReservedBits);
    }
    Ok(Some(ctp))
}

/// Legacy-mode root table: 256 root entries (one per bus), each pointing
/// to a context table of 256 entries (one per device/function).
#[derive(Debug)]
pub struct RootTable {
    pa: u64,
    phys_bits: u32,
}

impl RootTable {
    /// Allocates a zeroed root table (every device blocked).
    pub fn new<M: PhysMem, A: FrameAlloc>(
        mem: &mut M,
        alloc: &mut A,
        phys_bits: u32,
    ) -> Result<Self, Error> {
        if phys_bits <= PAGE_SHIFT || phys_bits > MAX_PHYS_BITS {
            return Err(Error::Unsupported);
        }
        let pa = alloc_zeroed(mem, alloc, phys_bits)?;
        Ok(Self { pa, phys_bits })
    }

    /// Value for `RTADDR_REG` (legacy mode: TTM bits 11:10 = 00b).
    #[must_use]
    pub const fn address(&self) -> u64 {
        self.pa
    }

    fn root_slot(&self, bus: u8) -> u64 {
        self.pa + u64::from(bus) * 16
    }

    fn context_table<M: PhysMem>(&self, mem: &M, bus: u8) -> Result<Option<u64>, Error> {
        let slot = self.root_slot(bus);
        decode_root_entry([mem.read_u64(slot), mem.read_u64(slot + 8)], self.phys_bits)
    }

    /// Present context entry of `bdf`, if any.
    pub fn context<M: PhysMem>(&self, mem: &M, bdf: Bdf) -> Result<Option<ContextEntry>, Error> {
        let Some(table) = self.context_table(mem, bdf.bus())? else {
            return Ok(None);
        };
        let slot = table + u64::from(bdf.devfn()) * 16;
        ContextEntry::decode([mem.read_u64(slot), mem.read_u64(slot + 8)], self.phys_bits)
    }

    /// Installs `entry` for `bdf`, allocating the bus's context table on
    /// first use. Fails if the device already has a present entry.
    /// The high quadword is written before the low one that carries P.
    pub fn attach<M: PhysMem, A: FrameAlloc>(
        &mut self,
        mem: &mut M,
        alloc: &mut A,
        bdf: Bdf,
        entry: &ContextEntry,
    ) -> Result<(), Error> {
        let raw = entry.encode(self.phys_bits)?;
        if self.context(mem, bdf)?.is_some() {
            return Err(Error::AlreadyAttached);
        }
        let (table, new_table) = match self.context_table(mem, bdf.bus())? {
            Some(t) => (t, false),
            None => (alloc_zeroed(mem, alloc, self.phys_bits)?, true),
        };
        let slot = table + u64::from(bdf.devfn()) * 16;
        mem.write_u64(slot + 8, raw[1]);
        mem.write_u64(slot, raw[0]);
        if new_table {
            let root = self.root_slot(bdf.bus());
            mem.write_u64(root + 8, 0);
            mem.write_u64(root, table | 1);
        }
        Ok(())
    }

    /// Clears the context entry of `bdf` (P first) and returns it. The
    /// caller must then invalidate the context cache (device-selective)
    /// and the IOTLB of the domain before reusing its structures.
    pub fn detach<M: PhysMem>(&mut self, mem: &mut M, bdf: Bdf) -> Result<ContextEntry, Error> {
        let old = self.context(mem, bdf)?.ok_or(Error::NotAttached)?;
        let table = self.context_table(mem, bdf.bus())?.ok_or(Error::Corrupt)?;
        let slot = table + u64::from(bdf.devfn()) * 16;
        mem.write_u64(slot, 0);
        mem.write_u64(slot + 8, 0);
        Ok(old)
    }
}

/// Queued-invalidation descriptors (§6.5.2), 128-bit form.
pub mod inv {
    use super::{Bdf, Descriptor, Error};
    use crate::PAGE_SHIFT;

    /// Context-cache invalidate descriptor type.
    pub const TYPE_CONTEXT: u64 = 0x1;
    /// IOTLB invalidate descriptor type.
    pub const TYPE_IOTLB: u64 = 0x2;
    /// Invalidation wait descriptor type.
    pub const TYPE_WAIT: u64 = 0x5;

    const GRAN_GLOBAL: u64 = 1 << 4;
    const GRAN_DOMAIN: u64 = 2 << 4;
    const GRAN_DEVICE_OR_PAGE: u64 = 3 << 4;

    /// Draining requested in IOTLB invalidations (only if CAP.DRD/DWD).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Drain {
        /// DR (bit 7).
        pub reads: bool,
        /// DW (bit 6).
        pub writes: bool,
    }

    impl Drain {
        const fn bits(self) -> u64 {
            (self.reads as u64) << 7 | (self.writes as u64) << 6
        }
    }

    /// Global context-cache invalidation (§6.5.2.1, G = 01b).
    #[must_use]
    pub const fn context_global() -> Descriptor {
        [TYPE_CONTEXT | GRAN_GLOBAL, 0]
    }

    /// Domain-selective context-cache invalidation (G = 10b).
    #[must_use]
    pub const fn context_domain(did: u16) -> Descriptor {
        [TYPE_CONTEXT | GRAN_DOMAIN | (did as u64) << 16, 0]
    }

    /// Device-selective context-cache invalidation (G = 11b, FM = 0).
    #[must_use]
    pub const fn context_device(did: u16, source: Bdf) -> Descriptor {
        [
            TYPE_CONTEXT | GRAN_DEVICE_OR_PAGE | (did as u64) << 16 | (source.raw() as u64) << 32,
            0,
        ]
    }

    /// Global IOTLB invalidation (§6.5.2.3, G = 01b).
    #[must_use]
    pub const fn iotlb_global(drain: Drain) -> Descriptor {
        [TYPE_IOTLB | GRAN_GLOBAL | drain.bits(), 0]
    }

    /// Domain-selective IOTLB invalidation (G = 10b).
    #[must_use]
    pub const fn iotlb_domain(did: u16, drain: Drain) -> Descriptor {
        [
            TYPE_IOTLB | GRAN_DOMAIN | drain.bits() | (did as u64) << 16,
            0,
        ]
    }

    /// Page-selective-within-domain IOTLB invalidation (G = 11b) of
    /// `2^address_mask` pages at `addr`, which must be aligned to that
    /// size. IH = 0, so cached paging-structure entries are dropped too.
    pub const fn iotlb_pages(
        did: u16,
        addr: u64,
        address_mask: u32,
        drain: Drain,
    ) -> Result<Descriptor, Error> {
        if address_mask >= 52 {
            return Err(Error::OutOfRange);
        }
        if addr & ((1 << (PAGE_SHIFT + address_mask)) - 1) != 0 {
            return Err(Error::Unaligned);
        }
        Ok([
            TYPE_IOTLB | GRAN_DEVICE_OR_PAGE | drain.bits() | (did as u64) << 16,
            addr | address_mask as u64,
        ])
    }

    /// Invalidation wait (§6.5.2.8) with SW = 1: stores `status` to the
    /// dword at `status_addr` once all earlier descriptors completed.
    pub const fn wait(status_addr: u64, status: u32) -> Result<Descriptor, Error> {
        if !status_addr.is_multiple_of(4) {
            return Err(Error::Unaligned);
        }
        Ok([TYPE_WAIT | 1 << 5 | (status as u64) << 32, status_addr])
    }
}

/// VT-d [`QueueFormat`]: page-selective IOTLB invalidation when
/// `CAP.PSI` and `CAP.MAMV` allow it, domain-selective otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalidator {
    caps: Capability,
}

impl Invalidator {
    /// Encoder for an IOMMU with capability register `caps`.
    #[must_use]
    pub const fn new(caps: Capability) -> Self {
        Self { caps }
    }

    fn drain(&self) -> inv::Drain {
        inv::Drain {
            reads: self.caps.read_draining(),
            writes: self.caps.write_draining(),
        }
    }
}

impl QueueFormat for Invalidator {
    const STATUS_BITS: u32 = 32;

    fn iotlb_range(&self, domain_id: u16, iova: u64, len: u64) -> Result<Descriptor, Error> {
        let (base, order) = covering_block(iova, len)?;
        if self.caps.page_selective_invalidation()
            && order <= self.caps.max_address_mask()
            && order < 52
        {
            inv::iotlb_pages(domain_id, base, order, self.drain())
        } else {
            Ok(inv::iotlb_domain(domain_id, self.drain()))
        }
    }

    fn wait(&self, status_addr: u64, value: u64) -> Result<Descriptor, Error> {
        inv::wait(status_addr, value as u32)
    }
}

/// `IQA_REG` value for `ring` with 128-bit descriptors (DW = 0,
/// QS = log2(entries / 256)).
#[must_use]
pub fn iqa_value(ring: &CommandRing) -> u64 {
    ring.base() | u64::from((ring.entries() / 256).trailing_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_level_decode_is_strict() {
        let pa = 0x1234_5000;
        assert_eq!(SecondLevel::decode(0, 1, 39), Ok(Entry::NotPresent));
        // R = W = 0 is not present whatever the other bits hold.
        assert_eq!(SecondLevel::decode(pa | 0x80, 2, 39), Ok(Entry::NotPresent));
        assert_eq!(
            SecondLevel::decode(pa | 1, 1, 39),
            Ok(Entry::Page {
                pa,
                perms: Perms::R
            })
        );
        assert_eq!(
            SecondLevel::decode(pa | 3 | SL_PS, 2, 39),
            Err(Error::Unsupported)
        );
        assert_eq!(
            SecondLevel::decode(pa | 3 | 1 << 11, 1, 39),
            Err(Error::ReservedBits)
        );
        assert_eq!(
            SecondLevel::decode(pa | 3 | 1 << 62, 1, 39),
            Err(Error::ReservedBits)
        );
        assert_eq!(
            SecondLevel::decode(1 << 39 | 3, 1, 39),
            Err(Error::ReservedBits)
        );
        assert_eq!(
            SecondLevel::decode(1 << 39 | 3, 1, 40),
            Ok(Entry::Page {
                pa: 1 << 39,
                perms: Perms::RW
            })
        );
    }
}
