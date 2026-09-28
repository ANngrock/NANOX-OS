//! TRB rings (xHCI §4.9).
//!
//! [`ProducerRing`] is a single-segment command or transfer ring whose last
//! entry is a Link TRB back to the start with Toggle Cycle set (§4.9.2.2).
//! A multi-TRB TD is published atomically: every TRB but the first is
//! written with the producer cycle state, then the first TRB's cycle bit is
//! flipped last, so the controller never sees a partial TD (it stops at the
//! first TRB whose cycle bit does not match its consumer cycle state).
//!
//! [`EventRing`] is the single-segment consumer side (§4.9.4): an entry is
//! valid while its cycle bit equals the consumer cycle state, which toggles
//! on every wrap.

use crate::trb::{flag, ty, Trb};
use crate::{DmaMemory, Error};

/// Size of a TRB in bytes.
pub const TRB_SIZE: u64 = 16;
/// Maximum TRBs in one TD pushed through [`ProducerRing::push`].
pub const MAX_TD_TRBS: usize = 4;

/// Position on a producer ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingPos {
    /// TRB index (never the Link TRB).
    pub index: u16,
    /// Cycle state of that position.
    pub cycle: bool,
}

/// A TD written by [`ProducerRing::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Td {
    /// Physical addresses of the TRBs.
    pub trbs: [u64; MAX_TD_TRBS],
    /// Number of TRBs.
    pub len: u8,
    /// Position of the first TRB.
    pub start: RingPos,
    /// Position right after the last TRB (next enqueue position).
    pub end: RingPos,
}

impl Td {
    /// True when `pa` is one of the TD's TRBs.
    pub fn contains(&self, pa: u64) -> bool {
        self.trbs[..usize::from(self.len)].contains(&pa)
    }
    /// Address of the last TRB.
    pub fn last(&self) -> u64 {
        self.trbs[usize::from(self.len) - 1]
    }
}

/// Single-segment producer ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProducerRing {
    base: u64,
    size: u16,
    enq: RingPos,
    deq: u16,
}

impl ProducerRing {
    /// Smallest supported ring (entries including the Link TRB).
    pub const MIN_TRBS: u16 = 8;
    /// Largest supported ring (one 64 KiB segment).
    pub const MAX_TRBS: u16 = 4096;

    /// Describes a ring of `size` TRBs (including the Link TRB) at `base`.
    /// The segment must be 64-byte aligned and must not cross a 64 KiB
    /// boundary (xHCI §6.1, Table 6-1).
    pub fn new(base: u64, size: u16) -> Result<Self, Error> {
        if !(Self::MIN_TRBS..=Self::MAX_TRBS).contains(&size) {
            return Err(Error::BadRequest("ring size"));
        }
        let bytes = u64::from(size) * TRB_SIZE;
        let end = base.checked_add(bytes - 1).ok_or(Error::BadDmaAddress)?;
        if !base.is_multiple_of(64) || base >> 16 != end >> 16 {
            return Err(Error::BadDmaAddress);
        }
        Ok(Self {
            base,
            size,
            enq: RingPos {
                index: 0,
                cycle: true,
            },
            deq: 0,
        })
    }

    /// Bytes occupied by a ring of `size` TRBs.
    pub const fn bytes(size: u16) -> usize {
        size as usize * TRB_SIZE as usize
    }

    /// Zeroes the ring, writes the Link TRB and resets the producer state
    /// (PCS = 1). The Link TRB is owned by software until the producer
    /// reaches it.
    pub fn reset<D: DmaMemory + ?Sized>(&mut self, mem: &mut D) {
        mem.fill_zero(self.base, Self::bytes(self.size));
        let link = Trb::link(self.base, true, false).with_cycle(false);
        mem.write(self.pa_of(self.size - 1), &link.to_bytes());
        self.enq = RingPos {
            index: 0,
            cycle: true,
        };
        self.deq = 0;
    }

    /// Physical base address.
    pub const fn base(&self) -> u64 {
        self.base
    }
    /// Entries including the Link TRB.
    pub const fn size(&self) -> u16 {
        self.size
    }
    /// Next enqueue position.
    pub const fn enqueue(&self) -> RingPos {
        self.enq
    }
    /// Software-tracked dequeue index.
    pub const fn dequeue_index(&self) -> u16 {
        self.deq
    }
    /// Physical address of entry `index`.
    pub const fn pa_of(&self, index: u16) -> u64 {
        self.base + index as u64 * TRB_SIZE
    }
    /// Index of a non-link TRB at `pa`.
    pub fn index_of(&self, pa: u64) -> Option<u16> {
        if pa < self.base || !(pa - self.base).is_multiple_of(TRB_SIZE) {
            return None;
        }
        let i = (pa - self.base) / TRB_SIZE;
        if i >= u64::from(self.size - 1) {
            return None;
        }
        Some(i as u16)
    }

    const fn usable(&self) -> u16 {
        self.size - 1
    }

    /// TRBs that can still be pushed (one entry is kept free so that full
    /// and empty differ).
    pub const fn free(&self) -> u16 {
        let u = self.usable();
        let used = (self.enq.index + u - self.deq) % u;
        u - 1 - used
    }

    /// Records that the controller consumed the TRB at `pa` and everything
    /// before it.
    pub fn consumed(&mut self, pa: u64) {
        if let Some(i) = self.index_of(pa) {
            self.deq = (i + 1) % self.usable();
        }
    }

    /// Sets the software dequeue index (after Set TR Dequeue Pointer).
    pub fn set_dequeue(&mut self, index: u16) {
        if index < self.usable() {
            self.deq = index;
        }
    }

    /// Publishes a TD. Fails without writing anything when the ring lacks
    /// space or `trbs` is empty/too long.
    pub fn push<D: DmaMemory + ?Sized>(&mut self, mem: &mut D, trbs: &[Trb]) -> Result<Td, Error> {
        if trbs.is_empty() || trbs.len() > MAX_TD_TRBS {
            return Err(Error::BadRequest("TD length"));
        }
        if usize::from(self.free()) < trbs.len() {
            return Err(Error::RingFull);
        }
        let start = self.enq;
        let mut td = Td {
            trbs: [0; MAX_TD_TRBS],
            len: trbs.len() as u8,
            start,
            end: start,
        };
        for (i, t) in trbs.iter().enumerate() {
            let pa = self.pa_of(self.enq.index);
            // The first TRB keeps the software-owned cycle value until the
            // whole TD is in memory.
            let c = if i == 0 {
                !self.enq.cycle
            } else {
                self.enq.cycle
            };
            mem.write(pa, &t.with_cycle(c).to_bytes());
            td.trbs[i] = pa;
            self.enq.index += 1;
            if self.enq.index == self.size - 1 {
                // Hand the Link TRB over; chain it if the TD continues.
                let chain = t.chain() && i + 1 < trbs.len();
                let link = Trb::link(self.base, true, chain).with_cycle(self.enq.cycle);
                mem.write(self.pa_of(self.size - 1), &link.to_bytes());
                self.enq = RingPos {
                    index: 0,
                    cycle: !self.enq.cycle,
                };
            }
        }
        let first = trbs[0].with_cycle(start.cycle);
        mem.write_u32(td.trbs[0] + 12, first.0[3]);
        td.end = self.enq;
        Ok(td)
    }
}

/// Single-segment event ring plus its one-entry segment table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRing {
    seg: u64,
    erst: u64,
    size: u16,
    deq: u16,
    ccs: bool,
}

impl EventRing {
    /// Smallest segment allowed by the ERST entry (xHCI §6.5).
    pub const MIN_TRBS: u16 = 16;
    /// Largest segment allowed by the ERST entry.
    pub const MAX_TRBS: u16 = 4096;
    /// Size of the one-entry ERST in bytes.
    pub const ERST_BYTES: usize = 16;

    /// Describes a ring segment at `seg` of `size` TRBs and an ERST at
    /// `erst`. Both must be 64-byte aligned; the segment must not cross a
    /// 64 KiB boundary.
    pub fn new(seg: u64, erst: u64, size: u16) -> Result<Self, Error> {
        if !(Self::MIN_TRBS..=Self::MAX_TRBS).contains(&size) {
            return Err(Error::BadRequest("event ring size"));
        }
        let end = seg
            .checked_add(u64::from(size) * TRB_SIZE - 1)
            .ok_or(Error::BadDmaAddress)?;
        if !seg.is_multiple_of(64) || !erst.is_multiple_of(64) || seg >> 16 != end >> 16 {
            return Err(Error::BadDmaAddress);
        }
        Ok(Self {
            seg,
            erst,
            size,
            deq: 0,
            ccs: true,
        })
    }

    /// Zeroes the segment, writes the ERST entry and resets the consumer
    /// state (CCS = 1).
    pub fn reset<D: DmaMemory + ?Sized>(&mut self, mem: &mut D) {
        mem.fill_zero(self.seg, usize::from(self.size) * TRB_SIZE as usize);
        mem.write_u64(self.erst, self.seg);
        mem.write_u32(self.erst + 8, u32::from(self.size));
        mem.write_u32(self.erst + 12, 0);
        self.deq = 0;
        self.ccs = true;
    }

    /// Takes the next event, if the controller has produced one.
    pub fn pop<D: DmaMemory + ?Sized>(&mut self, mem: &mut D) -> Option<Trb> {
        let mut b = [0u8; 16];
        mem.read(self.seg + u64::from(self.deq) * TRB_SIZE, &mut b);
        let t = Trb::from_bytes(&b);
        if t.cycle() != self.ccs {
            return None;
        }
        self.deq += 1;
        if self.deq == self.size {
            self.deq = 0;
            self.ccs = !self.ccs;
        }
        Some(t)
    }

    /// Value for ERDP's pointer field (current dequeue position).
    pub const fn erdp(&self) -> u64 {
        self.seg + self.deq as u64 * TRB_SIZE
    }
    /// ERST base.
    pub const fn erst(&self) -> u64 {
        self.erst
    }
    /// Segment base.
    pub const fn segment(&self) -> u64 {
        self.seg
    }
    /// Segment size in TRBs.
    pub const fn size(&self) -> u16 {
        self.size
    }
}

/// True for TRB types allowed on a command ring.
pub const fn is_command_type(t: u8) -> bool {
    matches!(t, ty::LINK | ty::ENABLE_SLOT..=ty::NOOP_COMMAND)
}

/// True when dword 3 marks a Link TRB with Toggle Cycle.
pub const fn is_toggle_link(t: &Trb) -> bool {
    t.trb_type() == ty::LINK && t.0[3] & flag::TC != 0
}
