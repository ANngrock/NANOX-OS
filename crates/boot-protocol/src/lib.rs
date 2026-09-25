#![no_std]

//! NANOX BootInfo v1 and its bounded, allocation-free input validators.
//! Physical addresses are integers, never implicitly converted to Rust pointers.

pub mod elf;

pub const PAGE_SIZE: u64 = 4096;
pub const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
pub const KERNEL_WINDOW_SIZE: u64 = 128 * 1024 * 1024;
pub const HANDOFF_BASE: u64 = 0xffff_ffff_9000_0000;
pub const HANDOFF_MAX_SIZE: u64 = 16 * 1024 * 1024;
/// Arena pages actually mapped by this M0 loader (the ABI permits up to 16 MiB).
pub const HANDOFF_MAPPED_SIZE: u64 = 256 * 1024;
pub const STACK_TOP: u64 = 0xffff_ffff_9200_0000;
pub const STACK_SIZE: u64 = 64 * 1024;
pub const SERIAL_PORT: u16 = 0x3f8;
pub const MAGIC: [u8; 8] = *b"NXBOOT01";
pub const HEADER_SIZE: u32 = 160;
pub const FLAG_EXIT_BOOT_SERVICES: u64 = 1;
pub const FLAG_TEST_PROFILE: u64 = 2;
pub const KIND_KERNEL: u32 = 1;
pub const KIND_BOOT_INFO: u32 = 2;
pub const KIND_PAGE_TABLES: u32 = 3;
pub const KIND_STACK: u32 = 4;
pub const KIND_TRANSITION: u32 = 5;
pub const KIND_INITRAMFS: u32 = 6;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;
pub const MAX_RESERVED_RANGES: u32 = 256;
pub const MAX_LOAD_SEGMENTS: u32 = 32;
const PHYS_LIMIT: u64 = 1 << 52;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub magic: [u8; 8],
    pub major: u16,
    pub minor: u16,
    pub header_size: u32,
    pub total_size: u64,
    pub flags: u64,
    pub memory_map_phys: u64,
    pub memory_map_virt: u64,
    pub memory_map_len: u64,
    pub memory_descriptor_size: u32,
    pub memory_descriptor_version: u32,
    pub reserved_ranges_phys: u64,
    pub reserved_ranges_virt: u64,
    pub reserved_ranges_count: u32,
    pub reserved_ranges_stride: u32,
    pub rsdp_phys: u64,
    pub kernel_entry_virt: u64,
    pub load_segments_phys: u64,
    pub load_segments_virt: u64,
    pub load_segments_count: u32,
    pub load_segments_stride: u32,
    pub pml4_phys: u64,
    pub stack_top_virt: u64,
    pub serial_io_port: u16,
    pub reserved0: [u8; 6],
    pub boot_epoch: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReservedRange {
    pub phys_start: u64,
    pub page_count: u64,
    pub kind: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadedSegment {
    pub phys_start: u64,
    pub virt_start: u64,
    pub memory_size: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootError {
    Magic,
    Version,
    HeaderSize,
    Flags,
    ExitBootServices,
    Reserved,
    MemoryMapShape,
    CountOrStride,
    Address,
    Overflow,
    BufferOverlap,
    BufferLength,
    Descriptor,
    ReservedRange,
    RangeOverlap,
    Segment,
    Entry,
    Ownership,
}

impl Default for BootInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl BootInfo {
    /// Construct the versioned header; the loader must fill its live addresses.
    pub const fn new() -> Self {
        Self {
            magic: MAGIC,
            major: 1,
            minor: 0,
            header_size: HEADER_SIZE,
            total_size: HEADER_SIZE as u64,
            flags: 0,
            memory_map_phys: 0,
            memory_map_virt: 0,
            memory_map_len: 0,
            memory_descriptor_size: 0,
            memory_descriptor_version: 1,
            reserved_ranges_phys: 0,
            reserved_ranges_virt: 0,
            reserved_ranges_count: 0,
            reserved_ranges_stride: 24,
            rsdp_phys: 0,
            kernel_entry_virt: 0,
            load_segments_phys: 0,
            load_segments_virt: 0,
            load_segments_count: 0,
            load_segments_stride: 32,
            pml4_phys: 0,
            stack_top_virt: STACK_TOP,
            serial_io_port: SERIAL_PORT,
            reserved0: [0; 6],
            boot_epoch: 0,
        }
    }

    /// Validate integer metadata before the caller creates any pointed-to slice.
    /// This cannot prove mappings exist: the kernel's entry boundary owns that
    /// invariant. The fixed handoff window bounds all subsequent slice lengths.
    pub fn validate_header(&self) -> Result<(), BootError> {
        if self.magic != MAGIC {
            return Err(BootError::Magic);
        }
        if self.major != 1 || self.minor != 0 {
            return Err(BootError::Version);
        }
        if self.header_size != HEADER_SIZE || self.total_size != HEADER_SIZE as u64 {
            return Err(BootError::HeaderSize);
        }
        if self.flags & !(FLAG_EXIT_BOOT_SERVICES | FLAG_TEST_PROFILE) != 0 {
            return Err(BootError::Flags);
        }
        if self.flags & FLAG_EXIT_BOOT_SERVICES == 0 {
            return Err(BootError::ExitBootServices);
        }
        if self.reserved0 != [0; 6] {
            return Err(BootError::Reserved);
        }
        let stride = self.memory_descriptor_size as u64;
        if stride < 40
            || !stride.is_multiple_of(8)
            || self.memory_descriptor_version != 1
            || self.memory_map_len == 0
            || !self.memory_map_len.is_multiple_of(stride)
        {
            return Err(BootError::MemoryMapShape);
        }
        if self.reserved_ranges_count == 0
            || self.reserved_ranges_count > MAX_RESERVED_RANGES
            || self.reserved_ranges_stride != 24
            || self.load_segments_count == 0
            || self.load_segments_count > MAX_LOAD_SEGMENTS
            || self.load_segments_stride != 32
        {
            return Err(BootError::CountOrStride);
        }
        let lengths = [
            self.memory_map_len,
            self.reserved_ranges_count as u64 * self.reserved_ranges_stride as u64,
            self.load_segments_count as u64 * self.load_segments_stride as u64,
        ];
        let phys = [
            self.memory_map_phys,
            self.reserved_ranges_phys,
            self.load_segments_phys,
        ];
        let virt = [
            self.memory_map_virt,
            self.reserved_ranges_virt,
            self.load_segments_virt,
        ];
        for i in 0..3 {
            if !phys[i].is_multiple_of(8) || !virt[i].is_multiple_of(8) {
                return Err(BootError::Address);
            }
            physical_range(phys[i], lengths[i])?;
            within(virt[i], lengths[i], HANDOFF_BASE, HANDOFF_MAX_SIZE)?;
            for j in 0..i {
                if overlaps(phys[i], lengths[i], phys[j], lengths[j])
                    || overlaps(virt[i], lengths[i], virt[j], lengths[j])
                {
                    return Err(BootError::BufferOverlap);
                }
            }
        }
        if !self.pml4_phys.is_multiple_of(PAGE_SIZE) {
            return Err(BootError::Address);
        }
        physical_range(self.pml4_phys, PAGE_SIZE)?;
        if self.rsdp_phys >= PHYS_LIMIT {
            return Err(BootError::Address);
        }
        within(self.kernel_entry_virt, 1, KERNEL_BASE, KERNEL_WINDOW_SIZE)?;
        if self.stack_top_virt != STACK_TOP || self.serial_io_port != SERIAL_PORT {
            return Err(BootError::Address);
        }
        Ok(())
    }

    /// Also prove the fixed header does not overlap any external data buffer.
    pub fn validate_header_at(&self, header_virt: u64) -> Result<(), BootError> {
        self.validate_header()?;
        if !header_virt.is_multiple_of(8) {
            return Err(BootError::Address);
        }
        within(
            header_virt,
            HEADER_SIZE as u64,
            HANDOFF_BASE,
            HANDOFF_MAX_SIZE,
        )?;
        for (start, len) in [
            (self.memory_map_virt, self.memory_map_len),
            (
                self.reserved_ranges_virt,
                self.reserved_ranges_count as u64 * 24,
            ),
            (
                self.load_segments_virt,
                self.load_segments_count as u64 * 32,
            ),
        ] {
            if overlaps(header_virt, HEADER_SIZE as u64, start, len) {
                return Err(BootError::BufferOverlap);
            }
        }
        Ok(())
    }

    pub fn validate_memory_map(&self, bytes: &[u8]) -> Result<(), BootError> {
        self.validate_header()?;
        if bytes.len() as u64 != self.memory_map_len {
            return Err(BootError::BufferLength);
        }
        let stride = self.memory_descriptor_size as usize;
        for descriptor in bytes.chunks_exact(stride) {
            let kind = u32_at(descriptor, 0);
            let start = u64_at(descriptor, 8);
            let pages = u64_at(descriptor, 24);
            let len = pages.checked_mul(PAGE_SIZE).ok_or(BootError::Overflow)?;
            if kind > 15 || !start.is_multiple_of(PAGE_SIZE) || pages == 0 {
                return Err(BootError::Descriptor);
            }
            // Firmware may describe physical address zero, unlike allocations.
            let end = start.checked_add(len).ok_or(BootError::Overflow)?;
            if end > PHYS_LIMIT {
                return Err(BootError::Address);
            }
        }
        Ok(())
    }

    pub fn validate_reserved_ranges(&self, bytes: &[u8]) -> Result<(), BootError> {
        self.validate_header()?;
        if bytes.len() != self.reserved_ranges_count as usize * 24 {
            return Err(BootError::BufferLength);
        }
        for (index, record) in bytes.chunks_exact(24).enumerate() {
            let range = ReservedRange::decode(record)?;
            if range.reserved != 0
                || !(KIND_KERNEL..=KIND_INITRAMFS).contains(&range.kind)
                || !range.phys_start.is_multiple_of(PAGE_SIZE)
                || range.page_count == 0
            {
                return Err(BootError::ReservedRange);
            }
            let len = range
                .page_count
                .checked_mul(PAGE_SIZE)
                .ok_or(BootError::Overflow)?;
            physical_range(range.phys_start, len)?;
            for previous in bytes[..index * 24].chunks_exact(24) {
                let prior = ReservedRange::decode(previous)?;
                if overlaps(
                    range.phys_start,
                    len,
                    prior.phys_start,
                    prior.page_count * PAGE_SIZE,
                ) {
                    return Err(BootError::RangeOverlap);
                }
            }
        }
        Ok(())
    }

    pub fn validate_load_segments(&self, bytes: &[u8]) -> Result<(), BootError> {
        self.validate_header()?;
        if bytes.len() != self.load_segments_count as usize * 32 {
            return Err(BootError::BufferLength);
        }
        let mut entry_found = false;
        let mut total = 0u64;
        for (index, record) in bytes.chunks_exact(32).enumerate() {
            let segment = LoadedSegment::decode(record)?;
            if segment.reserved != 0
                || !valid_flags(segment.flags)
                || !segment.phys_start.is_multiple_of(PAGE_SIZE)
                || !segment.virt_start.is_multiple_of(PAGE_SIZE)
                || segment.memory_size == 0
            {
                return Err(BootError::Segment);
            }
            let rounded = page_rounded(segment.memory_size).ok_or(BootError::Overflow)?;
            total = total.checked_add(rounded).ok_or(BootError::Overflow)?;
            if total > KERNEL_WINDOW_SIZE {
                return Err(BootError::Segment);
            }
            physical_range(segment.phys_start, rounded)?;
            within(segment.virt_start, rounded, KERNEL_BASE, KERNEL_WINDOW_SIZE)?;
            if segment.flags & PF_X != 0
                && self.kernel_entry_virt >= segment.virt_start
                && self.kernel_entry_virt < segment.virt_start + segment.memory_size
            {
                entry_found = true;
            }
            for previous in bytes[..index * 32].chunks_exact(32) {
                let prior = LoadedSegment::decode(previous)?;
                let prior_len = page_rounded(prior.memory_size).ok_or(BootError::Overflow)?;
                if overlaps(segment.virt_start, rounded, prior.virt_start, prior_len)
                    || overlaps(segment.phys_start, rounded, prior.phys_start, prior_len)
                {
                    return Err(BootError::RangeOverlap);
                }
            }
        }
        if !entry_found {
            return Err(BootError::Entry);
        }
        Ok(())
    }

    /// Validate supplied bytes and prove their allocations and loaded segments
    /// are represented in the reservation table. No guest pointer is followed.
    pub fn validate_buffers(
        &self,
        map: &[u8],
        ranges: &[u8],
        segments: &[u8],
    ) -> Result<(), BootError> {
        self.validate_memory_map(map)?;
        self.validate_reserved_ranges(ranges)?;
        self.validate_load_segments(segments)?;
        for (start, len, kind) in [
            (self.memory_map_phys, self.memory_map_len, KIND_BOOT_INFO),
            (
                self.reserved_ranges_phys,
                ranges.len() as u64,
                KIND_BOOT_INFO,
            ),
            (
                self.load_segments_phys,
                segments.len() as u64,
                KIND_BOOT_INFO,
            ),
            (self.pml4_phys, PAGE_SIZE, KIND_PAGE_TABLES),
        ] {
            if !reserved_contains(ranges, start, len, kind) {
                return Err(BootError::Ownership);
            }
        }
        let mut stack = false;
        let mut transition = false;
        for record in ranges.chunks_exact(24) {
            let range = ReservedRange::decode(record)?;
            stack |= range.kind == KIND_STACK && range.page_count * PAGE_SIZE >= STACK_SIZE;
            transition |= range.kind == KIND_TRANSITION;
        }
        if !stack || !transition {
            return Err(BootError::Ownership);
        }
        for record in segments.chunks_exact(32) {
            let segment = LoadedSegment::decode(record)?;
            let len = page_rounded(segment.memory_size).ok_or(BootError::Overflow)?;
            if !reserved_contains(ranges, segment.phys_start, len, KIND_KERNEL) {
                return Err(BootError::Ownership);
            }
        }
        Ok(())
    }
}

impl ReservedRange {
    pub fn decode(bytes: &[u8]) -> Result<Self, BootError> {
        if bytes.len() != 24 {
            return Err(BootError::BufferLength);
        }
        Ok(Self {
            phys_start: u64_at(bytes, 0),
            page_count: u64_at(bytes, 8),
            kind: u32_at(bytes, 16),
            reserved: u32_at(bytes, 20),
        })
    }
}

impl LoadedSegment {
    pub fn decode(bytes: &[u8]) -> Result<Self, BootError> {
        if bytes.len() != 32 {
            return Err(BootError::BufferLength);
        }
        Ok(Self {
            phys_start: u64_at(bytes, 0),
            virt_start: u64_at(bytes, 8),
            memory_size: u64_at(bytes, 16),
            flags: u32_at(bytes, 24),
            reserved: u32_at(bytes, 28),
        })
    }
}

fn reserved_contains(bytes: &[u8], start: u64, len: u64, kind: u32) -> bool {
    bytes.chunks_exact(24).any(|record| {
        // Called only after validate_reserved_ranges proved these products/ranges.
        let range = ReservedRange::decode(record).unwrap();
        range.kind == kind
            && start >= range.phys_start
            && start
                .checked_add(len)
                .is_some_and(|end| end <= range.phys_start + range.page_count * PAGE_SIZE)
    })
}

pub const fn page_rounded(size: u64) -> Option<u64> {
    match size.checked_add(PAGE_SIZE - 1) {
        Some(value) => Some(value & !(PAGE_SIZE - 1)),
        None => None,
    }
}

pub const fn valid_flags(flags: u32) -> bool {
    flags & !7 == 0 && flags & PF_R != 0 && flags & (PF_W | PF_X) != (PF_W | PF_X)
}

fn physical_range(start: u64, len: u64) -> Result<(), BootError> {
    let end = start.checked_add(len).ok_or(BootError::Overflow)?;
    if start == 0 || len == 0 || end > PHYS_LIMIT {
        return Err(BootError::Address);
    }
    Ok(())
}

fn within(start: u64, len: u64, base: u64, capacity: u64) -> Result<(), BootError> {
    let end = start.checked_add(len).ok_or(BootError::Overflow)?;
    if start < base || len == 0 || end > base + capacity {
        return Err(BootError::Address);
    }
    Ok(())
}

fn overlaps(a: u64, a_len: u64, b: u64, b_len: u64) -> bool {
    // Callers first validate each endpoint's checked addition.
    a < b + b_len && b < a + a_len
}

pub(crate) fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

// These assertions are compiled on host, UEFI, and kernel targets.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<BootInfo>() == 160);
    assert!(align_of::<BootInfo>() == 8);
    assert!(offset_of!(BootInfo, magic) == 0);
    assert!(offset_of!(BootInfo, major) == 8);
    assert!(offset_of!(BootInfo, minor) == 10);
    assert!(offset_of!(BootInfo, header_size) == 12);
    assert!(offset_of!(BootInfo, total_size) == 16);
    assert!(offset_of!(BootInfo, flags) == 24);
    assert!(offset_of!(BootInfo, memory_map_phys) == 32);
    assert!(offset_of!(BootInfo, memory_map_virt) == 40);
    assert!(offset_of!(BootInfo, memory_map_len) == 48);
    assert!(offset_of!(BootInfo, memory_descriptor_size) == 56);
    assert!(offset_of!(BootInfo, memory_descriptor_version) == 60);
    assert!(offset_of!(BootInfo, reserved_ranges_phys) == 64);
    assert!(offset_of!(BootInfo, reserved_ranges_virt) == 72);
    assert!(offset_of!(BootInfo, reserved_ranges_count) == 80);
    assert!(offset_of!(BootInfo, reserved_ranges_stride) == 84);
    assert!(offset_of!(BootInfo, rsdp_phys) == 88);
    assert!(offset_of!(BootInfo, kernel_entry_virt) == 96);
    assert!(offset_of!(BootInfo, load_segments_phys) == 104);
    assert!(offset_of!(BootInfo, load_segments_virt) == 112);
    assert!(offset_of!(BootInfo, load_segments_count) == 120);
    assert!(offset_of!(BootInfo, load_segments_stride) == 124);
    assert!(offset_of!(BootInfo, pml4_phys) == 128);
    assert!(offset_of!(BootInfo, stack_top_virt) == 136);
    assert!(offset_of!(BootInfo, serial_io_port) == 144);
    assert!(offset_of!(BootInfo, reserved0) == 146);
    assert!(offset_of!(BootInfo, boot_epoch) == 152);
    assert!(size_of::<ReservedRange>() == 24 && align_of::<ReservedRange>() == 8);
    assert!(offset_of!(ReservedRange, phys_start) == 0);
    assert!(offset_of!(ReservedRange, page_count) == 8);
    assert!(offset_of!(ReservedRange, kind) == 16);
    assert!(offset_of!(ReservedRange, reserved) == 20);
    assert!(size_of::<LoadedSegment>() == 32 && align_of::<LoadedSegment>() == 8);
    assert!(offset_of!(LoadedSegment, phys_start) == 0);
    assert!(offset_of!(LoadedSegment, virt_start) == 8);
    assert!(offset_of!(LoadedSegment, memory_size) == 16);
    assert!(offset_of!(LoadedSegment, flags) == 24);
    assert!(offset_of!(LoadedSegment, reserved) == 28);
};
