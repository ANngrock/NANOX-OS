//! AMD-Vi structures.
//!
//! Section numbers refer to the AMD I/O Virtualization Technology (IOMMU)
//! Specification #48882, rev 3.x:
//!
//! * §2.2.2 Device Table Entry Format (256 bits): V 0, TV 1, Mode 11:9,
//!   Host Page Table Root Pointer 51:12, IR 61, IW 62, DomainID 79:64.
//!   Everything else (PPR, GV/GCR3, I/SE/SA, IoCtl, SysMgt, interrupt
//!   remapping in bits 255:128, ...) is rejected on decode because this
//!   crate never writes it.
//! * §2.2.3 I/O Page Tables for Host Translations: PR 0, Next Level 11:9,
//!   Address 51:12, IR 61, IW 62; bits 58:52 reserved. Bits 8:1
//!   (ignored / A / D) are ignored; U (59), FC (60) and bit 63 are rejected
//!   as unmodelled.
//! * §2.4 Commands: COMPLETION_WAIT (01h), INVALIDATE_DEVTAB_ENTRY (02h),
//!   INVALIDATE_IOMMU_PAGES (03h), INVALIDATE_IOMMU_ALL (08h).
//! * §3.4 MMIO registers: Device Table Base (0000h, Size 8:0), Command
//!   Buffer Base (0008h, ComLen 59:56), Extended Feature Register (0030h,
//!   HATS 11:10), Command Buffer Head/Tail (2000h/2008h, bits 18:4).
//!
//! A device table entry with V = 0 lets the device's DMA pass through
//! untranslated. [`DeviceTable::new`] therefore initialises every entry to
//! "blocked" (V = 1, TV = 1, Mode = 0, IR = IW = 0) and
//! [`DeviceTable::detach`] restores that state instead of clearing V.

use crate::inval::{CommandRing, Descriptor, QueueFormat};
use crate::paging::{frame_ok, Domain, DomainConfig, Entry, PagingLevels, PteFormat, ADDR_MASK};
use crate::{Bdf, Error, Perms, PhysMem, PAGE_SHIFT, PAGE_SIZE};

/// MMIO register offsets (§3.4).
pub mod regs {
    /// Device Table Base Address.
    pub const DEV_TABLE_BASE: u64 = 0x0000;
    /// Command Buffer Base Address.
    pub const CMD_BUF_BASE: u64 = 0x0008;
    /// Event Log Base Address.
    pub const EVENT_LOG_BASE: u64 = 0x0010;
    /// IOMMU Control.
    pub const CONTROL: u64 = 0x0018;
    /// Extended Feature Register.
    pub const EXT_FEATURES: u64 = 0x0030;
    /// Command Buffer Head Pointer.
    pub const CMD_BUF_HEAD: u64 = 0x2000;
    /// Command Buffer Tail Pointer.
    pub const CMD_BUF_TAIL: u64 = 0x2008;
    /// Event Log Head Pointer.
    pub const EVENT_LOG_HEAD: u64 = 0x2010;
    /// Event Log Tail Pointer.
    pub const EVENT_LOG_TAIL: u64 = 0x2018;
    /// IOMMU Status.
    pub const STATUS: u64 = 0x2020;
}

/// Extended Feature Register (MMIO 0030h; also mirrored in IVHD types
/// 11h/40h at offset 18h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtendedFeatures(pub u64);

impl ExtendedFeatures {
    const fn bit(self, n: u32) -> bool {
        self.0 >> n & 1 != 0
    }

    /// PreFSup: prefetch pages command supported.
    #[must_use]
    pub const fn prefetch(self) -> bool {
        self.bit(0)
    }

    /// PPRSup: peripheral page requests supported.
    #[must_use]
    pub const fn ppr(self) -> bool {
        self.bit(1)
    }

    /// XTSup: x2APIC supported.
    #[must_use]
    pub const fn x2apic(self) -> bool {
        self.bit(2)
    }

    /// NXSup: no-execute supported.
    #[must_use]
    pub const fn no_execute(self) -> bool {
        self.bit(3)
    }

    /// GTSup: guest translation supported.
    #[must_use]
    pub const fn guest_translation(self) -> bool {
        self.bit(4)
    }

    /// IASup: INVALIDATE_IOMMU_ALL supported.
    #[must_use]
    pub const fn invalidate_all(self) -> bool {
        self.bit(6)
    }

    /// GASup: guest virtual APIC supported.
    #[must_use]
    pub const fn guest_vapic(self) -> bool {
        self.bit(7)
    }

    /// HESup: hardware error registers supported.
    #[must_use]
    pub const fn hardware_error(self) -> bool {
        self.bit(8)
    }

    /// PCSup: performance counters supported.
    #[must_use]
    pub const fn perf_counters(self) -> bool {
        self.bit(9)
    }

    /// HATS: maximum host page-table levels (00b = 4, 01b = 5, 10b = 6;
    /// 11b reserved).
    pub const fn max_host_levels(self) -> Result<u32, Error> {
        match (self.0 >> 10) & 3 {
            3 => Err(Error::ReservedBits),
            hats => Ok(4 + hats as u32),
        }
    }

    /// 4 levels (48-bit IOVA): the deepest layout this crate implements;
    /// HATS guarantees at least 4.
    pub const fn select_levels(self) -> Result<PagingLevels, Error> {
        match self.max_host_levels() {
            Ok(_) => Ok(PagingLevels::Four),
            Err(e) => Err(e),
        }
    }

    /// Domain geometry: selected levels, IOVA width limited by
    /// `va_bits` (IVRS IVinfo VAsize), physical width `phys_bits`.
    pub const fn domain_config(self, va_bits: u32, phys_bits: u32) -> Result<DomainConfig, Error> {
        let levels = match self.select_levels() {
            Ok(l) => l,
            Err(e) => return Err(e),
        };
        let iova_bits = if va_bits < levels.address_bits() {
            va_bits
        } else {
            levels.address_bits()
        };
        DomainConfig::new(levels, iova_bits, phys_bits)
    }
}

/// Host page-table entry format (§2.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostPageTable;

/// AMD-Vi host-translation domain.
pub type AmdDomain = Domain<HostPageTable>;

const PR: u64 = 1 << 0;
const NEXT_LEVEL_SHIFT: u32 = 9;
const NEXT_LEVEL_MASK: u64 = 7 << NEXT_LEVEL_SHIFT;
const IR: u64 = 1 << 61;
const IW: u64 = 1 << 62;
const PTE_IGNORED: u64 = 0x1FE;
const PTE_RESERVED: u64 = 0x07F0_0000_0000_0000;

const fn perm_bits(perms: Perms) -> u64 {
    (if perms.read { IR } else { 0 }) | (if perms.write { IW } else { 0 })
}

impl PteFormat for HostPageTable {
    fn encode_table(next: u64, level: u32) -> u64 {
        next | u64::from(level - 1) << NEXT_LEVEL_SHIFT | IR | IW | PR
    }

    fn encode_page(pa: u64, perms: Perms) -> u64 {
        pa | perm_bits(perms) | PR
    }

    fn decode(raw: u64, level: u32, phys_bits: u32) -> Result<Entry, Error> {
        if raw & PR == 0 {
            return Ok(Entry::NotPresent);
        }
        if raw & PTE_RESERVED != 0 {
            return Err(Error::ReservedBits);
        }
        if raw & !(ADDR_MASK | NEXT_LEVEL_MASK | IR | IW | PR | PTE_IGNORED | PTE_RESERVED) != 0 {
            return Err(Error::ReservedBits);
        }
        let next = ((raw & NEXT_LEVEL_MASK) >> NEXT_LEVEL_SHIFT) as u32;
        let expected = level - 1;
        if next != expected {
            // 0 at level > 1 or 7: large page; other values: level skip.
            return Err(Error::Unsupported);
        }
        let pa = raw & ADDR_MASK;
        if pa >> phys_bits != 0 {
            return Err(Error::ReservedBits);
        }
        let perms = Perms {
            read: raw & IR != 0,
            write: raw & IW != 0,
        };
        Ok(if level == 1 {
            Entry::Page { pa, perms }
        } else {
            Entry::Table { pa, perms }
        })
    }
}

/// Translation part of a device table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DteTranslation {
    /// V = 0: DMA passes through untranslated and unchecked.
    Invalid,
    /// V = 1, TV = 1, Mode = 0: no translation, access allowed by IR/IW
    /// (blocked when both are clear).
    Untranslated(Perms),
    /// V = 1, TV = 1, Mode = levels: host page tables at `root`; IR/IW
    /// combine with the page-table permissions.
    Paged {
        /// Table depth (Mode).
        levels: PagingLevels,
        /// Host Page Table Root Pointer.
        root: u64,
        /// DTE-level IR/IW.
        perms: Perms,
    },
}

/// Device table entry (§2.2.2), restricted to host translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceTableEntry {
    /// Translation fields.
    pub translation: DteTranslation,
    /// DomainID tagging IOTLB entries.
    pub domain_id: u16,
}

impl DeviceTableEntry {
    /// Entry that target-aborts every DMA of the device.
    #[must_use]
    pub const fn blocked() -> Self {
        Self {
            translation: DteTranslation::Untranslated(Perms::NONE),
            domain_id: 0,
        }
    }

    /// Translating entry for `domain` (DTE IR = IW = 1; the page tables
    /// decide).
    #[must_use]
    pub const fn for_domain(domain: &AmdDomain) -> Self {
        Self {
            translation: DteTranslation::Paged {
                levels: domain.config().levels(),
                root: domain.root(),
                perms: Perms::RW,
            },
            domain_id: domain.id(),
        }
    }

    /// Encodes the four quadwords.
    pub fn encode(&self, phys_bits: u32) -> Result<[u64; 4], Error> {
        let q0 = match self.translation {
            DteTranslation::Invalid => 0,
            DteTranslation::Untranslated(perms) => perm_bits(perms) | 0b11,
            DteTranslation::Paged {
                levels,
                root,
                perms,
            } => {
                if !frame_ok(root, phys_bits) {
                    return Err(Error::Unaligned);
                }
                root | u64::from(levels.count()) << 9 | perm_bits(perms) | 0b11
            }
        };
        Ok([q0, u64::from(self.domain_id), 0, 0])
    }

    /// Decodes the four quadwords.
    pub fn decode(raw: [u64; 4], phys_bits: u32) -> Result<Self, Error> {
        let [q0, q1, q2, q3] = raw;
        if q1 >> 16 != 0 || q2 != 0 || q3 != 0 {
            return Err(Error::ReservedBits);
        }
        let domain_id = q1 as u16;
        if q0 & 1 == 0 {
            if q0 != 0 {
                return Err(Error::ReservedBits);
            }
            return Ok(Self {
                translation: DteTranslation::Invalid,
                domain_id,
            });
        }
        if q0 & !(0b11 | 0x7 << 9 | ADDR_MASK | IR | IW) != 0 {
            return Err(Error::ReservedBits);
        }
        if q0 & 0b10 == 0 {
            // V = 1, TV = 0: translation fields invalid; not modelled.
            return Err(Error::Unsupported);
        }
        let perms = Perms {
            read: q0 & IR != 0,
            write: q0 & IW != 0,
        };
        let root = q0 & ADDR_MASK;
        let translation = match (q0 >> 9) & 7 {
            0 => {
                if root != 0 {
                    return Err(Error::ReservedBits);
                }
                DteTranslation::Untranslated(perms)
            }
            7 => return Err(Error::ReservedBits),
            mode => {
                let levels = PagingLevels::from_count(mode as u32).ok_or(Error::Unsupported)?;
                if root >> phys_bits != 0 {
                    return Err(Error::ReservedBits);
                }
                DteTranslation::Paged {
                    levels,
                    root,
                    perms,
                }
            }
        };
        Ok(Self {
            translation,
            domain_id,
        })
    }
}

/// Bytes per device table entry.
pub const DTE_SIZE: u64 = 32;

/// Device table in caller-provided, physically contiguous memory.
#[derive(Debug)]
pub struct DeviceTable {
    base: u64,
    pages: u32,
    phys_bits: u32,
}

impl DeviceTable {
    /// Takes `pages` (1..=512) contiguous pages at `base` and blocks every
    /// entry. Covers DeviceIDs `0..pages * 128`.
    pub fn new<M: PhysMem>(
        mem: &mut M,
        base: u64,
        pages: u32,
        phys_bits: u32,
    ) -> Result<Self, Error> {
        if !(1..=512).contains(&pages) {
            return Err(Error::OutOfRange);
        }
        if !base.is_multiple_of(PAGE_SIZE) {
            return Err(Error::Unaligned);
        }
        let end = base
            .checked_add(u64::from(pages) * PAGE_SIZE)
            .ok_or(Error::Overflow)?;
        if phys_bits <= PAGE_SHIFT
            || phys_bits > crate::paging::MAX_PHYS_BITS
            || end > 1 << phys_bits
        {
            return Err(Error::OutOfRange);
        }
        let table = Self {
            base,
            pages,
            phys_bits,
        };
        let blocked = DeviceTableEntry::blocked().encode(phys_bits)?;
        for devid in 0..table.entries() {
            table.write(mem, devid, blocked);
        }
        Ok(table)
    }

    /// Number of entries.
    #[must_use]
    pub const fn entries(&self) -> u32 {
        self.pages * (PAGE_SIZE / DTE_SIZE) as u32
    }

    /// Device Table Base Address Register value (Size = pages - 1).
    #[must_use]
    pub const fn register_value(&self) -> u64 {
        self.base | (self.pages as u64 - 1)
    }

    fn slot(&self, devid: Bdf) -> Result<u64, Error> {
        if u32::from(devid.raw()) >= self.entries() {
            return Err(Error::OutOfRange);
        }
        Ok(self.base + u64::from(devid.raw()) * DTE_SIZE)
    }

    /// Writes quadwords 3..1 before quadword 0, which holds V/TV/Mode.
    fn write<M: PhysMem>(&self, mem: &mut M, devid: u32, raw: [u64; 4]) {
        let slot = self.base + u64::from(devid) * DTE_SIZE;
        for i in (0..4).rev() {
            mem.write_u64(slot + i * 8, raw[i as usize]);
        }
    }

    /// Current entry of `devid`.
    pub fn entry<M: PhysMem>(&self, mem: &M, devid: Bdf) -> Result<DeviceTableEntry, Error> {
        let slot = self.slot(devid)?;
        let raw = [
            mem.read_u64(slot),
            mem.read_u64(slot + 8),
            mem.read_u64(slot + 16),
            mem.read_u64(slot + 24),
        ];
        DeviceTableEntry::decode(raw, self.phys_bits)
    }

    /// Points `devid` at `domain`. Fails if the device already translates.
    /// The caller must then issue INVALIDATE_DEVTAB_ENTRY.
    pub fn attach<M: PhysMem>(
        &mut self,
        mem: &mut M,
        devid: Bdf,
        domain: &AmdDomain,
    ) -> Result<(), Error> {
        if let DteTranslation::Paged { .. } = self.entry(mem, devid)?.translation {
            return Err(Error::AlreadyAttached);
        }
        let raw = DeviceTableEntry::for_domain(domain).encode(self.phys_bits)?;
        self.write(mem, u32::from(devid.raw()), raw);
        Ok(())
    }

    /// Blocks `devid` again and returns the old entry. Quadword 0 is
    /// written first so the entry never passes DMA through. The caller must
    /// then issue INVALIDATE_DEVTAB_ENTRY and invalidate the old domain's
    /// IOTLB entries.
    pub fn detach<M: PhysMem>(
        &mut self,
        mem: &mut M,
        devid: Bdf,
    ) -> Result<DeviceTableEntry, Error> {
        let slot = self.slot(devid)?;
        let old = self.entry(mem, devid)?;
        if !matches!(old.translation, DteTranslation::Paged { .. }) {
            return Err(Error::NotAttached);
        }
        let blocked = DeviceTableEntry::blocked().encode(self.phys_bits)?;
        for (i, q) in blocked.iter().enumerate() {
            mem.write_u64(slot + i as u64 * 8, *q);
        }
        Ok(old)
    }
}

/// Command encodings (§2.4); quadword 0 holds dwords 0-1 (opcode in bits
/// 63:60), quadword 1 holds dwords 2-3.
pub mod cmd {
    use super::{Descriptor, Error};
    use crate::paging::covering_block;
    use crate::PAGE_SHIFT;

    /// COMPLETION_WAIT opcode.
    pub const COMPLETION_WAIT: u64 = 0x1;
    /// INVALIDATE_DEVTAB_ENTRY opcode.
    pub const INVALIDATE_DEVTAB_ENTRY: u64 = 0x2;
    /// INVALIDATE_IOMMU_PAGES opcode.
    pub const INVALIDATE_IOMMU_PAGES: u64 = 0x3;
    /// INVALIDATE_IOMMU_ALL opcode.
    pub const INVALIDATE_IOMMU_ALL: u64 = 0x8;

    /// Address of INVALIDATE_IOMMU_PAGES with S = 1 meaning "all pages".
    pub const ALL_PAGES_ADDRESS: u64 = 0x7FFF_FFFF_FFFF_F000;

    const S: u64 = 1 << 0;
    const PDE: u64 = 1 << 1;

    /// COMPLETION_WAIT with S = 1: stores `data` at the 8-byte aligned
    /// `store_addr` after every earlier command has completed.
    pub const fn completion_wait(store_addr: u64, data: u64) -> Result<Descriptor, Error> {
        if !store_addr.is_multiple_of(8) {
            return Err(Error::Unaligned);
        }
        if store_addr >> 52 != 0 {
            return Err(Error::OutOfRange);
        }
        Ok([COMPLETION_WAIT << 60 | store_addr | S, data])
    }

    /// INVALIDATE_DEVTAB_ENTRY for `devid`.
    #[must_use]
    pub const fn invalidate_devtab_entry(devid: u16) -> Descriptor {
        [INVALIDATE_DEVTAB_ENTRY << 60 | devid as u64, 0]
    }

    /// INVALIDATE_IOMMU_PAGES (PDE = 1) of `domain_id` covering
    /// `[iova, iova + len)`: S = 0 for one page, otherwise the smallest
    /// aligned power-of-two block, encoded by the lowest clear address bit.
    pub fn invalidate_pages(domain_id: u16, iova: u64, len: u64) -> Result<Descriptor, Error> {
        let (base, order) = covering_block(iova, len)?;
        let q1 = if order == 0 {
            base | PDE
        } else if PAGE_SHIFT + order >= 63 {
            ALL_PAGES_ADDRESS | S | PDE
        } else {
            // Bits 12 .. 12+order-2 set, bit 12+order-1 clear: 2^order pages.
            (base | ((1 << (PAGE_SHIFT + order - 1)) - 1)) & !0xfff | S | PDE
        };
        Ok([INVALIDATE_IOMMU_PAGES << 60 | (domain_id as u64) << 32, q1])
    }

    /// INVALIDATE_IOMMU_PAGES of every page of `domain_id`.
    #[must_use]
    pub const fn invalidate_all_pages(domain_id: u16) -> Descriptor {
        [
            INVALIDATE_IOMMU_PAGES << 60 | (domain_id as u64) << 32,
            ALL_PAGES_ADDRESS | S | PDE,
        ]
    }

    /// INVALIDATE_IOMMU_ALL (requires EFR.IASup).
    #[must_use]
    pub const fn invalidate_all() -> Descriptor {
        [INVALIDATE_IOMMU_ALL << 60, 0]
    }
}

/// AMD-Vi [`QueueFormat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Invalidator;

impl QueueFormat for Invalidator {
    const STATUS_BITS: u32 = 64;

    fn iotlb_range(&self, domain_id: u16, iova: u64, len: u64) -> Result<Descriptor, Error> {
        cmd::invalidate_pages(domain_id, iova, len)
    }

    fn wait(&self, status_addr: u64, value: u64) -> Result<Descriptor, Error> {
        cmd::completion_wait(status_addr, value)
    }
}

/// Command Buffer Base Address Register value (ComLen = log2(entries)).
#[must_use]
pub fn command_buffer_base_value(ring: &CommandRing) -> u64 {
    ring.base() | u64::from(ring.entries().trailing_zeros()) << 56
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_pte_decode_checks_next_level() {
        let pa = 0x7_0000_0000;
        let table = HostPageTable::encode_table(pa, 3);
        assert_eq!(table, pa | 2 << 9 | IR | IW | PR);
        assert_eq!(
            HostPageTable::decode(table, 3, 48),
            Ok(Entry::Table {
                pa,
                perms: Perms::RW
            })
        );
        // The same entry one level lower claims the wrong next level.
        assert_eq!(HostPageTable::decode(table, 2, 48), Err(Error::Unsupported));
        // Next Level 7 at level 1: non-default page size.
        assert_eq!(
            HostPageTable::decode(pa | 7 << 9 | PR, 1, 48),
            Err(Error::Unsupported)
        );
        assert_eq!(
            HostPageTable::decode(pa | 1 << 52 | PR, 1, 48),
            Err(Error::ReservedBits)
        );
        assert_eq!(
            HostPageTable::decode(pa | 1 << 60 | PR, 1, 48),
            Err(Error::ReservedBits)
        );
        // Accessed/dirty and ignored bits 8:1 do not matter.
        assert_eq!(
            HostPageTable::decode(pa | 0x60 | IW | PR, 1, 48),
            Ok(Entry::Page {
                pa,
                perms: Perms::W
            })
        );
        assert_eq!(HostPageTable::decode(pa | IR, 1, 48), Ok(Entry::NotPresent));
    }
}
