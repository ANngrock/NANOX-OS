//! Submission/completion rings and command identifier tracking
//! (NVMe Base 2.0 §3.3.1 "Memory-based Transport Queue Model" (?)).
//!
//! * A queue of `n` entries holds at most `n - 1` commands: full when
//!   `tail + 1 == head (mod n)`.
//! * The host learns how far the controller consumed a submission queue
//!   only from the SQ head field of completion entries.
//! * Completion entries are new when their phase tag equals the expected
//!   phase; the expected phase starts at 1 and flips on every wrap. The
//!   memory must be zeroed before the controller starts posting.
//!
//! Command identifiers encode a slot index (bits 6:0) and a per-slot
//! sequence number (bits 15:7). A completion for a free slot whose last
//! sequence matches is a duplicate; any other mismatch is unknown. A stale
//! duplicate therefore cannot complete a newer command that reused the
//! slot.

use crate::command::{Command, CompletionEntry, CQE_SIZE, SQE_SIZE};
use crate::DmaMemory;

/// Why a completion entry was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionError {
    /// CID does not belong to any command issued on this queue.
    UnknownCid(u16),
    /// CID of a command that already completed.
    DuplicateCid(u16),
    /// SQ identifier differs from the queue the completion queue serves.
    WrongQueue {
        /// Expected SQ identifier.
        expected: u16,
        /// SQ identifier in the entry.
        got: u16,
    },
    /// SQ head outside the range of submitted entries.
    SqHead(u16),
}

/// Host side of a submission queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmissionRing {
    base: u64,
    entries: u32,
    head: u32,
    tail: u32,
}

impl SubmissionRing {
    /// Ring of `entries` (>= 2) 64-byte entries at `base`.
    #[must_use]
    pub const fn new(base: u64, entries: u32) -> Self {
        Self {
            base,
            entries,
            head: 0,
            tail: 0,
        }
    }

    /// Number of entries.
    #[must_use]
    pub const fn entries(&self) -> u32 {
        self.entries
    }

    /// Host tail (next entry to write).
    #[must_use]
    pub const fn tail(&self) -> u32 {
        self.tail
    }

    /// Last SQ head reported by the controller.
    #[must_use]
    pub const fn head(&self) -> u32 {
        self.head
    }

    /// Entries submitted and not yet reported consumed.
    #[must_use]
    pub const fn in_use(&self) -> u32 {
        (self.tail + self.entries - self.head) % self.entries
    }

    /// Entries that can still be written.
    #[must_use]
    pub const fn free(&self) -> u32 {
        self.entries - 1 - self.in_use()
    }

    /// Writes `cmd` at the tail and advances the tail; returns the new
    /// tail for the doorbell, or `None` (nothing written) when full. The
    /// caller rings the doorbell after this returns.
    pub fn push<M: DmaMemory + ?Sized>(&mut self, mem: &mut M, cmd: &Command) -> Option<u32> {
        if self.free() == 0 {
            return None;
        }
        mem.write(self.base + u64::from(self.tail) * SQE_SIZE, &cmd.to_bytes());
        self.tail = (self.tail + 1) % self.entries;
        Some(self.tail)
    }

    /// Checks an SQ head from a completion entry: it must lie between the
    /// current head and the tail (inclusive).
    pub const fn check_head(&self, head: u16) -> Result<(), CompletionError> {
        let h = head as u32;
        if h >= self.entries {
            return Err(CompletionError::SqHead(head));
        }
        let advance = (h + self.entries - self.head) % self.entries;
        if advance > self.in_use() {
            return Err(CompletionError::SqHead(head));
        }
        Ok(())
    }

    /// Records an SQ head accepted by [`Self::check_head`].
    pub fn set_head(&mut self, head: u16) {
        self.head = u32::from(head);
    }

    /// Empties the ring (controller reset or queue re-creation).
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

/// Host side of a completion queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionRing {
    base: u64,
    entries: u32,
    head: u32,
    phase: bool,
}

impl CompletionRing {
    /// Ring of `entries` (>= 2) 16-byte entries at `base`.
    #[must_use]
    pub const fn new(base: u64, entries: u32) -> Self {
        Self {
            base,
            entries,
            head: 0,
            phase: true,
        }
    }

    /// Host head (next entry to read).
    #[must_use]
    pub const fn head(&self) -> u32 {
        self.head
    }

    /// Expected phase tag of the next new entry.
    #[must_use]
    pub const fn phase(&self) -> bool {
        self.phase
    }

    /// Returns the entry at the head if the controller has posted it.
    /// Dword 3 (with the phase tag) is read first; the rest only after the
    /// phase matched.
    pub fn peek<M: DmaMemory + ?Sized>(&self, mem: &mut M) -> Option<CompletionEntry> {
        let at = self.base + u64::from(self.head) * CQE_SIZE;
        let mut dw3 = [0u8; 4];
        mem.read(at + 12, &mut dw3);
        if (u32::from_le_bytes(dw3) & (1 << 16) != 0) != self.phase {
            return None;
        }
        let mut raw = [0u8; 16];
        mem.read(at, &mut raw);
        let cqe = CompletionEntry::from_bytes(&raw);
        // A torn read would show a different phase in the second copy.
        (cqe.phase == self.phase).then_some(cqe)
    }

    /// Consumes the entry at the head.
    pub fn advance(&mut self) {
        self.head += 1;
        if self.head == self.entries {
            self.head = 0;
            self.phase = !self.phase;
        }
    }

    /// Zeroes the ring memory and resets head and phase. Must run before
    /// the controller may post to this queue.
    pub fn reset<M: DmaMemory + ?Sized>(&mut self, mem: &mut M) {
        let zero = [0u8; 256];
        let total = u64::from(self.entries) * CQE_SIZE;
        let mut done = 0;
        while done < total {
            let n = (total - done).min(zero.len() as u64);
            mem.write(self.base + done, &zero[..n as usize]);
            done += n;
        }
        self.head = 0;
        self.phase = true;
    }
}

const INDEX_BITS: u32 = 7;
const INDEX_MASK: u16 = (1 << INDEX_BITS) - 1;
const SEQ_MASK: u16 = (1 << (16 - INDEX_BITS)) - 1;

/// What a tracked command is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Caller I/O command.
    Io { tag: u64 },
    /// Admin command issued by the driver itself.
    Admin,
    /// Abort of the I/O command with this CID.
    Abort,
}

/// Lifecycle of a tracked command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotState {
    Free,
    Active,
    /// Timed out; an Abort was issued. Reset is needed after `until`.
    Aborting {
        until: u64,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Slot {
    pub state: SlotState,
    pub seq: u16,
    pub kind: Kind,
    pub deadline: u64,
}

const FREE_SLOT: Slot = Slot {
    state: SlotState::Free,
    seq: 0,
    kind: Kind::Admin,
    deadline: 0,
};

/// Fixed-capacity command tracker; `N` <= 128.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slots<const N: usize> {
    slots: [Slot; N],
    limit: usize,
    busy: usize,
}

impl<const N: usize> Slots<N> {
    pub const fn new() -> Self {
        assert!(N <= 1 << INDEX_BITS);
        Self {
            slots: [FREE_SLOT; N],
            limit: N,
            busy: 0,
        }
    }

    /// Caps the number of simultaneously busy slots (at most `N`).
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.min(N);
    }

    pub const fn busy(&self) -> usize {
        self.busy
    }

    pub const fn has_free(&self) -> bool {
        self.busy < self.limit
    }

    pub fn cid(&self, idx: usize) -> u16 {
        self.slots[idx].seq << INDEX_BITS | idx as u16
    }

    /// Takes a free slot below the limit; returns its index and CID.
    pub fn alloc(&mut self, kind: Kind, deadline: u64) -> Option<(usize, u16)> {
        if !self.has_free() {
            return None;
        }
        let idx = self.slots[..self.limit]
            .iter()
            .position(|s| s.state == SlotState::Free)?;
        let slot = &mut self.slots[idx];
        slot.seq = (slot.seq + 1) & SEQ_MASK;
        slot.state = SlotState::Active;
        slot.kind = kind;
        slot.deadline = deadline;
        self.busy += 1;
        Some((idx, self.cid(idx)))
    }

    /// Finds the busy slot a completion's CID refers to.
    pub fn lookup(&self, cid: u16) -> Result<usize, CompletionError> {
        let idx = usize::from(cid & INDEX_MASK);
        let seq = cid >> INDEX_BITS;
        let Some(slot) = self.slots.get(idx) else {
            return Err(CompletionError::UnknownCid(cid));
        };
        match slot.state {
            SlotState::Free if slot.seq == seq => Err(CompletionError::DuplicateCid(cid)),
            SlotState::Free => Err(CompletionError::UnknownCid(cid)),
            _ if slot.seq != seq => Err(CompletionError::UnknownCid(cid)),
            _ => Ok(idx),
        }
    }

    pub fn get(&self, idx: usize) -> &Slot {
        &self.slots[idx]
    }

    pub fn get_mut(&mut self, idx: usize) -> &mut Slot {
        &mut self.slots[idx]
    }

    pub fn release(&mut self, idx: usize) {
        if self.slots[idx].state != SlotState::Free {
            self.slots[idx].state = SlotState::Free;
            self.busy -= 1;
        }
    }

    /// Frees every slot without reporting anything (sequence numbers are
    /// kept, so stale identifiers stay stale).
    pub fn release_all(&mut self) {
        for i in 0..N {
            self.release(i);
        }
    }

    /// Index of the first busy slot at or after `from`.
    pub fn next_busy(&self, from: usize) -> Option<usize> {
        (from..N).find(|&i| self.slots[i].state != SlotState::Free)
    }
}
