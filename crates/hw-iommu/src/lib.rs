//! IOMMU translation structures (Intel VT-d, AMD-Vi) and DMA mapping
//! lifetimes for NANOX M9 (`docs/specs/M9-HARDWARE.md` §3.4).
//!
//! Layers:
//!
//! * [`paging`] — a generic 4 KiB I/O page-table [`Domain`] with a
//!   no-partial-effect `map`/`unmap`, parameterised by the entry format:
//!   [`vtd::SecondLevel`] (VT-d second-level tables) or
//!   [`amd::HostPageTable`] (AMD-Vi host page tables).
//! * [`vtd`] — capability registers, legacy-mode root/context entries,
//!   queued-invalidation descriptors.
//! * [`amd`] — extended feature register, device table entries, commands.
//! * [`inval`] — a command ring shared by both queue formats and a
//!   completion tracker that turns status writes into confirmed tokens.
//! * [`dma`] — the per-buffer lifecycle
//!   `Mapped → InFlight(n) → Quiescing → Unmapped → InvalidationPending →
//!   Invalidated → Freed`; the buffer is only handed back after the IOTLB
//!   invalidation covering it has been confirmed.
//!
//! The crate never touches physical memory itself: page-table memory is
//! read and written through [`PhysMem`] and frames come from
//! [`FrameAlloc`], both implemented by the caller (kernel or test model).
//! The functions assume single-threaded access to the structures they
//! build; the IOMMU hardware is the only concurrent reader.
//!
//! Spec references use Intel VT-d Architecture Specification rev 3.x/4.x
//! section numbers and AMD I/O Virtualization Technology (IOMMU)
//! Specification #48882 rev 3.x section numbers; see the module docs for
//! the exact fields.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod amd;
pub mod dma;
pub mod inval;
pub mod paging;
pub mod vtd;

pub use dma::{DmaMapping, DmaRegion, DmaState};
pub use inval::{
    CommandRing, CompletionTracker, InvalidationQueue, InvalidationToken, QueueFormat,
};
pub use paging::{Domain, DomainConfig, Entry, PagingLevels, PteFormat};

/// log2 of the translation granule.
pub const PAGE_SHIFT: u32 = 12;
/// Translation granule and size of every frame handed out by [`FrameAlloc`].
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

/// Physical memory accessor implemented by the caller.
///
/// Every address passed by this crate is 8-byte aligned and lies inside a
/// frame obtained from [`FrameAlloc`] or a region the caller handed in
/// (root of a device table, command ring, status word).
pub trait PhysMem {
    /// Reads the little-endian `u64` at physical address `pa`.
    fn read_u64(&self, pa: u64) -> u64;
    /// Writes the little-endian `u64` at physical address `pa`.
    fn write_u64(&mut self, pa: u64, value: u64);
}

/// 4 KiB physical frame allocator implemented by the caller.
///
/// Returned frames need not be zeroed; the crate zeroes every frame before
/// it becomes reachable by the IOMMU. A frame that is not 4 KiB aligned or
/// lies above the configured physical address width is handed back through
/// [`FrameAlloc::free_frame`] and reported as [`Error::BadFrame`].
pub trait FrameAlloc {
    /// Allocates one frame and returns its physical address.
    fn alloc_frame(&mut self) -> Option<u64>;
    /// Returns a frame previously obtained from [`FrameAlloc::alloc_frame`].
    fn free_frame(&mut self, pa: u64);
}

/// DMA access permissions of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Perms {
    /// Device may read (DMA from memory).
    pub read: bool,
    /// Device may write (DMA to memory).
    pub write: bool,
}

impl Perms {
    /// No access.
    pub const NONE: Self = Self {
        read: false,
        write: false,
    };
    /// Read-only.
    pub const R: Self = Self {
        read: true,
        write: false,
    };
    /// Write-only.
    pub const W: Self = Self {
        read: false,
        write: true,
    };
    /// Read and write.
    pub const RW: Self = Self {
        read: true,
        write: true,
    };

    /// True when neither read nor write is allowed.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.read && !self.write
    }

    /// Permissions allowed by both `self` and `other`.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self {
            read: self.read && other.read,
            write: self.write && other.write,
        }
    }

    /// True when `access` is allowed.
    #[must_use]
    pub const fn allows(self, access: Access) -> bool {
        match access {
            Access::Read => self.read,
            Access::Write => self.write,
        }
    }
}

/// Kind of DMA access performed by a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Device reads memory.
    Read,
    /// Device writes memory.
    Write,
}

/// PCI requester ID (bus/device/function); the VT-d source-id and the
/// AMD-Vi DeviceID within one PCI segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bdf(u16);

impl Bdf {
    /// Builds a requester ID; `dev` must be < 32 and `func` < 8.
    pub const fn new(bus: u8, dev: u8, func: u8) -> Result<Self, Error> {
        if dev >= 32 || func >= 8 {
            return Err(Error::OutOfRange);
        }
        Ok(Self(
            ((bus as u16) << 8) | ((dev as u16) << 3) | func as u16,
        ))
    }

    /// Wraps a raw 16-bit requester ID.
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Raw 16-bit requester ID (`bus << 8 | dev << 3 | func`).
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Bus number.
    #[must_use]
    pub const fn bus(self) -> u8 {
        (self.0 >> 8) as u8
    }

    /// `dev << 3 | func`, the context-table index.
    #[must_use]
    pub const fn devfn(self) -> u8 {
        self.0 as u8
    }
}

/// Errors reported by this crate. No operation that returns an error has
/// left a partial effect in IOMMU-visible memory unless stated otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Address or length is not 4 KiB aligned (or not aligned as the
    /// structure requires).
    Unaligned,
    /// Zero-length range.
    ZeroLength,
    /// Address arithmetic overflowed `u64`.
    Overflow,
    /// Address beyond the configured IOVA or physical address width, or an
    /// index beyond a table.
    OutOfRange,
    /// Mapping requested with neither read nor write permission.
    NoPermissions,
    /// The page at `iova` is already mapped.
    AlreadyMapped {
        /// First conflicting page.
        iova: u64,
    },
    /// The page at `iova` is not mapped.
    NotMapped {
        /// First unmapped page.
        iova: u64,
    },
    /// The frame allocator is exhausted.
    OutOfFrames,
    /// The frame allocator returned an unusable frame (handed back).
    BadFrame {
        /// The rejected frame.
        pa: u64,
    },
    /// A decoded structure has reserved (or unmodelled) bits set.
    ReservedBits,
    /// A valid encoding this crate does not implement (large pages, 5/6
    /// level tables, level skipping, 57-bit AGAW, ...).
    Unsupported,
    /// Domain id not usable with this IOMMU.
    InvalidDomainId,
    /// The device already has a translating entry.
    AlreadyAttached,
    /// The device has no translating entry.
    NotAttached,
    /// Page table contents contradict what this crate wrote.
    Corrupt,
    /// Operation not allowed in the current DMA mapping state.
    InvalidTransition,
    /// Device DMA is still outstanding on the mapping.
    DmaOutstanding,
    /// Mapping presented to a domain it does not belong to.
    WrongDomain,
    /// The invalidation covering the mapping has not completed yet.
    InvalidationNotComplete,
    /// Token issued by a different invalidation queue.
    ForeignToken,
    /// Token issued before the queue was reset; its wait was discarded.
    StaleToken,
    /// Completion status or head pointer outside the submitted window.
    BogusCompletion,
    /// Not enough free slots in the command ring.
    QueueFull,
    /// Domain still has mapped pages.
    StillMapped,
}
