use boot_protocol::{
    elf::{parse, ElfError, MAX_FILE_SIZE},
    KERNEL_BASE, KERNEL_WINDOW_SIZE,
};

const VALID: &[u8] = include_bytes!("../../../tests/fixtures/valid-minimal.elf");
fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn mutated(at: usize, value: u64) -> Vec<u8> {
    let mut bytes = VALID.to_vec();
    put64(&mut bytes, at, value);
    bytes
}

#[test]
fn valid_input_preserves_bss_and_ignores_physical_address() {
    let image = parse(VALID).unwrap();
    assert_eq!(image.entry, KERNEL_BASE);
    assert_eq!(image.segment_count, 1);
    let segment = image.segments()[0];
    assert_eq!(segment.file_offset, 4096);
    assert_eq!(segment.file_size, 4);
    assert_eq!(segment.memory_size, 8192);
    assert_eq!(segment.page_count(), 2);
    assert_eq!(parse(&mutated(88, u64::MAX)), Ok(image));
}

#[test]
fn committed_negative_fixtures_have_precise_failures() {
    for (bytes, expected) in [
        (
            include_bytes!("../../../tests/fixtures/truncated-header.elf").as_slice(),
            ElfError::Truncated,
        ),
        (
            include_bytes!("../../../tests/fixtures/overlapping-loads.elf").as_slice(),
            ElfError::SegmentOverlap,
        ),
        (
            include_bytes!("../../../tests/fixtures/writable-executable.elf").as_slice(),
            ElfError::SegmentFlags,
        ),
        (
            include_bytes!("../../../tests/fixtures/overflow-offset.elf").as_slice(),
            ElfError::Overflow,
        ),
    ] {
        assert_eq!(parse(bytes), Err(expected));
    }
}

#[test]
fn every_truncation_before_end_of_payload_is_rejected() {
    for length in 0..VALID.len() {
        assert!(parse(&VALID[..length]).is_err(), "accepted length {length}");
    }
}

#[test]
fn file_limit_is_enforced_before_parsing() {
    assert_eq!(parse(&vec![0; MAX_FILE_SIZE + 1]), Err(ElfError::FileSize));
}

#[test]
fn rejects_incompatible_header_contracts() {
    for (offset, byte, error) in [
        (0, 0, ElfError::Magic),
        (4, 1, ElfError::Class),
        (5, 2, ElfError::Endianness),
        (6, 2, ElfError::Version),
        (7, 3, ElfError::Abi),
        (8, 1, ElfError::Abi),
        (15, 1, ElfError::Abi),
        (16, 3, ElfError::Type),
        (18, 3, ElfError::Machine),
        (20, 0, ElfError::Version),
        (48, 1, ElfError::HeaderFlags),
        (52, 63, ElfError::HeaderSize),
        (54, 55, ElfError::HeaderSize),
    ] {
        let mut bytes = VALID.to_vec();
        bytes[offset] = byte;
        assert_eq!(parse(&bytes), Err(error), "offset {offset}");
    }
}

#[test]
fn program_table_bounds_and_counts_are_checked() {
    for count in [0, 33, u16::MAX] {
        let mut bytes = VALID.to_vec();
        put16(&mut bytes, 56, count);
        assert_eq!(parse(&bytes), Err(ElfError::HeaderCount));
    }
    assert_eq!(parse(&mutated(32, u64::MAX - 7)), Err(ElfError::Overflow));
    assert_eq!(parse(&mutated(32, 4096)), Err(ElfError::FileRange));
    assert_eq!(parse(&mutated(32, 63)), Err(ElfError::HeaderSize));
}

#[test]
fn file_ranges_cannot_escape_or_overflow() {
    assert_eq!(parse(&mutated(72, 8192)), Err(ElfError::FileRange));
    assert_eq!(parse(&mutated(72, u64::MAX - 2)), Err(ElfError::Overflow));
    assert_eq!(
        parse(&mutated(96, 8193)),
        Err(ElfError::FileLargerThanMemory)
    );
    let mut bytes = mutated(96, 0);
    put64(&mut bytes, 104, 0);
    assert_eq!(parse(&bytes), Err(ElfError::EmptySegment));
}

#[test]
fn bss_only_segment_is_supported() {
    let bytes = mutated(96, 0);
    assert_eq!(parse(&bytes).unwrap().segments()[0].file_size, 0);
}

#[test]
fn alignment_and_congruence_are_checked() {
    for alignment in [0, 1, 2048, 4097, KERNEL_WINDOW_SIZE * 2] {
        assert_eq!(parse(&mutated(112, alignment)), Err(ElfError::Alignment));
    }
    assert_eq!(
        parse(&mutated(80, KERNEL_BASE + 1)),
        Err(ElfError::Alignment)
    );
    // Aligned offset zero is legal; header bytes can be part of a load segment.
    assert_eq!(parse(&mutated(72, 0)).unwrap().segments()[0].file_offset, 0);
    // Alignment is larger than a page, so offset 4096 and virtual base differ.
    assert_eq!(parse(&mutated(112, 8192)), Err(ElfError::Alignment));
}

#[test]
fn rejects_noncanonical_low_and_reserved_window_addresses() {
    for address in [
        0x400000,
        0x0000_8000_0000_0000,
        0xffff_ffff_9000_0000,
        KERNEL_BASE + KERNEL_WINDOW_SIZE - 4096,
    ] {
        assert_eq!(parse(&mutated(80, address)), Err(ElfError::VirtualRange));
    }
    assert_eq!(
        parse(&mutated(80, u64::MAX - 4095)),
        Err(ElfError::Overflow)
    );
    assert_eq!(parse(&mutated(104, u64::MAX)), Err(ElfError::Overflow));
}

#[test]
fn all_unsupported_program_header_kinds_are_errors() {
    for kind in [2, 3, 4, 6, 7, 0x6474_e550, 0x6474_e552, u32::MAX] {
        let mut bytes = VALID.to_vec();
        put32(&mut bytes, 64, kind);
        assert_eq!(parse(&bytes), Err(ElfError::UnsupportedProgramHeader));
    }
    let mut bytes = VALID.to_vec();
    put32(&mut bytes, 64, 0);
    assert_eq!(parse(&bytes), Err(ElfError::NoLoadSegments));
}

#[test]
fn entry_must_belong_to_executable_memory_not_page_padding() {
    assert_eq!(
        parse(&mutated(24, KERNEL_BASE + 8192)),
        Err(ElfError::Entry)
    );
    let mut bytes = mutated(104, 4);
    put64(&mut bytes, 24, KERNEL_BASE + 4);
    assert_eq!(parse(&bytes), Err(ElfError::Entry));
    let mut bytes = VALID.to_vec();
    put32(&mut bytes, 68, 6);
    assert_eq!(parse(&bytes), Err(ElfError::Entry));
    assert!(parse(&mutated(24, KERNEL_BASE + 8191)).is_ok());
}

#[test]
fn flags_must_be_known_readable_and_never_write_execute() {
    for flags in [0, 1, 2, 3, 7, 8, u32::MAX] {
        let mut bytes = VALID.to_vec();
        put32(&mut bytes, 68, flags);
        assert_eq!(parse(&bytes), Err(ElfError::SegmentFlags));
    }
}

#[test]
fn adjacent_segments_are_valid_and_page_overlap_is_not() {
    let mut bytes = VALID.to_vec();
    put16(&mut bytes, 56, 2);
    let first = bytes[64..120].to_vec();
    bytes[120..176].copy_from_slice(&first);
    put64(&mut bytes, 136, KERNEL_BASE + 8192);
    assert_eq!(parse(&bytes).unwrap().segment_count, 2);
    put64(&mut bytes, 104, 8193);
    assert_eq!(parse(&bytes), Err(ElfError::SegmentOverlap));
}

#[test]
fn optional_section_table_is_bounded_and_relocations_rejected() {
    for kind in [4, 9, 19, 6, 11] {
        let mut bytes = VALID.to_vec();
        put64(&mut bytes, 40, 256);
        put16(&mut bytes, 58, 64);
        put16(&mut bytes, 60, 1);
        put32(&mut bytes, 260, kind);
        let expected = if [4, 9, 19].contains(&kind) {
            ElfError::Relocations
        } else {
            ElfError::UnsupportedSections
        };
        assert_eq!(parse(&bytes), Err(expected));
    }
    let mut bytes = VALID.to_vec();
    put64(&mut bytes, 40, u64::MAX - 7);
    put16(&mut bytes, 58, 64);
    put16(&mut bytes, 60, 1);
    assert_eq!(parse(&bytes), Err(ElfError::Overflow));
    put64(&mut bytes, 40, 256);
    put16(&mut bytes, 60, 0);
    assert_eq!(parse(&bytes), Err(ElfError::UnsupportedSections));
}

#[test]
fn deterministic_mutations_never_panic() {
    // Exercise header, table, and section metadata together across many invalid
    // combinations; a malformed image must be an error, not a panic/hang.
    let mut state = 0x3141_5926u64;
    for _ in 0..4096 {
        let mut bytes = VALID.to_vec();
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let at = state as usize % 512;
            bytes[at] ^= (state >> 32) as u8;
        }
        let _ = parse(&bytes);
    }
}
