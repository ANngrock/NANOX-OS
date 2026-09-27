//! Lifecycle of one DMA mapping.
//!
//! ```text
//! map ─► Mapped ⇄ InFlight(n) ─quiesce─► Quiescing{n} ─(n = 0) unmap─► Unmapped
//!        ─submit_invalidation─► InvalidationPending(token)
//!        ─confirm_invalidation─► Invalidated ─release─► Freed
//! ```
//!
//! The buffer (`B`, the caller's ownership handle for the physical pages)
//! is moved into the mapping and only comes back from [`DmaMapping::release`],
//! which is possible only after the IOTLB invalidation covering the
//! mapping has been confirmed through the queue's completion status.
//! Until then the IOMMU may still translate to the pages, so they must
//! not be reused. Every other order of operations is
//! [`Error::InvalidTransition`] and leaves the mapping unchanged.

use crate::inval::{InvalidationQueue, InvalidationToken, QueueFormat};
use crate::paging::{Domain, PteFormat};
use crate::{Error, FrameAlloc, Perms, PhysMem};

/// Physical extent of a DMA buffer; implemented by the caller's owning
/// handle (e.g. a pinned-page object).
pub trait DmaRegion {
    /// 4 KiB aligned physical start.
    fn phys(&self) -> u64;
    /// Length in bytes, a multiple of 4 KiB.
    fn size(&self) -> u64;
}

/// State of a [`DmaMapping`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaState {
    /// Mapped, no DMA outstanding.
    Mapped,
    /// `n > 0` DMA transfers outstanding.
    InFlight(u32),
    /// The device was told to stop; `outstanding` transfers remain.
    Quiescing {
        /// Transfers still outstanding.
        outstanding: u32,
    },
    /// Page-table entries removed; the IOTLB may still hold them.
    Unmapped,
    /// IOTLB invalidation submitted; waiting for its completion.
    InvalidationPending(InvalidationToken),
    /// Invalidation confirmed; the buffer can be released.
    Invalidated,
    /// Buffer handed back to its owner.
    Freed,
}

/// One buffer mapped into one domain.
#[derive(Debug)]
pub struct DmaMapping<B> {
    domain_id: u16,
    domain_root: u64,
    iova: u64,
    len: u64,
    perms: Perms,
    state: DmaState,
    buffer: Option<B>,
}

impl<B: DmaRegion> DmaMapping<B> {
    /// Maps `buffer` at `iova` in `domain`. On failure the buffer is
    /// returned with the error and the domain is unchanged.
    pub fn map<F: PteFormat, M: PhysMem, A: FrameAlloc>(
        domain: &mut Domain<F>,
        mem: &mut M,
        alloc: &mut A,
        iova: u64,
        buffer: B,
        perms: Perms,
    ) -> Result<Self, (B, Error)> {
        let (phys, len) = (buffer.phys(), buffer.size());
        if let Err(e) = domain.map(mem, alloc, iova, phys, len, perms) {
            return Err((buffer, e));
        }
        Ok(Self {
            domain_id: domain.id(),
            domain_root: domain.root(),
            iova,
            len,
            perms,
            state: DmaState::Mapped,
            buffer: Some(buffer),
        })
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> DmaState {
        self.state
    }

    /// IOVA of the mapping.
    #[must_use]
    pub const fn iova(&self) -> u64 {
        self.iova
    }

    /// Length in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Never true: mappings have a non-zero length.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Permissions.
    #[must_use]
    pub const fn perms(&self) -> Perms {
        self.perms
    }

    /// Domain id the mapping lives in.
    #[must_use]
    pub const fn domain_id(&self) -> u16 {
        self.domain_id
    }

    /// Records that a DMA transfer using the mapping was started.
    pub fn begin_dma(&mut self) -> Result<(), Error> {
        self.state = match self.state {
            DmaState::Mapped => DmaState::InFlight(1),
            DmaState::InFlight(n) => DmaState::InFlight(n.checked_add(1).ok_or(Error::Overflow)?),
            _ => return Err(Error::InvalidTransition),
        };
        Ok(())
    }

    /// Records that a DMA transfer using the mapping completed.
    pub fn end_dma(&mut self) -> Result<(), Error> {
        self.state = match self.state {
            DmaState::InFlight(1) => DmaState::Mapped,
            DmaState::InFlight(n) => DmaState::InFlight(n - 1),
            DmaState::Quiescing { outstanding } if outstanding > 0 => DmaState::Quiescing {
                outstanding: outstanding - 1,
            },
            _ => return Err(Error::InvalidTransition),
        };
        Ok(())
    }

    /// Records that the device was told to stop using the mapping; no new
    /// transfer may begin afterwards.
    pub fn quiesce(&mut self) -> Result<(), Error> {
        self.state = match self.state {
            DmaState::Mapped => DmaState::Quiescing { outstanding: 0 },
            DmaState::InFlight(n) => DmaState::Quiescing { outstanding: n },
            _ => return Err(Error::InvalidTransition),
        };
        Ok(())
    }

    /// Removes the page-table entries once no transfer is outstanding.
    pub fn unmap<F: PteFormat, M: PhysMem>(
        &mut self,
        domain: &mut Domain<F>,
        mem: &mut M,
    ) -> Result<(), Error> {
        match self.state {
            DmaState::Quiescing { outstanding: 0 } => {}
            DmaState::Quiescing { .. } => return Err(Error::DmaOutstanding),
            _ => return Err(Error::InvalidTransition),
        }
        if domain.id() != self.domain_id || domain.root() != self.domain_root {
            return Err(Error::WrongDomain);
        }
        domain.unmap(mem, self.iova, self.len)?;
        self.state = DmaState::Unmapped;
        Ok(())
    }

    /// Submits the IOTLB invalidation of the mapping's range and a wait.
    /// The caller then writes the ring tail to the IOMMU. `queue` must
    /// belong to the IOMMU unit(s) the domain is attached to.
    pub fn submit_invalidation<Q: QueueFormat, M: PhysMem>(
        &mut self,
        mem: &mut M,
        queue: &mut InvalidationQueue<Q>,
    ) -> Result<InvalidationToken, Error> {
        if self.state != DmaState::Unmapped {
            return Err(Error::InvalidTransition);
        }
        let token = queue.submit_iotlb(mem, self.domain_id, self.iova, self.len)?;
        self.state = DmaState::InvalidationPending(token);
        Ok(token)
    }

    /// Moves to `Invalidated` if the queue has observed completion of the
    /// mapping's wait (call [`InvalidationQueue::poll`] first). After a
    /// queue reset the token is stale: [`Self::resubmit_after_reset`].
    pub fn confirm_invalidation<Q: QueueFormat>(
        &mut self,
        queue: &InvalidationQueue<Q>,
    ) -> Result<(), Error> {
        let DmaState::InvalidationPending(token) = self.state else {
            return Err(Error::InvalidTransition);
        };
        if !queue.is_complete(token)? {
            return Err(Error::InvalidationNotComplete);
        }
        self.state = DmaState::Invalidated;
        Ok(())
    }

    /// Returns to `Unmapped` when the pending token was discarded by a
    /// queue reset, so the invalidation can be submitted again.
    pub fn resubmit_after_reset<Q: QueueFormat>(
        &mut self,
        queue: &InvalidationQueue<Q>,
    ) -> Result<(), Error> {
        let DmaState::InvalidationPending(token) = self.state else {
            return Err(Error::InvalidTransition);
        };
        match queue.is_complete(token) {
            Err(Error::StaleToken) => {
                self.state = DmaState::Unmapped;
                Ok(())
            }
            Err(e) => Err(e),
            Ok(_) => Err(Error::InvalidTransition),
        }
    }

    /// Hands the buffer back; only after confirmed invalidation.
    pub fn release(&mut self) -> Result<B, Error> {
        if self.state != DmaState::Invalidated {
            return Err(Error::InvalidTransition);
        }
        let buffer = self.buffer.take().ok_or(Error::InvalidTransition)?;
        self.state = DmaState::Freed;
        Ok(buffer)
    }
}
