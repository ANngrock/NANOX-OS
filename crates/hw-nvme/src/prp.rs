//! Physical Region Page entries and lists (NVMe Base 2.0 §4.1.1 (?),
//! "Physical Region Page Entry and List").
//!
//! Rules implemented, for a memory page size of 4 KiB:
//!
//! * PRP1 addresses the first byte; its offset within the page must be
//!   dword aligned.
//! * If the transfer ends in the first page, PRP2 is 0.
//! * If it ends in the second page, PRP2 is the page-aligned address of
//!   that page.
//! * Otherwise PRP2 points to a PRP list: page-aligned entries, one per
//!   remaining page. A list page holds 512 entries; when more entries
//!   follow, the last entry of a list page points to the next list page.
//!
//! [`build`] validates everything before it writes any list entry.

use crate::{DmaMemory, PAGE_SIZE};

/// Entries per PRP list page.
pub const ENTRIES_PER_LIST_PAGE: u64 = PAGE_SIZE / 8;

/// A data buffer as the device sees it: the physical pages in transfer
/// order, the byte offset into the first page and the length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataBuffer<'a> {
    /// Page-aligned physical address of every page the transfer touches,
    /// in order; exactly `ceil((offset + len) / 4096)` entries.
    pub pages: &'a [u64],
    /// Offset of the first byte in `pages[0]`, dword aligned, below 4096.
    pub offset: u32,
    /// Transfer length in bytes.
    pub len: u32,
}

/// Physically contiguous pages reserved for the PRP lists of one command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrpList {
    /// Page-aligned physical address of the first list page.
    pub base: u64,
    /// Number of list pages.
    pub pages: u32,
}

/// PRP entries of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prp {
    /// PRP entry 1.
    pub prp1: u64,
    /// PRP entry 2 (0, a page, or a list pointer).
    pub prp2: u64,
}

/// Why a buffer was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrpError {
    /// Zero length.
    Empty,
    /// Buffer length differs from the transfer length of the command.
    Length,
    /// First-page offset not dword aligned.
    OffsetAlignment,
    /// First-page offset not below the page size.
    OffsetRange,
    /// Page `index` is not page aligned.
    PageAlignment(usize),
    /// `pages` does not have exactly the number of pages the transfer
    /// spans.
    PageCount {
        /// Pages the transfer spans.
        expected: u64,
        /// Pages supplied.
        got: usize,
    },
    /// The PRP list region is too small for this transfer.
    ListTooSmall {
        /// List pages needed.
        needed: u64,
        /// List pages available.
        available: u32,
    },
    /// The PRP list region is not page aligned or wraps the address space.
    ListRegion,
}

/// Pages touched by `len` bytes starting at `offset` in the first page.
#[must_use]
pub const fn data_pages(offset: u32, len: u32) -> u64 {
    (offset as u64 + len as u64).div_ceil(PAGE_SIZE)
}

/// List pages needed for a transfer touching `data_pages` pages.
#[must_use]
pub const fn list_pages_needed(data_pages: u64) -> u64 {
    if data_pages <= 2 {
        return 0;
    }
    let entries = data_pages - 1;
    if entries <= ENTRIES_PER_LIST_PAGE {
        1
    } else {
        1 + (entries - ENTRIES_PER_LIST_PAGE).div_ceil(ENTRIES_PER_LIST_PAGE - 1)
    }
}

/// Data pages that `list_pages` list pages can describe (PRP1 included).
#[must_use]
pub const fn max_data_pages(list_pages: u32) -> u64 {
    if list_pages == 0 {
        2
    } else {
        1 + ENTRIES_PER_LIST_PAGE + (list_pages as u64 - 1) * (ENTRIES_PER_LIST_PAGE - 1)
    }
}

/// Checks `buf` against the PRP rules and the list region; returns the
/// number of data pages.
pub fn validate(buf: &DataBuffer<'_>, list: PrpList) -> Result<u64, PrpError> {
    if buf.len == 0 {
        return Err(PrpError::Empty);
    }
    if !buf.offset.is_multiple_of(4) {
        return Err(PrpError::OffsetAlignment);
    }
    if u64::from(buf.offset) >= PAGE_SIZE {
        return Err(PrpError::OffsetRange);
    }
    let n = data_pages(buf.offset, buf.len);
    if buf.pages.len() as u64 != n {
        return Err(PrpError::PageCount {
            expected: n,
            got: buf.pages.len(),
        });
    }
    if let Some(i) = buf.pages.iter().position(|p| !p.is_multiple_of(PAGE_SIZE)) {
        return Err(PrpError::PageAlignment(i));
    }
    let needed = list_pages_needed(n);
    if needed > u64::from(list.pages) {
        return Err(PrpError::ListTooSmall {
            needed,
            available: list.pages,
        });
    }
    if needed > 0
        && (!list.base.is_multiple_of(PAGE_SIZE)
            || list.base.checked_add(needed * PAGE_SIZE).is_none())
    {
        return Err(PrpError::ListRegion);
    }
    Ok(n)
}

fn write_entries<M: DmaMemory + ?Sized>(mem: &mut M, at: u64, entries: &[u64]) {
    let mut chunk = [0u8; 256];
    let mut pa = at;
    for group in entries.chunks(chunk.len() / 8) {
        for (slot, e) in chunk.chunks_exact_mut(8).zip(group) {
            slot.copy_from_slice(&e.to_le_bytes());
        }
        let bytes = group.len() * 8;
        mem.write(pa, &chunk[..bytes]);
        pa += bytes as u64;
    }
}

/// Validates `buf`, writes the PRP list (if any) into `list` and returns
/// the PRP entries. Nothing is written when validation fails.
pub fn build<M: DmaMemory + ?Sized>(
    mem: &mut M,
    buf: &DataBuffer<'_>,
    list: PrpList,
) -> Result<Prp, PrpError> {
    let n = validate(buf, list)?;
    let prp1 = buf.pages[0] + u64::from(buf.offset);
    let prp2 = match n {
        1 => 0,
        2 => buf.pages[1],
        _ => {
            let per_page = ENTRIES_PER_LIST_PAGE as usize;
            let mut rest = &buf.pages[1..];
            let mut page = list.base;
            while rest.len() > per_page {
                write_entries(mem, page, &rest[..per_page - 1]);
                write_entries(mem, page + PAGE_SIZE - 8, &[page + PAGE_SIZE]);
                rest = &rest[per_page - 1..];
                page += PAGE_SIZE;
            }
            write_entries(mem, page, rest);
            list.base
        }
    };
    Ok(Prp { prp1, prp2 })
}
