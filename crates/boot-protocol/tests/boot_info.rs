use boot_protocol::*;

fn header() -> BootInfo {
    let golden = include_bytes!("../../../tests/fixtures/boot-info-v1.bin");
    // SAFETY: golden has exactly 160 initialized bytes, BootInfo has no padding
    // or invalid bit patterns (only fixed integers/byte arrays); unaligned read
    // is intentional. The result is an owned copy with no borrowed pointers.
    unsafe { core::ptr::read_unaligned(golden.as_ptr().cast::<BootInfo>()) }
}

fn buffers() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut map = vec![0u8; 48];
    map[0..4].copy_from_slice(&2u32.to_le_bytes());
    map[8..16].copy_from_slice(&0x100000u64.to_le_bytes());
    map[24..32].copy_from_slice(&0x500u64.to_le_bytes());
    let mut ranges = Vec::new();
    for (start, pages, kind) in [
        (0x100000u64, 64u64, 2u32),
        (0x200000, 128, 3),
        (0x300000, 2, 1),
        (0x400000, 16, 4),
        (0x500000, 4, 5),
    ] {
        ranges.extend(start.to_le_bytes());
        ranges.extend(pages.to_le_bytes());
        ranges.extend(kind.to_le_bytes());
        ranges.extend(0u32.to_le_bytes());
    }
    let mut segments = Vec::new();
    segments.extend(0x300000u64.to_le_bytes());
    segments.extend(KERNEL_BASE.to_le_bytes());
    segments.extend(8192u64.to_le_bytes());
    segments.extend(5u32.to_le_bytes());
    segments.extend(0u32.to_le_bytes());
    (map, ranges, segments)
}

#[test]
fn golden_layout_and_real_stride_are_accepted() {
    let h = header();
    assert_eq!(h.boot_epoch, 42);
    assert_eq!(h.memory_descriptor_size, 48);
    assert_eq!(h.memory_map_virt, HANDOFF_BASE + 16384);
    assert_eq!(h.validate_header_at(HANDOFF_BASE), Ok(()));
    let (map, ranges, segments) = buffers();
    assert_eq!(h.validate_buffers(&map, &ranges, &segments), Ok(()));
    // No padding exists: the field offset assertions cover all 160 bytes.
    // SAFETY: shared immutable borrow of an initialized live header, exact size,
    // u8 alignment, no concurrent mutation, and no uninitialized padding.
    let bytes = unsafe { core::slice::from_raw_parts((&h as *const BootInfo).cast::<u8>(), 160) };
    assert_eq!(
        bytes,
        include_bytes!("../../../tests/fixtures/boot-info-v1.bin")
    );
}

#[test]
fn rejects_version_flags_and_missing_exit() {
    let mut h = header();
    h.major = 2;
    assert_eq!(h.validate_header(), Err(BootError::Version));
    let mut h = header();
    h.flags = 2;
    assert_eq!(h.validate_header(), Err(BootError::ExitBootServices));
    let mut h = header();
    h.flags |= 4;
    assert_eq!(h.validate_header(), Err(BootError::Flags));
    let mut h = header();
    h.reserved0[5] = 1;
    assert_eq!(h.validate_header(), Err(BootError::Reserved));
}

#[test]
fn pointer_wrap_counts_and_header_overlap_are_rejected_before_dereference() {
    let mut h = header();
    h.memory_map_virt = u64::MAX - 7;
    assert_eq!(h.validate_header(), Err(BootError::Overflow));
    let mut h = header();
    h.reserved_ranges_count = u32::MAX;
    assert_eq!(h.validate_header(), Err(BootError::CountOrStride));
    let mut h = header();
    h.memory_map_virt = HANDOFF_BASE;
    assert_eq!(
        h.validate_header_at(HANDOFF_BASE),
        Err(BootError::BufferOverlap)
    );
    let mut h = header();
    h.memory_map_virt = h.load_segments_virt;
    assert_eq!(h.validate_header(), Err(BootError::BufferOverlap));
}

#[test]
fn truncated_buffers_and_bad_stride_are_rejected() {
    let h = header();
    let (map, ranges, segments) = buffers();
    assert_eq!(
        h.validate_buffers(&map[..40], &ranges, &segments),
        Err(BootError::BufferLength)
    );
    assert_eq!(
        h.validate_buffers(&map, &ranges[..119], &segments),
        Err(BootError::BufferLength)
    );
    assert_eq!(
        h.validate_buffers(&map, &ranges, &segments[..31]),
        Err(BootError::BufferLength)
    );
    let mut h = header();
    h.memory_descriptor_size = 41;
    assert_eq!(h.validate_header(), Err(BootError::MemoryMapShape));
}

#[test]
fn reserved_ownership_and_overlap_are_enforced() {
    let h = header();
    let (map, mut ranges, segments) = buffers();
    ranges[48 + 16..48 + 20].copy_from_slice(&6u32.to_le_bytes());
    assert_eq!(
        h.validate_buffers(&map, &ranges, &segments),
        Err(BootError::Ownership)
    );
    ranges[48 + 16..48 + 20].copy_from_slice(&1u32.to_le_bytes());
    ranges[48..56].copy_from_slice(&0x200000u64.to_le_bytes());
    assert_eq!(
        h.validate_buffers(&map, &ranges, &segments),
        Err(BootError::RangeOverlap)
    );
}

#[test]
fn malformed_memory_and_segment_metadata_never_reaches_kernel_effects() {
    let h = header();
    let (mut map, ranges, mut segments) = buffers();
    map[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        h.validate_buffers(&map, &ranges, &segments),
        Err(BootError::Overflow)
    );
    let (map, _, _) = buffers();
    segments[24..28].copy_from_slice(&7u32.to_le_bytes());
    assert_eq!(
        h.validate_buffers(&map, &ranges, &segments),
        Err(BootError::Segment)
    );
}
