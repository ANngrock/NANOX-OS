//! A deliberately small ELF64 executable subset. Validation completes before
//! any caller allocates memory, copies segments, or transfers control.

use crate::{
    page_rounded, u16_at, u32_at, u64_at, valid_flags, KERNEL_BASE, KERNEL_WINDOW_SIZE, PAGE_SIZE,
};
pub use crate::{PF_R, PF_W, PF_X};

pub const MAX_FILE_SIZE: usize = 32 * 1024 * 1024;
pub const MAX_PROGRAM_HEADERS: usize = 32;
const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const SECTION_HEADER_SIZE: usize = 64;
const MAX_SECTION_HEADERS: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ElfSegment {
    pub file_offset: u64,
    pub virt_start: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub alignment: u64,
    pub flags: u32,
}

impl ElfSegment {
    pub fn page_count(&self) -> u64 {
        // Parser bounds memory_size by the 128 MiB window, so rounding fits.
        (self.memory_size + PAGE_SIZE - 1) / PAGE_SIZE
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElfImage {
    pub entry: u64,
    pub segments: [ElfSegment; MAX_PROGRAM_HEADERS],
    pub segment_count: usize,
}

impl ElfImage {
    pub fn segments(&self) -> &[ElfSegment] {
        &self.segments[..self.segment_count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    FileSize,
    Truncated,
    Magic,
    Class,
    Endianness,
    Version,
    Abi,
    Type,
    Machine,
    HeaderSize,
    HeaderFlags,
    HeaderCount,
    Overflow,
    FileRange,
    UnsupportedProgramHeader,
    UnsupportedSections,
    Relocations,
    Alignment,
    FileLargerThanMemory,
    EmptySegment,
    SegmentFlags,
    VirtualRange,
    SegmentOverlap,
    LoadLimit,
    NoLoadSegments,
    Entry,
}

pub fn parse(bytes: &[u8]) -> Result<ElfImage, ElfError> {
    if bytes.len() > MAX_FILE_SIZE {
        return Err(ElfError::FileSize);
    }
    if bytes.len() < ELF_HEADER_SIZE {
        return Err(ElfError::Truncated);
    }
    if bytes[..4] != *b"\x7fELF" {
        return Err(ElfError::Magic);
    }
    if bytes[4] != 2 {
        return Err(ElfError::Class);
    }
    if bytes[5] != 1 {
        return Err(ElfError::Endianness);
    }
    if bytes[6] != 1 || u32_at(bytes, 20) != 1 {
        return Err(ElfError::Version);
    }
    if bytes[7] != 0 || bytes[8] != 0 || bytes[9..16] != [0; 7] {
        return Err(ElfError::Abi);
    }
    if u16_at(bytes, 16) != 2 {
        return Err(ElfError::Type);
    }
    if u16_at(bytes, 18) != 62 {
        return Err(ElfError::Machine);
    }
    if u32_at(bytes, 48) != 0 {
        return Err(ElfError::HeaderFlags);
    }
    if u16_at(bytes, 52) != ELF_HEADER_SIZE as u16
        || u16_at(bytes, 54) != PROGRAM_HEADER_SIZE as u16
    {
        return Err(ElfError::HeaderSize);
    }
    let count = u16_at(bytes, 56) as usize;
    if count == 0 || count > MAX_PROGRAM_HEADERS {
        return Err(ElfError::HeaderCount);
    }
    let table_offset = u64_at(bytes, 32);
    if table_offset < ELF_HEADER_SIZE as u64 || table_offset % 8 != 0 {
        return Err(ElfError::HeaderSize);
    }
    let table_len = (count as u64)
        .checked_mul(PROGRAM_HEADER_SIZE as u64)
        .ok_or(ElfError::Overflow)?;
    let table = file_range(bytes, table_offset, table_len)?;
    validate_sections(bytes)?;
    let mut image = ElfImage {
        entry: u64_at(bytes, 24),
        segments: [ElfSegment::default(); MAX_PROGRAM_HEADERS],
        segment_count: 0,
    };
    let mut total = 0u64;
    for header in table.chunks_exact(PROGRAM_HEADER_SIZE) {
        match u32_at(header, 0) {
            0 => continue, // PT_NULL has no load effect.
            1 => {}        // PT_LOAD.
            0x6474_e551 => {
                // GNU_STACK is metadata only; M0 supplies its own fixed NX stack.
                if u32_at(header, 4) != PF_R | PF_W
                    || u64_at(header, 32) != 0
                    || u64_at(header, 40) != 0
                {
                    return Err(ElfError::UnsupportedProgramHeader);
                }
                continue;
            }
            // Includes INTERP, DYNAMIC, TLS, PHDR, NOTE, and unknown extensions.
            _ => return Err(ElfError::UnsupportedProgramHeader),
        }
        let segment = ElfSegment {
            file_offset: u64_at(header, 8),
            virt_start: u64_at(header, 16),
            file_size: u64_at(header, 32),
            memory_size: u64_at(header, 40),
            flags: u32_at(header, 4),
            alignment: u64_at(header, 48),
        };
        // p_paddr is intentionally ignored: firmware chooses physical placement.
        if segment.file_size > segment.memory_size {
            return Err(ElfError::FileLargerThanMemory);
        }
        if segment.memory_size == 0 {
            return Err(ElfError::EmptySegment);
        }
        file_range(bytes, segment.file_offset, segment.file_size)?;
        if !valid_flags(segment.flags) {
            return Err(ElfError::SegmentFlags);
        }
        if segment.alignment < PAGE_SIZE
            || !segment.alignment.is_power_of_two()
            || segment.alignment > KERNEL_WINDOW_SIZE
            || segment.virt_start % PAGE_SIZE != 0
            || segment.file_offset % PAGE_SIZE != 0
            || segment.virt_start % segment.alignment != segment.file_offset % segment.alignment
        {
            return Err(ElfError::Alignment);
        }
        let rounded = page_rounded(segment.memory_size).ok_or(ElfError::Overflow)?;
        let end = segment
            .virt_start
            .checked_add(rounded)
            .ok_or(ElfError::Overflow)?;
        if segment.virt_start < KERNEL_BASE || end > KERNEL_BASE + KERNEL_WINDOW_SIZE {
            return Err(ElfError::VirtualRange);
        }
        total = total.checked_add(rounded).ok_or(ElfError::Overflow)?;
        if total > KERNEL_WINDOW_SIZE {
            return Err(ElfError::LoadLimit);
        }
        for previous in image.segments() {
            let prior_end = previous.virt_start + previous.page_count() * PAGE_SIZE;
            if segment.virt_start < prior_end && previous.virt_start < end {
                return Err(ElfError::SegmentOverlap);
            }
        }
        image.segments[image.segment_count] = segment;
        image.segment_count += 1;
    }
    if image.segment_count == 0 {
        return Err(ElfError::NoLoadSegments);
    }
    if !image.segments().iter().any(|s| {
        s.flags & PF_X != 0
            && image.entry >= s.virt_start
            && image.entry < s.virt_start + s.memory_size
    }) {
        return Err(ElfError::Entry);
    }
    Ok(image)
}

fn file_range(bytes: &[u8], offset: u64, length: u64) -> Result<&[u8], ElfError> {
    let end = offset.checked_add(length).ok_or(ElfError::Overflow)?;
    if end > bytes.len() as u64 {
        return Err(ElfError::FileRange);
    }
    Ok(&bytes[offset as usize..end as usize])
}

fn validate_sections(bytes: &[u8]) -> Result<(), ElfError> {
    let offset = u64_at(bytes, 40);
    let stride = u16_at(bytes, 58) as usize;
    let count = u16_at(bytes, 60) as usize;
    let names = u16_at(bytes, 62) as usize;
    if offset == 0 {
        if count != 0 || names != 0 || (stride != 0 && stride != SECTION_HEADER_SIZE) {
            return Err(ElfError::UnsupportedSections);
        }
        return Ok(());
    }
    // Extended numbering is intentionally outside M0's supported subset.
    if count == 0
        || count > MAX_SECTION_HEADERS
        || stride != SECTION_HEADER_SIZE
        || names >= count
        || offset < ELF_HEADER_SIZE as u64
        || offset % 8 != 0
    {
        return Err(ElfError::UnsupportedSections);
    }
    let len = (count as u64)
        .checked_mul(stride as u64)
        .ok_or(ElfError::Overflow)?;
    for section in file_range(bytes, offset, len)?.chunks_exact(stride) {
        let kind = u32_at(section, 4);
        match kind {
            4 | 9 | 19 => return Err(ElfError::Relocations), // RELA, REL, RELR.
            6 | 11 => return Err(ElfError::UnsupportedSections), // DYNAMIC, DYNSYM.
            _ => {}
        }
        if kind != 8 && kind != 0 {
            // SHT_NOBITS and SHT_NULL own no file bytes.
            file_range(bytes, u64_at(section, 24), u64_at(section, 32))?;
        }
    }
    Ok(())
}
