mod common;

use common::{bdf, mcfg_allocation, LENOVO_MCFG, Q35_MCFG};
use hw_pci::{AccessWidth, Bdf, BusRange, EcamSegment, PciError, PHYS_ADDRESS_LIMIT};

#[test]
fn q35_fixture_segment_matches_captured_mcfg() {
    let (base, segment, start, end) = mcfg_allocation(Q35_MCFG);
    assert_eq!((base, segment, start, end), (0xE000_0000, 0, 0, 0xFF));
    let ecam = EcamSegment::new(base, segment, start, end).unwrap();
    assert_eq!(ecam.region_start(), 0xE000_0000);
    assert_eq!(ecam.region_len(), 256 << 20);
    assert_eq!(
        ecam.address(bdf(0, 2, 0), 0x10, AccessWidth::Dword),
        Ok(0xE001_0010)
    );
    assert_eq!(
        ecam.address(bdf(0xFF, 31, 7), 0xFFC, AccessWidth::Dword),
        Ok(0xEFFF_FFFC)
    );
}

#[test]
fn lenovo_fixture_segment_limits_buses() {
    let (base, segment, start, end) = mcfg_allocation(LENOVO_MCFG);
    assert_eq!((base, segment, start, end), (0xF800_0000, 0, 0, 0x3F));
    let ecam = EcamSegment::new(base, segment, start, end).unwrap();
    assert_eq!(ecam.region_len(), 64 << 20);
    assert_eq!(
        ecam.address(bdf(0x3F, 0, 0), 0, AccessWidth::Byte),
        Ok(0xFBF0_0000)
    );
    assert_eq!(
        ecam.address(bdf(0x40, 0, 0), 0, AccessWidth::Byte),
        Err(PciError::BusOutOfRange)
    );
}

#[test]
fn segment_with_nonzero_start_bus_uses_bus_zero_base() {
    let ecam = EcamSegment::new(0xC000_0000, 1, 0x80, 0x8F).unwrap();
    assert_eq!(ecam.region_start(), 0xC000_0000 + (0x80 << 20));
    assert_eq!(ecam.region_len(), 16 << 20);
    assert_eq!(
        ecam.address(bdf(0x7F, 0, 0), 0, AccessWidth::Dword),
        Err(PciError::BusOutOfRange)
    );
    assert_eq!(
        ecam.address(bdf(0x90, 0, 0), 0, AccessWidth::Dword),
        Err(PciError::BusOutOfRange)
    );
    let address = ecam
        .address(bdf(0x85, 3, 1), 0x44, AccessWidth::Word)
        .unwrap();
    assert_eq!(ecam.decode(address), Ok((bdf(0x85, 3, 1), 0x44)));
    assert_eq!(
        ecam.decode(ecam.region_start() - 1),
        Err(PciError::BusOutOfRange)
    );
    assert_eq!(
        ecam.decode(ecam.region_start() + ecam.region_len()),
        Err(PciError::BusOutOfRange)
    );
    assert_eq!(ecam.decode(0), Err(PciError::BusOutOfRange));
}

#[test]
fn offsets_are_bounded_and_aligned() {
    let ecam = EcamSegment::new(0xE000_0000, 0, 0, 0xFF).unwrap();
    let f = bdf(1, 2, 3);
    assert!(ecam.address(f, 0xFFF, AccessWidth::Byte).is_ok());
    assert!(ecam.address(f, 0xFFE, AccessWidth::Word).is_ok());
    assert_eq!(
        ecam.address(f, 0x1000, AccessWidth::Byte),
        Err(PciError::InvalidOffset)
    );
    assert_eq!(
        ecam.address(f, 0xFFFF, AccessWidth::Dword),
        Err(PciError::InvalidOffset)
    );
    assert_eq!(
        ecam.address(f, 0xFFE, AccessWidth::Dword),
        Err(PciError::InvalidOffset)
    );
    assert_eq!(
        ecam.address(f, 0x11, AccessWidth::Word),
        Err(PciError::MisalignedOffset)
    );
    assert_eq!(
        ecam.address(f, 0x12, AccessWidth::Dword),
        Err(PciError::MisalignedOffset)
    );
}

#[test]
fn bdf_rejects_out_of_range_device_and_function() {
    assert_eq!(Bdf::new(0, 32, 0), Err(PciError::InvalidDevice));
    assert_eq!(Bdf::new(0, 0, 8), Err(PciError::InvalidFunction));
    assert_eq!(Bdf::new(0, 255, 255), Err(PciError::InvalidDevice));
    let f = Bdf::new(255, 31, 7).unwrap();
    assert_eq!((f.bus(), f.device(), f.function()), (255, 31, 7));
}

#[test]
fn segment_construction_rejects_bad_ranges() {
    assert_eq!(
        EcamSegment::new(0xE000_0000, 0, 5, 4),
        Err(PciError::InvalidBusRange)
    );
    assert_eq!(BusRange::new(9, 8), Err(PciError::InvalidBusRange));
    assert_eq!(
        EcamSegment::new(0xE008_0000, 0, 0, 0),
        Err(PciError::EcamAlignment)
    );
    assert_eq!(
        EcamSegment::new(u64::MAX & !0xF_FFFF, 0, 0, 0xFF),
        Err(PciError::EcamOverflow)
    );
    // Region end exactly at the physical limit is accepted, one bus more is not.
    let base = PHYS_ADDRESS_LIMIT - (256 << 20);
    assert!(EcamSegment::new(base, 0, 0, 0xFF).is_ok());
    assert_eq!(
        EcamSegment::new(base + (1 << 20), 0, 0, 0xFF),
        Err(PciError::EcamOverflow)
    );
}
