mod common;

use common::{bdf, segment_from, Func, Model, Q35_MCFG};
use hw_pci::{
    capabilities, extended_capabilities, find_capability, find_extended_capability, probe_bars,
    Bdf, Capability, ExtendedCapability, HeaderKind, Msi, MsiX, PciError, PcieCapability,
    PciePortType, CAP_ID_MSI, CAP_ID_MSIX, CAP_ID_PCIE,
};

/// Device 4 on the root bus (physical bus 0, bus number 0).
const DEVICE: u8 = 4;

fn single(f: Func) -> (Model, Bdf) {
    let mut model = Model::new(segment_from(Q35_MCFG));
    model.add(0, DEVICE, 0, f);
    (model, bdf(0, DEVICE, 0))
}

fn list(model: &mut Model, at: Bdf) -> Vec<Result<Capability, PciError>> {
    capabilities(model, at, HeaderKind::Endpoint).collect()
}

fn plain() -> Func {
    Func::endpoint(0x1AF4, 0x1042, 1, 0, 0)
}

#[test]
fn walks_list_in_link_order() {
    let (mut model, at) = single(
        plain()
            .msix(0x98, 2, (1, 0), (1, 0x800))
            .cap(0x84, 0x09, &[0x14])
            .cap(0x40, 0x09, &[0x10]),
    );
    let caps: Vec<_> = list(&mut model, at)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        caps,
        vec![
            Capability {
                id: 0x11,
                offset: 0x98
            },
            Capability {
                id: 0x09,
                offset: 0x84
            },
            Capability {
                id: 0x09,
                offset: 0x40
            },
        ]
    );
    assert_eq!(
        find_capability(&mut model, at, HeaderKind::Endpoint, CAP_ID_MSIX),
        Ok(Some(0x98))
    );
    assert_eq!(
        find_capability(&mut model, at, HeaderKind::Endpoint, CAP_ID_MSI),
        Ok(None)
    );
}

#[test]
fn status_bit_clear_means_no_list_even_with_pointer() {
    let mut f = plain().cap(0x40, 0x05, &[0; 8]);
    let status = f.get16(0x06) & !0x10;
    f.set16(0x06, status);
    let (mut model, at) = single(f);
    assert!(list(&mut model, at).is_empty());
}

#[test]
fn cardbus_uses_its_own_pointer_register() {
    let mut f = plain().cap(0x40, 0x05, &[0; 8]);
    f.cfg[0x34] = 0;
    f.cfg[0x14] = 0x40;
    let (mut model, at) = single(f);
    let caps: Vec<_> = capabilities(&mut model, at, HeaderKind::CardBus).collect();
    assert_eq!(
        caps,
        vec![Ok(Capability {
            id: 0x05,
            offset: 0x40
        })]
    );
}

#[test]
fn loop_is_detected() {
    // 0x40 -> 0x50 -> 0x40
    let mut f = plain().cap(0x40, 0x09, &[]).cap(0x50, 0x09, &[]);
    f.cfg[0x51] = 0x40;
    let (mut model, at) = single(f);
    let caps = list(&mut model, at);
    assert_eq!(caps.len(), 3);
    assert_eq!(caps[2], Err(PciError::CapabilityLoop));

    // Self-loop.
    let mut f = plain().cap(0x40, 0x09, &[]);
    f.cfg[0x41] = 0x40;
    let (mut model, at) = single(f);
    assert_eq!(
        list(&mut model, at).last(),
        Some(&Err(PciError::CapabilityLoop))
    );
}

#[test]
fn longest_legal_chain_terminates() {
    // Every dword slot from 0x40 to 0xFC, then a pointer back to the start.
    let mut f = plain();
    for offset in (0x40..=0xFCu16).step_by(4) {
        f = f.cap(offset as u8, 0x09, &[]);
    }
    f.cfg[0xFD] = 0x40;
    let (mut model, at) = single(f);
    let caps = list(&mut model, at);
    assert_eq!(caps.len(), 49);
    assert!(caps[..48].iter().all(Result::is_ok));
    assert_eq!(caps[48], Err(PciError::CapabilityLoop));
}

#[test]
fn pointer_into_header_or_misaligned_is_rejected() {
    for bad in [0x3C, 0x04, 0x01, 0x42, 0x43] {
        let mut f = plain().cap(0x40, 0x09, &[]);
        f.cfg[0x34] = bad;
        let (mut model, at) = single(f);
        assert_eq!(
            list(&mut model, at),
            vec![Err(PciError::CapabilityPointer)],
            "head {bad:#x}"
        );
    }
    let mut f = plain().cap(0x40, 0x09, &[]);
    f.cfg[0x41] = 0x45;
    let (mut model, at) = single(f);
    assert_eq!(list(&mut model, at)[1], Err(PciError::CapabilityPointer));
    // An error mid-list stops iteration for good.
    let mut iter = capabilities(&mut model, at, HeaderKind::Endpoint);
    assert!(iter.next().unwrap().is_ok());
    assert!(iter.next().unwrap().is_err());
    assert!(iter.next().is_none());
}

#[test]
fn extended_list_walk_loop_and_bounds() {
    let (mut model, at) = single(
        plain()
            .ext_cap(0x100, 0x0001, 2)
            .ext_cap(0x148, 0x000D, 1)
            .ext_cap(0xFFC, 0x0019, 1),
    );
    let caps: Vec<_> = extended_capabilities(&mut model, at).collect();
    assert_eq!(
        caps,
        vec![
            Ok(ExtendedCapability {
                id: 1,
                version: 2,
                offset: 0x100
            }),
            Ok(ExtendedCapability {
                id: 0xD,
                version: 1,
                offset: 0x148
            }),
            Ok(ExtendedCapability {
                id: 0x19,
                version: 1,
                offset: 0xFFC
            }),
        ]
    );
    assert_eq!(
        find_extended_capability(&mut model, at, 0xD),
        Ok(Some(0x148))
    );
    assert_eq!(find_extended_capability(&mut model, at, 0x2), Ok(None));

    // Loop back to 0x100.
    let mut f = plain().ext_cap(0x100, 1, 1).ext_cap(0x200, 2, 1);
    let header = f.get32(0x200) | (0x100 << 20);
    f.set32(0x200, header);
    let (mut model, at) = single(f);
    let caps: Vec<_> = extended_capabilities(&mut model, at).collect();
    assert_eq!(caps.last(), Some(&Err(PciError::CapabilityLoop)));
    assert_eq!(caps.len(), 3);

    // Next pointer back into conventional space, or misaligned.
    for bad in [0x0FC, 0x040, 0x102, 0x201] {
        let mut f = plain().ext_cap(0x100, 1, 1);
        let header = f.get32(0x100) | (bad << 20);
        f.set32(0x100, header);
        let (mut model, at) = single(f);
        let caps: Vec<_> = extended_capabilities(&mut model, at).collect();
        assert_eq!(caps[1], Err(PciError::CapabilityPointer), "next {bad:#x}");
    }
}

#[test]
fn extended_header_of_zero_or_all_ones_after_first_is_an_error() {
    for dead in [u32::MAX, 0] {
        let mut f = plain().ext_cap(0x100, 0x0001, 2);
        let header = f.get32(0x100) | (0x200 << 20);
        f.set32(0x100, header);
        f.set32(0x200, dead);
        let (mut model, at) = single(f);
        let caps: Vec<_> = extended_capabilities(&mut model, at).collect();
        assert_eq!(
            caps,
            vec![
                Ok(ExtendedCapability {
                    id: 1,
                    version: 2,
                    offset: 0x100
                }),
                Err(PciError::ExtendedCapabilityHeader),
            ],
            "header {dead:#x}"
        );
        assert_eq!(
            find_extended_capability(&mut model, at, 0xFFFF),
            Err(PciError::ExtendedCapabilityHeader)
        );
    }
}

#[test]
fn absent_extended_space_is_empty() {
    let (mut model, at) = single(plain());
    assert_eq!(extended_capabilities(&mut model, at).count(), 0);
    let mut f = plain();
    f.set32(0x100, u32::MAX);
    let (mut model, at) = single(f);
    assert_eq!(extended_capabilities(&mut model, at).count(), 0);
    // Unreachable function: reads are all ones.
    assert_eq!(extended_capabilities(&mut model, bdf(0, 9, 0)).count(), 0);
}

#[test]
fn msi_layouts() {
    let cases = [
        (false, false, 0x44, None, 0x48, None),
        (true, false, 0x44, Some(0x48), 0x4C, None),
        (false, true, 0x44, None, 0x48, Some((0x4C, 0x50))),
        (true, true, 0x44, Some(0x48), 0x4C, Some((0x50, 0x54))),
    ];
    for (wide, masking, address, upper, data, mask) in cases {
        let (mut model, at) = single(plain().msi(0x40, wide, masking, 3));
        let msi = Msi::read(&mut model, at, 0x40).unwrap();
        assert_eq!(msi.address_64bit, wide);
        assert_eq!(msi.per_vector_masking, masking);
        assert_eq!(msi.vectors_capable, 8);
        assert_eq!(msi.vectors_enabled, 1);
        assert!(!msi.enabled);
        assert_eq!(msi.address_offset, address);
        assert_eq!(msi.upper_address_offset, upper);
        assert_eq!(msi.data_offset, data);
        assert_eq!(msi.mask_offset.zip(msi.pending_offset), mask);
    }
}

#[test]
fn msi_must_fit_in_conventional_space() {
    // 64-bit with masking needs 0x18 bytes: fits at 0xE8, not at 0xEC.
    let (mut model, at) = single(plain().msi(0xE8, true, true, 0));
    assert!(Msi::read(&mut model, at, 0xE8).is_ok());
    let (mut model, at) = single(plain().msi(0xEC, true, true, 0));
    assert_eq!(
        Msi::read(&mut model, at, 0xEC),
        Err(PciError::CapabilityBounds)
    );
    // 32-bit without masking needs 0x0A bytes: fits at 0xF4, not at 0xF8.
    let (mut model, at) = single(plain().msi(0xF4, false, false, 0));
    assert!(Msi::read(&mut model, at, 0xF4).is_ok());
    let (mut model, at) = single(plain().msi(0xF8, false, false, 0));
    assert_eq!(
        Msi::read(&mut model, at, 0xF8),
        Err(PciError::CapabilityBounds)
    );
}

#[test]
fn msi_control_and_identity_errors() {
    let (mut model, at) = single(plain().msi(0x40, false, false, 6));
    assert_eq!(Msi::read(&mut model, at, 0x40), Err(PciError::MsiControl));
    // Enabled vectors (4) above capable (2).
    let mut f = plain().msi(0x40, false, false, 1);
    let control = f.get16(0x42) | (2 << 4);
    f.set16(0x42, control);
    let (mut model, at) = single(f);
    assert_eq!(Msi::read(&mut model, at, 0x40), Err(PciError::MsiControl));
    let (mut model, at) = single(plain().cap(0x40, 0x09, &[]));
    assert_eq!(Msi::read(&mut model, at, 0x40), Err(PciError::CapabilityId));
    assert_eq!(
        Msi::read(&mut model, at, 0x3C),
        Err(PciError::CapabilityPointer)
    );
    assert_eq!(
        Msi::read(&mut model, at, 0x41),
        Err(PciError::CapabilityPointer)
    );
}

fn msix_device(table: (u8, u32), pba: (u8, u32), entries: u16) -> (Model, Bdf) {
    single(
        plain()
            .io(0, 32, 0xC000, true)
            .mem32(1, 4096, 0xC100_1000, false)
            .mem64(2, 16 << 10, 0x80_0000_0000, true)
            .msix(0x98, entries, table, pba),
    )
}

fn read_msix(table: (u8, u32), pba: (u8, u32), entries: u16) -> Result<MsiX, PciError> {
    let (mut model, at) = msix_device(table, pba, entries);
    let bars = probe_bars(&mut model, at, HeaderKind::Endpoint).unwrap();
    MsiX::read(&mut model, at, 0x98, &bars)
}

#[test]
fn msix_resolves_table_and_pba_through_bars() {
    let msix = read_msix((1, 0), (1, 0x800), 2).unwrap();
    assert_eq!(msix.table_size, 2);
    assert_eq!(
        (msix.table.bir, msix.table.offset, msix.table.length),
        (1, 0, 32)
    );
    assert_eq!(msix.table.address, 0xC100_1000);
    assert_eq!((msix.pba.offset, msix.pba.length), (0x800, 8));
    assert_eq!(msix.pba.address, 0xC100_1800);

    // Table in a 64-bit BAR fills it exactly; PBA in another BAR.
    let msix = read_msix((2, 0x3000), (1, 0), 256).unwrap();
    assert_eq!(msix.table.address, 0x80_0000_3000);
    assert_eq!(msix.table.length, 4096);
    assert_eq!(msix.pba.length, 32);
}

#[test]
fn msix_rejects_bad_bir_and_placement() {
    assert_eq!(read_msix((6, 0), (1, 0x800), 2), Err(PciError::MsixBir));
    assert_eq!(read_msix((1, 0), (7, 0), 2), Err(PciError::MsixBir));
    // I/O BAR, upper half of a 64-bit BAR, unimplemented BAR.
    assert_eq!(read_msix((0, 0), (1, 0x800), 2), Err(PciError::MsixBar));
    assert_eq!(read_msix((3, 0), (1, 0x800), 2), Err(PciError::MsixBar));
    assert_eq!(read_msix((5, 0), (1, 0x800), 2), Err(PciError::MsixBar));
    // Table crossing the end of a 4 KiB BAR, or one entry too many.
    assert_eq!(
        read_msix((1, 0xFF8), (2, 0), 2),
        Err(PciError::MsixTableOutOfBar)
    );
    assert_eq!(
        read_msix((1, 0), (2, 0), 257),
        Err(PciError::MsixTableOutOfBar)
    );
    assert_eq!(
        read_msix((2, 0x3008), (1, 0), 256),
        Err(PciError::MsixTableOutOfBar)
    );
    // PBA past the end.
    assert_eq!(
        read_msix((2, 0), (1, 0xFF8), 65),
        Err(PciError::MsixPbaOutOfBar)
    );
    // Offsets near u32::MAX do not wrap.
    assert_eq!(
        read_msix((1, 0xFFFF_FFF8), (2, 0), 1),
        Err(PciError::MsixTableOutOfBar)
    );
    // Overlap inside the same BAR.
    assert_eq!(read_msix((1, 0), (1, 0x10), 2), Err(PciError::MsixOverlap));
}

#[test]
fn msix_capability_must_fit() {
    let (mut model, at) = msix_device((1, 0), (1, 0x800), 1);
    let bars = probe_bars(&mut model, at, HeaderKind::Endpoint).unwrap();
    let f = model.func_mut(0, 4, 0).unwrap();
    f.cfg[0xF8] = 0x11;
    assert_eq!(
        MsiX::read(&mut model, at, 0xF8, &bars),
        Err(PciError::CapabilityBounds)
    );
    assert_eq!(
        MsiX::read(&mut model, at, 0xF4, &bars),
        Err(PciError::CapabilityId)
    );
}

#[test]
fn pcie_capability_port_types() {
    let types = [
        (0x0, PciePortType::Endpoint),
        (0x1, PciePortType::LegacyEndpoint),
        (0x4, PciePortType::RootPort),
        (0x5, PciePortType::UpstreamSwitchPort),
        (0x6, PciePortType::DownstreamSwitchPort),
        (0x7, PciePortType::PcieToPciBridge),
        (0x8, PciePortType::PciToPcieBridge),
        (0x9, PciePortType::RootComplexIntegratedEndpoint),
        (0xA, PciePortType::RootComplexEventCollector),
    ];
    for (field, expected) in types {
        let (mut model, at) = single(plain().pcie(0x40, field, 2));
        let pcie = PcieCapability::read(&mut model, at, 0x40).unwrap();
        assert_eq!(pcie.port_type, expected);
        assert_eq!(pcie.version, 2);
        assert_eq!(
            find_capability(&mut model, at, HeaderKind::Endpoint, CAP_ID_PCIE),
            Ok(Some(0x40))
        );
    }
    for field in [0x2, 0x3, 0xB, 0xF] {
        let (mut model, at) = single(plain().pcie(0x40, field, 2));
        assert_eq!(
            PcieCapability::read(&mut model, at, 0x40),
            Err(PciError::PcieCapability)
        );
    }
    for version in [0, 3] {
        let (mut model, at) = single(plain().pcie(0x40, 0, version));
        assert_eq!(
            PcieCapability::read(&mut model, at, 0x40),
            Err(PciError::PcieCapability)
        );
    }
    // v2 structure is 0x3C bytes: 0xC4 fits, 0xC8 does not; v1 (0x24) fits at 0xDC.
    let (mut model, at) = single(plain().pcie(0xC4, 0, 2));
    assert!(PcieCapability::read(&mut model, at, 0xC4).is_ok());
    let (mut model, at) = single(plain().pcie(0xC8, 0, 2));
    assert_eq!(
        PcieCapability::read(&mut model, at, 0xC8),
        Err(PciError::CapabilityBounds)
    );
    let (mut model, at) = single(plain().pcie(0xDC, 0, 1));
    assert!(PcieCapability::read(&mut model, at, 0xDC).is_ok());
}
