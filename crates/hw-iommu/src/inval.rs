//! Invalidation queue: command ring, completion tracking, tokens.
//!
//! Both IOMMUs take 128-bit commands from a ring in memory: VT-d's
//! invalidation queue (VT-d §6.5.2, registers IQA/IQH/IQT) and AMD-Vi's
//! command buffer (#48882 §2.4, registers at MMIO 0008h/2000h/2008h). In
//! both, a wait command (VT-d Invalidation Wait with SW=1, AMD-Vi
//! COMPLETION_WAIT with S=1) stores a value to memory once every earlier
//! command has completed.
//!
//! [`InvalidationQueue::submit`] appends the caller's commands followed by
//! a wait that stores the next sequence number, in one step: either all of
//! them are written and a token is issued, or nothing is. A token is
//! complete once the stored value has reached its sequence number; because
//! waits complete in order, a later value implies every earlier one.

use crate::{Error, PhysMem, PAGE_SIZE};

/// One 128-bit command/descriptor as two little-endian quadwords.
pub type Descriptor = [u64; 2];

/// Bytes per command in both rings.
pub const DESCRIPTOR_SIZE: u64 = 16;

/// Format-specific pieces of an invalidation queue.
pub trait QueueFormat {
    /// Width of the value stored by the wait command (VT-d 32, AMD-Vi 64).
    const STATUS_BITS: u32;
    /// Command invalidating IOTLB entries of `domain_id` covering
    /// `[iova, iova + len)` (it may invalidate more).
    fn iotlb_range(&self, domain_id: u16, iova: u64, len: u64) -> Result<Descriptor, Error>;
    /// Wait command storing `value` at `status_addr` after completion of
    /// all earlier commands.
    fn wait(&self, status_addr: u64, value: u64) -> Result<Descriptor, Error>;
}

/// Ring of 16-byte commands in caller-provided contiguous memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandRing {
    base: u64,
    entries: u32,
    head: u32,
    tail: u32,
}

impl CommandRing {
    /// `base` must be 4 KiB aligned; `entries` a power of two in
    /// 256..=32768 (VT-d IQA.QS 0..=7, AMD-Vi ComLen 8..=15).
    pub fn new(base: u64, entries: u32) -> Result<Self, Error> {
        if !entries.is_power_of_two() || !(256..=32768).contains(&entries) {
            return Err(Error::Unsupported);
        }
        if !base.is_multiple_of(PAGE_SIZE) {
            return Err(Error::Unaligned);
        }
        base.checked_add(u64::from(entries) * DESCRIPTOR_SIZE)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            base,
            entries,
            head: 0,
            tail: 0,
        })
    }

    /// Physical base address.
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// Ring size in commands.
    #[must_use]
    pub const fn entries(&self) -> u32 {
        self.entries
    }

    /// Tail as a byte offset: the value for VT-d IQT / AMD-Vi Command
    /// Buffer Tail Pointer (both hold the index in bits 18:4).
    #[must_use]
    pub const fn tail_offset(&self) -> u64 {
        self.tail as u64 * DESCRIPTOR_SIZE
    }

    /// Last head known to software, as a byte offset.
    #[must_use]
    pub const fn head_offset(&self) -> u64 {
        self.head as u64 * DESCRIPTOR_SIZE
    }

    /// Commands that can still be written without overrunning the head.
    #[must_use]
    pub const fn free_slots(&self) -> u32 {
        self.entries - 1 - (self.tail.wrapping_sub(self.head) & (self.entries - 1))
    }

    /// Writes all `descs` or none of them.
    pub fn push_all<M: PhysMem>(&mut self, mem: &mut M, descs: &[Descriptor]) -> Result<(), Error> {
        if descs.len() > self.free_slots() as usize {
            return Err(Error::QueueFull);
        }
        for d in descs {
            let slot = self.base + u64::from(self.tail) * DESCRIPTOR_SIZE;
            mem.write_u64(slot, d[0]);
            mem.write_u64(slot + 8, d[1]);
            self.tail = (self.tail + 1) & (self.entries - 1);
        }
        Ok(())
    }

    /// Records the hardware head pointer (byte offset read from VT-d IQH
    /// or the AMD-Vi Command Buffer Head Pointer). It must lie between the
    /// previous head and the tail.
    pub fn update_head(&mut self, head_offset: u64) -> Result<(), Error> {
        if !head_offset.is_multiple_of(DESCRIPTOR_SIZE) {
            return Err(Error::Unaligned);
        }
        let index = head_offset / DESCRIPTOR_SIZE;
        if index >= u64::from(self.entries) {
            return Err(Error::OutOfRange);
        }
        let index = index as u32;
        let mask = self.entries - 1;
        let advanced = index.wrapping_sub(self.head) & mask;
        let pending = self.tail.wrapping_sub(self.head) & mask;
        if advanced > pending {
            return Err(Error::BogusCompletion);
        }
        self.head = index;
        Ok(())
    }

    /// Empties the ring (after the hardware queue was re-initialised).
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

/// Proof-of-submission for one wait command; see [`InvalidationQueue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InvalidationToken {
    queue: u32,
    epoch: u32,
    seq: u64,
}

impl InvalidationToken {
    /// Sequence number stored by the wait command.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Epoch of the issuing queue.
    #[must_use]
    pub const fn epoch(&self) -> u32 {
        self.epoch
    }
}

/// Sequence bookkeeping: issued and completed wait values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionTracker {
    id: u32,
    epoch: u32,
    issued: u64,
    completed: u64,
}

impl CompletionTracker {
    /// A tracker for the queue with identifier `id`.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self {
            id,
            epoch: 0,
            issued: 0,
            completed: 0,
        }
    }

    /// Last issued sequence number.
    #[must_use]
    pub const fn issued(&self) -> u64 {
        self.issued
    }

    /// Last completed sequence number.
    #[must_use]
    pub const fn completed(&self) -> u64 {
        self.completed
    }

    /// Current epoch.
    #[must_use]
    pub const fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Sequence number the next token will carry.
    fn next_seq(&self, status_bits: u32) -> Result<u64, Error> {
        let next = self.issued.checked_add(1).ok_or(Error::Overflow)?;
        // A 32-bit status word must identify the outstanding window
        // unambiguously.
        if status_bits < 64 && next - self.completed >= 1 << (status_bits - 1) {
            return Err(Error::QueueFull);
        }
        Ok(next)
    }

    /// Accounts a value read from the status word.
    pub fn observe(&mut self, value: u64, status_bits: u32) -> Result<(), Error> {
        let outstanding = self.issued - self.completed;
        let delta = if status_bits >= 64 {
            if value < self.completed || value > self.issued {
                return Err(Error::BogusCompletion);
            }
            value - self.completed
        } else {
            let mask = (1u64 << status_bits) - 1;
            (value.wrapping_sub(self.completed)) & mask
        };
        if delta > outstanding {
            return Err(Error::BogusCompletion);
        }
        self.completed += delta;
        Ok(())
    }

    /// True once the wait carrying `token` has stored its value.
    pub fn is_complete(&self, token: InvalidationToken) -> Result<bool, Error> {
        if token.queue != self.id {
            return Err(Error::ForeignToken);
        }
        if token.epoch != self.epoch {
            return Err(Error::StaleToken);
        }
        if token.seq > self.issued {
            return Err(Error::ForeignToken);
        }
        Ok(token.seq <= self.completed)
    }
}

/// An invalidation queue of one IOMMU unit.
#[derive(Debug)]
pub struct InvalidationQueue<Q> {
    ring: CommandRing,
    tracker: CompletionTracker,
    status_addr: u64,
    format: Q,
}

impl<Q: QueueFormat> InvalidationQueue<Q> {
    /// Takes over `ring` and the 8-byte status word at `status_addr`
    /// (zeroed here). `id` must be unique per IOMMU unit.
    pub fn new<M: PhysMem>(
        mem: &mut M,
        ring: CommandRing,
        status_addr: u64,
        id: u32,
        format: Q,
    ) -> Result<Self, Error> {
        if !status_addr.is_multiple_of(8) {
            return Err(Error::Unaligned);
        }
        mem.write_u64(status_addr, 0);
        Ok(Self {
            ring,
            tracker: CompletionTracker::new(id),
            status_addr,
            format,
        })
    }

    /// The ring (tail/head offsets, base for register programming).
    #[must_use]
    pub const fn ring(&self) -> &CommandRing {
        &self.ring
    }

    /// Sequence bookkeeping.
    #[must_use]
    pub const fn tracker(&self) -> &CompletionTracker {
        &self.tracker
    }

    /// Address of the status word.
    #[must_use]
    pub const fn status_addr(&self) -> u64 {
        self.status_addr
    }

    /// Format-specific encoder.
    #[must_use]
    pub const fn format(&self) -> &Q {
        &self.format
    }

    /// Appends `commands` and a wait; returns the token of that wait. The
    /// caller then writes [`CommandRing::tail_offset`] to the tail
    /// register. Nothing is written on error.
    pub fn submit<M: PhysMem>(
        &mut self,
        mem: &mut M,
        commands: &[Descriptor],
    ) -> Result<InvalidationToken, Error> {
        if commands.len() >= self.ring.free_slots() as usize {
            return Err(Error::QueueFull);
        }
        let seq = self.tracker.next_seq(Q::STATUS_BITS)?;
        let wait = self.format.wait(self.status_addr, seq)?;
        self.ring.push_all(mem, commands)?;
        self.ring.push_all(mem, &[wait])?;
        self.tracker.issued = seq;
        Ok(InvalidationToken {
            queue: self.tracker.id,
            epoch: self.tracker.epoch,
            seq,
        })
    }

    /// Submits an IOTLB invalidation of `[iova, iova + len)` in
    /// `domain_id` followed by a wait.
    pub fn submit_iotlb<M: PhysMem>(
        &mut self,
        mem: &mut M,
        domain_id: u16,
        iova: u64,
        len: u64,
    ) -> Result<InvalidationToken, Error> {
        let cmd = self.format.iotlb_range(domain_id, iova, len)?;
        self.submit(mem, &[cmd])
    }

    /// Reads the status word and advances the completed sequence.
    pub fn poll<M: PhysMem>(&mut self, mem: &M) -> Result<u64, Error> {
        let value = mem.read_u64(self.status_addr);
        self.tracker.observe(value, Q::STATUS_BITS)?;
        Ok(self.tracker.completed)
    }

    /// Records the hardware head pointer; see [`CommandRing::update_head`].
    pub fn update_head(&mut self, head_offset: u64) -> Result<(), Error> {
        self.ring.update_head(head_offset)
    }

    /// True once the wait of `token` has completed.
    pub fn is_complete(&self, token: InvalidationToken) -> Result<bool, Error> {
        self.tracker.is_complete(token)
    }

    /// Discards the ring after the hardware queue was re-initialised.
    /// Waits not yet completed are lost, so every earlier token becomes
    /// [`Error::StaleToken`]; mappings holding one must be invalidated
    /// again.
    pub fn reset<M: PhysMem>(&mut self, mem: &mut M) {
        self.ring.reset();
        self.tracker.epoch = self.tracker.epoch.wrapping_add(1);
        self.tracker.completed = self.tracker.issued;
        let mask = if Q::STATUS_BITS >= 64 {
            u64::MAX
        } else {
            (1u64 << Q::STATUS_BITS) - 1
        };
        mem.write_u64(self.status_addr, self.tracker.issued & mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_rejects_values_outside_window() {
        let mut t = CompletionTracker::new(7);
        t.issued = 3;
        assert_eq!(t.observe(4, 64), Err(Error::BogusCompletion));
        assert_eq!(t.observe(2, 64), Ok(()));
        assert_eq!(t.observe(1, 64), Err(Error::BogusCompletion));
        assert_eq!(t.completed(), 2);
        // 32-bit status word: wrap-around is resolved within the window.
        let mut w = CompletionTracker::new(1);
        w.issued = 0x1_0000_0002;
        w.completed = 0xFFFF_FFFE;
        assert_eq!(w.observe(0x0000_0001, 32), Ok(()));
        assert_eq!(w.completed(), 0x1_0000_0001);
        assert_eq!(w.observe(0x0000_0005, 32), Err(Error::BogusCompletion));
    }

    #[test]
    fn ring_head_must_stay_behind_tail() {
        let mut r = CommandRing::new(0x10_0000, 256).unwrap();
        assert_eq!(r.free_slots(), 255);
        r.tail = 3;
        assert_eq!(r.update_head(4 * 16), Err(Error::BogusCompletion));
        assert_eq!(r.update_head(8), Err(Error::Unaligned));
        assert_eq!(r.update_head(256 * 16), Err(Error::OutOfRange));
        assert_eq!(r.update_head(2 * 16), Ok(()));
        assert_eq!(r.free_slots(), 254);
        assert!(CommandRing::new(0x10_0000, 128).is_err());
        assert!(CommandRing::new(0x10_0800, 256).is_err());
    }
}
