//! Per-table rules: synthetic tables for layouts the fixtures lack, and one
//! precise failure per validation rule.

mod common;

use common::{
    fix_checksum, fix_rsdp, fixture, manifest, put16, put32, put64, rsdp_v2, sdt, PROFILES,
};
use hw_acpi::dmar::{DmarStructure, PciPathEntry, SCOPE_PCI_ENDPOINT, SCOPE_PCI_SUB_HIERARCHY};
use hw_acpi::ivrs::{IvhdEntry, Ivmd, IvrsBlock, IVMD_ALL, IVMD_RANGE, IVMD_SELECT};
use hw_acpi::madt::{MadtEntry, TriggerMode};
use hw_acpi::{
    AcpiError, Dmar, Fadt, Gas, Hpet, Ivrs, Madt, Mcfg, RootKind, RootTable, Rsdp, Sdt, PHYS_LIMIT,
};

fn patched(profile: &str, file: &str, patch: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut bytes = fixture(profile, file);
    patch(&mut bytes);
    fix_checksum(&mut bytes);
    bytes
}

// ---------------------------------------------------------------- SDT header

#[test]
fn every_truncation_of_every_table_is_rejected() {
    for profile in PROFILES {
        for table in manifest(profile) {
            if table.signature == "RSDP" || table.signature == "FACS" {
                continue;
            }
            let bytes = fixture(profile, &table.file);
            for n in 0..bytes.len() {
                assert_eq!(
                    Sdt::parse(&bytes[..n]).err(),
                    Some(AcpiError::Truncated),
                    "{profile}/{} cut to {n}",
                    table.file
                );
            }
        }
    }
}

#[test]
fn any_single_byte_change_breaks_the_checksum() {
    for profile in PROFILES {
        for table in manifest(profile) {
            if table.signature == "RSDP" || table.signature == "FACS" {
                continue;
            }
            let bytes = fixture(profile, &table.file);
            // Bytes 4..8 are the length: changing them fails earlier.
            for at in (0..bytes.len())
                .filter(|at| !(4..8).contains(at))
                .step_by(7)
            {
                let mut changed = bytes.clone();
                changed[at] = changed[at].wrapping_add(1);
                assert_eq!(Sdt::parse(&changed).err(), Some(AcpiError::BadChecksum));
            }
        }
    }
}

#[test]
fn header_length_rules() {
    let table = sdt(b"TEST", 1, &[1, 2, 3, 4]);
    let mut short = table.clone();
    put32(&mut short, 4, 35);
    fix_checksum(&mut short);
    assert_eq!(Sdt::parse(&short).err(), Some(AcpiError::BadLength));
    let mut long = table.clone();
    put32(&mut long, 4, 41);
    fix_checksum(&mut long);
    assert_eq!(Sdt::parse(&long).err(), Some(AcpiError::Truncated));
    // Extra caller-buffer bytes after the table are not part of it.
    let mut padded = table.clone();
    padded.extend_from_slice(&[0xaa; 9]);
    let parsed = Sdt::parse(&padded).unwrap();
    assert_eq!(parsed.bytes(), &table[..]);
    assert_eq!(parsed.body(), &[1, 2, 3, 4]);
    assert_eq!(&parsed.header().oem_table_id, b"SYNTHETC");
}

#[test]
fn parsers_reject_other_signatures() {
    let mcfg = fixture("q35-smp4", "MCFG.bin");
    let apic = fixture("q35-smp4", "APIC.bin");
    assert_eq!(Madt::parse(&mcfg).err(), Some(AcpiError::BadSignature));
    assert_eq!(Mcfg::parse(&apic).err(), Some(AcpiError::BadSignature));
    assert_eq!(Fadt::parse(&apic).err(), Some(AcpiError::BadSignature));
    assert_eq!(Hpet::parse(&apic).err(), Some(AcpiError::BadSignature));
    assert_eq!(Dmar::parse(&apic).err(), Some(AcpiError::BadSignature));
    assert_eq!(Ivrs::parse(&apic).err(), Some(AcpiError::BadSignature));
    assert_eq!(
        RootTable::parse(&apic, RootKind::Rsdt).err(),
        Some(AcpiError::BadSignature)
    );
}

// ---------------------------------------------------------------- RSDP

#[test]
fn rsdp_revisions_and_checksums() {
    let q35 = Rsdp::parse(&fixture("q35-smp4", "RSDP.bin")).unwrap();
    assert_eq!(
        (q35.revision, q35.rsdt_address, q35.length),
        (0, 0x1f77_d000, 20)
    );
    assert_eq!(q35.root_pointer().unwrap().kind, RootKind::Rsdt);

    let v2 = Rsdp::parse(&rsdp_v2(0x1000, 0x2_0000_0000)).unwrap();
    assert_eq!(
        (v2.revision, v2.length, v2.xsdt_address),
        (2, 36, 0x2_0000_0000)
    );
    assert_eq!(v2.root_pointer().unwrap().address, 0x2_0000_0000);

    let mut bad = fixture("q35-smp4", "RSDP.bin");
    bad[0] = b'X';
    assert_eq!(Rsdp::parse(&bad).err(), Some(AcpiError::BadSignature));
    let mut bad = fixture("q35-smp4", "RSDP.bin");
    bad[16] ^= 1;
    assert_eq!(Rsdp::parse(&bad).err(), Some(AcpiError::BadChecksum));
    let mut rev1 = fixture("q35-smp4", "RSDP.bin");
    rev1[15] = 1;
    fix_rsdp(&mut rev1);
    assert_eq!(
        Rsdp::parse(&rev1).err(),
        Some(AcpiError::UnsupportedRevision)
    );
    assert_eq!(
        Rsdp::parse(&rsdp_v2(1, 2)[..19]).err(),
        Some(AcpiError::Truncated)
    );
    assert_eq!(
        Rsdp::parse(&rsdp_v2(1, 2)[..20]).err(),
        Some(AcpiError::Truncated)
    );
    assert_eq!(
        Rsdp::parse(&rsdp_v2(1, 2)[..30]).err(),
        Some(AcpiError::Truncated)
    );

    // Extended checksum covers bytes 20..36: first checksum still valid.
    let mut ext = rsdp_v2(0x1000, 0x2000);
    ext[30] ^= 0x10;
    assert_eq!(Rsdp::parse(&ext).err(), Some(AcpiError::BadChecksum));
    let mut short = rsdp_v2(0x1000, 0x2000);
    put32(&mut short, 20, 24);
    fix_rsdp(&mut short);
    assert_eq!(Rsdp::parse(&short).err(), Some(AcpiError::BadLength));
    // A future revision is accepted as long as its extended layout holds.
    let mut rev3 = rsdp_v2(0x1000, 0x2000);
    rev3[15] = 3;
    fix_rsdp(&mut rev3);
    assert_eq!(
        Rsdp::parse(&rev3).unwrap().root_pointer().unwrap().kind,
        RootKind::Xsdt
    );
}

// ---------------------------------------------------------------- MADT

fn madt(entries: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    for entry in entries {
        body.extend_from_slice(entry);
    }
    sdt(b"APIC", 5, &body)
}

#[test]
fn madt_decodes_every_known_entry_type() {
    let table = madt(&[
        // Local APIC uid 1, id 2, online-capable but not enabled.
        &[0, 8, 1, 2, 2, 0, 0, 0],
        // NMI source: flags level/active-low (0xf), GSI 7.
        &[3, 8, 0xf, 0, 7, 0, 0, 0],
        // Local APIC address override.
        &[5, 12, 0, 0, 0, 0, 0xe0, 0xfe, 1, 0, 0, 0],
        // Local x2APIC: id 0x100, enabled, uid 0x20.
        &[9, 16, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0x20, 0, 0, 0],
        // Local x2APIC NMI: all processors, LINT1.
        &[0xa, 12, 5, 0, 0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0],
        // Unknown type 0x11 (e.g. a future controller), 5 bytes.
        &[0x11, 5, 9, 9, 9],
        // Local APIC entry longer than its minimum: accepted.
        &[0, 10, 3, 4, 1, 0, 0, 0, 0xee, 0xee],
    ]);
    let madt = Madt::parse(&table).unwrap();
    assert_eq!(madt.local_apic_address(), 0xfee0_0000);
    assert_eq!(madt.effective_local_apic_address(), 0x1_fee0_0000);
    let entries: Vec<_> = madt.entries().collect();
    assert_eq!(entries.len(), 7);
    let MadtEntry::LocalApic(lapic) = entries[0] else {
        panic!()
    };
    assert!(!lapic.enabled() && lapic.online_capable());
    let MadtEntry::NmiSource(nmi) = entries[1] else {
        panic!()
    };
    assert_eq!((nmi.gsi, nmi.flags.trigger_mode()), (7, TriggerMode::Level));
    let MadtEntry::LocalX2Apic(x2) = entries[3] else {
        panic!()
    };
    assert_eq!(
        (x2.x2apic_id, x2.processor_uid, x2.enabled()),
        (0x100, 0x20, true)
    );
    let MadtEntry::X2ApicNmi(x2nmi) = entries[4] else {
        panic!()
    };
    assert_eq!((x2nmi.processor_uid, x2nmi.lint), (0xffff_ffff, 1));
    assert_eq!(
        entries[5],
        MadtEntry::Unknown {
            entry_type: 0x11,
            bytes: &[0x11, 5, 9, 9, 9]
        }
    );
    let MadtEntry::LocalApic(long) = entries[6] else {
        panic!()
    };
    assert_eq!((long.processor_uid, long.apic_id), (3, 4));
}

#[test]
fn madt_record_rules() {
    // Entry length 0.
    let zero = patched("q35-smp4", "APIC.bin", |b| b[45] = 0);
    assert_eq!(Madt::parse(&zero).err(), Some(AcpiError::ZeroLengthRecord));
    // Local APIC entry shorter than 8 bytes.
    let short = patched("q35-smp4", "APIC.bin", |b| b[45] = 6);
    assert_eq!(Madt::parse(&short).err(), Some(AcpiError::ShortRecord));
    // Unknown type with a length that cannot hold its own header.
    assert_eq!(
        Madt::parse(&madt(&[&[0x7f, 1]])).err(),
        Some(AcpiError::ShortRecord)
    );
    // Last entry (LAPIC NMI at 138) claiming 8 bytes in a 144-byte table.
    let over = patched("q35-smp4", "APIC.bin", |b| b[139] = 8);
    assert_eq!(Madt::parse(&over).err(), Some(AcpiError::RecordOverrun));
    // A lone trailing byte cannot even hold the length field.
    assert_eq!(
        Madt::parse(&madt(&[&[0, 8, 0, 0, 1, 0, 0, 0], &[0]])).err(),
        Some(AcpiError::RecordOverrun)
    );
    // Two address overrides.
    let ovr: &[u8] = &[5, 12, 0, 0, 0, 0, 0xe0, 0xfe, 0, 0, 0, 0];
    assert_eq!(
        Madt::parse(&madt(&[ovr, ovr])).err(),
        Some(AcpiError::InvalidField)
    );
    // Table too short for the controller address and flags.
    assert_eq!(
        Madt::parse(&sdt(b"APIC", 3, &[0; 4])).err(),
        Some(AcpiError::BadLength)
    );
    // An empty entry list is valid.
    assert_eq!(Madt::parse(&madt(&[])).unwrap().entries().count(), 0);
}

// ---------------------------------------------------------------- MCFG

fn mcfg(entries: &[(u64, u16, u8, u8)]) -> Vec<u8> {
    let mut body = vec![0u8; 8];
    for &(base, segment, start, end) in entries {
        body.extend_from_slice(&base.to_le_bytes());
        body.extend_from_slice(&segment.to_le_bytes());
        body.extend_from_slice(&[start, end, 0, 0, 0, 0]);
    }
    sdt(b"MCFG", 1, &body)
}

#[test]
fn mcfg_rules() {
    let inverted = patched("lenovo-82k8", "MCFG.bin", |b| b[54] = 0x40);
    assert_eq!(Mcfg::parse(&inverted).err(), Some(AcpiError::InvalidField));
    let mut partial = mcfg(&[(0xe000_0000, 0, 0, 255)]);
    partial.truncate(partial.len() - 4);
    let len = partial.len() as u32;
    put32(&mut partial, 4, len);
    fix_checksum(&mut partial);
    assert_eq!(Mcfg::parse(&partial).err(), Some(AcpiError::BadLength));
    let overlap = mcfg(&[(0xe000_0000, 0, 0, 0x3f), (0xf000_0000, 0, 0x3f, 0x80)]);
    assert_eq!(Mcfg::parse(&overlap).err(), Some(AcpiError::InvalidField));
    let wrap = mcfg(&[(u64::MAX - 0xf_ffff, 0, 0, 0)]);
    assert_eq!(Mcfg::parse(&wrap).err(), Some(AcpiError::AddressOverflow));
    let beyond = mcfg(&[(PHYS_LIMIT - (1 << 20), 0, 0, 1)]);
    assert_eq!(Mcfg::parse(&beyond).err(), Some(AcpiError::AddressOverflow));

    // Same buses on different segments and adjacent ranges are fine.
    let ok = mcfg(&[
        (0xe000_0000, 0, 0, 0x3f),
        (0xd000_0000, 0, 0x40, 0x7f),
        (0xc000_0000, 1, 0, 0x3f),
    ]);
    let parsed = Mcfg::parse(&ok).unwrap();
    assert_eq!(parsed.len(), 3);
    assert_eq!(
        parsed.segments().map(|s| s.segment).collect::<Vec<_>>(),
        [0, 0, 1]
    );
    assert!(Mcfg::parse(&mcfg(&[])).unwrap().is_empty());
}

// ---------------------------------------------------------------- HPET

#[test]
fn hpet_rules() {
    let mut short = fixture("q35-smp4", "HPET.bin");
    short.truncate(52);
    put32(&mut short, 4, 52);
    fix_checksum(&mut short);
    assert_eq!(Hpet::parse(&short).err(), Some(AcpiError::BadLength));
    let io = patched("q35-smp4", "HPET.bin", |b| b[40] = Gas::SYSTEM_IO);
    assert_eq!(Hpet::parse(&io).err(), Some(AcpiError::InvalidField));
    let null = patched("q35-smp4", "HPET.bin", |b| put64(b, 44, 0));
    assert_eq!(Hpet::parse(&null).err(), Some(AcpiError::InvalidField));
}

// ---------------------------------------------------------------- FADT

#[test]
fn fadt_revision_and_length_rules() {
    for revision in [0, 7, 0xff] {
        let bytes = patched("q35-smp4", "FACP.bin", |b| b[8] = revision);
        assert_eq!(
            Fadt::parse(&bytes).err(),
            Some(AcpiError::UnsupportedRevision)
        );
    }
    let mut short = fixture("q35-smp4", "FACP.bin");
    short.truncate(100);
    put32(&mut short, 4, 100);
    fix_checksum(&mut short);
    assert_eq!(Fadt::parse(&short).err(), Some(AcpiError::BadLength));
}

/// An ACPI 1.0 FADT: 116 bytes, revision 1, 32-bit fields only.
fn fadt_v1() -> Vec<u8> {
    let mut body = vec![0u8; 116 - 36];
    let at = |offset: usize| offset - 36;
    put32(&mut body, at(36), 0x7fe0_0000); // FIRMWARE_CTRL
    put32(&mut body, at(40), 0x7fd0_0000); // DSDT
    put16(&mut body, at(46), 9); // SCI_INT
    put32(&mut body, at(56), 0x400); // PM1a_EVT_BLK
    put32(&mut body, at(64), 0x404); // PM1a_CNT_BLK
    put32(&mut body, at(76), 0x408); // PM_TMR_BLK
    put32(&mut body, at(80), 0x420); // GPE0_BLK
    body[at(88)] = 4; // PM1_EVT_LEN
    body[at(89)] = 2; // PM1_CNT_LEN
    body[at(91)] = 4; // PM_TMR_LEN
    body[at(92)] = 8; // GPE0_BLK_LEN
    put32(&mut body, at(112), (1 << 10) | (1 << 8)); // RESET_REG_SUP | TMR_VAL_EXT
    sdt(b"FACP", 1, &body)
}

#[test]
fn short_acpi1_fadt_falls_back_to_32bit_fields() {
    let bytes = fadt_v1();
    let fadt = Fadt::parse(&bytes).unwrap();
    assert_eq!(fadt.revision(), 1);
    assert_eq!(fadt.dsdt(), Some(0x7fd0_0000));
    assert_eq!(fadt.firmware_ctrl(), Some(0x7fe0_0000));
    let timer = fadt.pm_timer().unwrap();
    assert_eq!(
        timer.register,
        Gas {
            address_space: Gas::SYSTEM_IO,
            bit_width: 32,
            bit_offset: 0,
            access_size: 0,
            address: 0x408
        }
    );
    assert!(timer.counter_is_32bit);
    assert_eq!(fadt.pm1a_control_block().unwrap().bit_width, 16);
    assert_eq!(fadt.gpe0_block().unwrap().bit_width, 64);
    // The flag is set but a 116-byte table has no RESET_REG.
    assert_eq!(fadt.reset_register(), None);
    assert_eq!(fadt.minor_version(), None);
    assert_eq!(fadt.sleep_control_register(), None);
    assert_eq!(fadt.hypervisor_vendor_id(), None);
    assert_eq!(fadt.pm1b_event_block(), None);
}

#[test]
fn acpi1b_fadt_reaches_the_reset_register_only() {
    // 129 bytes: ACPI 1.0b added RESET_REG and RESET_VALUE.
    let mut bytes = fadt_v1();
    bytes.resize(129, 0);
    bytes[8] = 2;
    put32(&mut bytes, 4, 129);
    bytes[116..128].copy_from_slice(&[1, 8, 0, 1, 0xf9, 0x0c, 0, 0, 0, 0, 0, 0]);
    bytes[128] = 6;
    fix_checksum(&mut bytes);
    let fadt = Fadt::parse(&bytes).unwrap();
    let reset = fadt.reset_register().unwrap();
    assert_eq!((reset.register.address, reset.value), (0xcf9, 6));
    assert_eq!(fadt.dsdt(), Some(0x7fd0_0000));
    assert_eq!(fadt.minor_version(), None);
}

#[test]
fn nonzero_x_fields_override_32bit_fields() {
    let bytes = patched("q35-smp4", "FACP.bin", |b| {
        put32(b, 40, 0x0123_4000); // DSDT
        put64(b, 140, 0x1_0000_0000); // X_DSDT
        put64(b, 132, 0x2_0000_0000); // X_FIRMWARE_CTRL
        put64(b, 212, 0); // X_PM_TMR_BLK address -> use PM_TMR_BLK
        b[113] &= !(1 << 2); // clear RESET_REG_SUP (flags bit 10)
    });
    let fadt = Fadt::parse(&bytes).unwrap();
    assert_eq!(fadt.dsdt(), Some(0x1_0000_0000));
    assert_eq!(fadt.firmware_ctrl(), Some(0x2_0000_0000));
    let timer = fadt.pm_timer().unwrap().register;
    // Legacy fallback describes the 4-byte I/O block itself.
    assert_eq!(
        (timer.address, timer.bit_width, timer.access_size),
        (0x608, 32, 0)
    );
    assert_eq!(fadt.reset_register(), None);

    // Both address forms zero: the block is absent.
    let none = patched("q35-smp4", "FACP.bin", |b| {
        put32(b, 76, 0);
        put64(b, 212, 0);
    });
    assert_eq!(Fadt::parse(&none).unwrap().pm_timer(), None);
}

// ---------------------------------------------------------------- DMAR

fn dmar(structures: &[Vec<u8>]) -> Vec<u8> {
    let mut body = vec![0u8; 12];
    body[0] = 45; // host address width 46
    body[1] = 0b11; // INTR_REMAP | X2APIC_OPT_OUT
    for s in structures {
        body.extend_from_slice(s);
    }
    sdt(b"DMAR", 1, &body)
}

fn structure(structure_type: u16, fixed: &[u8], scopes: &[&[u8]]) -> Vec<u8> {
    let mut s = Vec::new();
    s.extend_from_slice(&structure_type.to_le_bytes());
    let len = 4 + fixed.len() + scopes.iter().map(|x| x.len()).sum::<usize>();
    s.extend_from_slice(&(len as u16).to_le_bytes());
    s.extend_from_slice(fixed);
    for scope in scopes {
        s.extend_from_slice(scope);
    }
    s
}

fn drhd(flags: u8, base: u64, scopes: &[&[u8]]) -> Vec<u8> {
    let mut fixed = vec![flags, 0, 0, 0];
    fixed.extend_from_slice(&base.to_le_bytes());
    structure(0, &fixed, scopes)
}

const ENDPOINT_0_2: &[u8] = &[SCOPE_PCI_ENDPOINT, 8, 0, 0, 0, 0, 2, 0];

#[test]
fn dmar_decodes_all_structure_types() {
    let mut rmrr = vec![0, 0, 0, 0];
    rmrr.extend_from_slice(&0x7c00_0000u64.to_le_bytes());
    rmrr.extend_from_slice(&0x7c0f_ffffu64.to_le_bytes());
    let mut rhsa = vec![0; 4];
    rhsa.extend_from_slice(&0xfed9_1000u64.to_le_bytes());
    rhsa.extend_from_slice(&1u32.to_le_bytes());
    let mut andd = vec![0, 0, 0, 7];
    andd.extend_from_slice(b"\\_SB.PCI0.I2C0\0");
    let table = dmar(&[
        drhd(
            0,
            0xfed9_0000,
            &[
                &[SCOPE_PCI_SUB_HIERARCHY, 10, 0, 0, 0, 0, 0x1c, 0, 0, 0],
                ENDPOINT_0_2,
            ],
        ),
        drhd(1, 0xfed9_1000, &[]),
        structure(1, &rmrr, &[ENDPOINT_0_2]),
        structure(2, &[1, 0, 0, 0], &[]),
        structure(3, &rhsa, &[]),
        structure(4, &andd, &[]),
        structure(5, &[0, 0, 0, 0], &[]), // SATC: not decoded
    ]);
    let dmar = Dmar::parse(&table).unwrap();
    assert_eq!(dmar.host_address_width(), 46);
    assert!(dmar.interrupt_remapping() && dmar.x2apic_opt_out());
    let s: Vec<_> = dmar.structures().collect();
    assert_eq!(s.len(), 7);
    let DmarStructure::Drhd(first) = &s[0] else {
        panic!()
    };
    let scopes: Vec<_> = first.scopes.clone().collect();
    assert_eq!(scopes.len(), 2);
    assert_eq!(scopes[0].scope_type, SCOPE_PCI_SUB_HIERARCHY);
    assert_eq!(scopes[0].path.len(), 2);
    assert_eq!(
        scopes[0].path.collect::<Vec<_>>(),
        [
            PciPathEntry {
                device: 0x1c,
                function: 0
            },
            PciPathEntry {
                device: 0,
                function: 0
            }
        ]
    );
    let DmarStructure::Drhd(all) = &s[1] else {
        panic!()
    };
    assert!(all.include_pci_all() && all.scopes.clone().next().is_none());
    let DmarStructure::Rmrr(rmrr) = &s[2] else {
        panic!()
    };
    assert_eq!(
        (rmrr.base_address, rmrr.limit_address),
        (0x7c00_0000, 0x7c0f_ffff)
    );
    assert_eq!(rmrr.scopes.clone().count(), 1);
    let DmarStructure::Atsr(atsr) = &s[3] else {
        panic!()
    };
    assert!(atsr.all_ports());
    let DmarStructure::Rhsa(rhsa) = &s[4] else {
        panic!()
    };
    assert_eq!(
        (rhsa.register_base, rhsa.proximity_domain),
        (0xfed9_1000, 1)
    );
    let DmarStructure::Andd(andd) = &s[5] else {
        panic!()
    };
    assert_eq!(
        (andd.device_number, andd.object_name),
        (7, &b"\\_SB.PCI0.I2C0"[..])
    );
    assert!(matches!(
        s[6],
        DmarStructure::Unknown {
            structure_type: 5,
            ..
        }
    ));
}

#[test]
fn dmar_record_rules() {
    let err = |structures: &[Vec<u8>]| Dmar::parse(&dmar(structures)).err();
    let mut zero = drhd(0, 0xfed9_0000, &[]);
    zero[2] = 0;
    assert_eq!(err(&[zero]), Some(AcpiError::ZeroLengthRecord));
    let mut short = drhd(0, 0xfed9_0000, &[]);
    short[2] = 12;
    assert_eq!(err(&[short]), Some(AcpiError::ShortRecord));
    let mut over = drhd(0, 0xfed9_0000, &[]);
    over[2] = 24;
    assert_eq!(err(&[over]), Some(AcpiError::RecordOverrun));
    assert_eq!(err(&[vec![0, 0]]), Some(AcpiError::RecordOverrun));
    // Device scope rules.
    let scope = |bytes: &[u8]| err(&[drhd(0, 0xfed9_0000, &[bytes])]);
    assert_eq!(
        scope(&[1, 0, 0, 0, 0, 0, 0, 0]),
        Some(AcpiError::ZeroLengthRecord)
    );
    assert_eq!(scope(&[1, 6, 0, 0, 0, 0]), Some(AcpiError::ShortRecord));
    assert_eq!(
        scope(&[1, 9, 0, 0, 0, 0, 2, 0, 0]),
        Some(AcpiError::BadLength)
    );
    assert_eq!(
        scope(&[1, 10, 0, 0, 0, 0, 2, 0]),
        Some(AcpiError::RecordOverrun)
    );
    assert_eq!(
        scope(&[1, 8, 0, 0, 0, 0, 32, 0]),
        Some(AcpiError::InvalidField)
    );
    assert_eq!(
        scope(&[1, 8, 0, 0, 0, 0, 0, 8]),
        Some(AcpiError::InvalidField)
    );
    // RMRR whose base lies above its limit.
    let mut rmrr = vec![0, 0, 0, 0];
    rmrr.extend_from_slice(&0x8000u64.to_le_bytes());
    rmrr.extend_from_slice(&0x7fffu64.to_le_bytes());
    assert_eq!(
        err(&[structure(1, &rmrr, &[])]),
        Some(AcpiError::InvalidField)
    );
    // The captured table with its first scope length zeroed.
    let captured = patched("q35-smp4-intel-iommu", "DMAR.bin", |b| b[0x41] = 0);
    assert_eq!(
        Dmar::parse(&captured).err(),
        Some(AcpiError::ZeroLengthRecord)
    );
    // Header without the host address width and reserved bytes.
    assert_eq!(
        Dmar::parse(&sdt(b"DMAR", 1, &[0; 8])).err(),
        Some(AcpiError::BadLength)
    );
}

// ---------------------------------------------------------------- IVRS

fn ivrs(revision: u8, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = vec![0u8; 12];
    body[..4].copy_from_slice(&0x0020_3043u32.to_le_bytes());
    for b in blocks {
        body.extend_from_slice(b);
    }
    sdt(b"IVRS", revision, &body)
}

fn ivhd(block_type: u8, entries: &[&[u8]]) -> Vec<u8> {
    let header_len = if block_type == 0x10 { 24 } else { 40 };
    let len = header_len + entries.iter().map(|e| e.len()).sum::<usize>();
    let mut block = vec![0u8; header_len];
    block[0] = block_type;
    block[1] = 0xb0;
    block[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    block[4..6].copy_from_slice(&2u16.to_le_bytes());
    block[8..16].copy_from_slice(&0xfdd0_0000u64.to_le_bytes());
    for entry in entries {
        block.extend_from_slice(entry);
    }
    block
}

fn ivmd(block_type: u8, device: u16, aux: u16, start: u64, length: u64) -> Vec<u8> {
    let mut block = vec![block_type, 1, 32, 0];
    block.extend_from_slice(&device.to_le_bytes());
    block.extend_from_slice(&aux.to_le_bytes());
    block.extend_from_slice(&[0; 8]);
    block.extend_from_slice(&start.to_le_bytes());
    block.extend_from_slice(&length.to_le_bytes());
    block
}

#[test]
fn ivrs_decodes_ivmd_and_every_entry_class() {
    let table = ivrs(
        2,
        &[
            ivhd(
                0x40,
                &[
                    &[1, 0, 0, 0xd0],                   // All
                    &[0x42, 0x10, 0, 0, 0, 0x08, 0, 0], // Alias select 0x10 -> 0x08
                    &[0x46, 0x20, 0, 0, 1, 0, 0, 0],    // Extended select
                    &[0x47, 0x30, 0, 0, 1, 0, 0, 0],    // Extended start range
                    &[4, 0x38, 0, 0],                   // End range
                    &[5, 0, 0, 0],                      // Unknown 4-byte class
                    &[0x80; 16],                        // Unknown 16-byte class
                    &[0xc0; 32],                        // Unknown 32-byte class
                    &[
                        0xf0, 0xa5, 0, 0x40, b'A', b'M', b'D', b'I', b'0', b'0', b'2', b'0', 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0,
                    ], // ACPI HID, no UID
                ],
            ),
            ivmd(IVMD_ALL, 0, 0, 0x9_0000, 0x1000),
            ivmd(IVMD_SELECT, 0x00a0, 0, 0xa_0000, 0x2000),
            ivmd(IVMD_RANGE, 0x0100, 0x01ff, 0xb_0000, 0x3000),
            vec![0x50, 0, 8, 0, 1, 2, 3, 4], // unknown block type
        ],
    );
    let ivrs = Ivrs::parse(&table).unwrap();
    let blocks: Vec<_> = ivrs.blocks().collect();
    assert_eq!(blocks.len(), 5);
    let IvrsBlock::Ivhd(ivhd) = &blocks[0] else {
        panic!()
    };
    let entries: Vec<_> = ivhd.entries.collect();
    assert_eq!(entries.len(), 9);
    assert_eq!(entries[0], IvhdEntry::All { data: 0xd0 });
    assert_eq!(
        entries[1],
        IvhdEntry::AliasSelect {
            device_id: 0x10,
            data: 0,
            source_id: 0x08
        }
    );
    assert_eq!(
        entries[2],
        IvhdEntry::ExtendedSelect {
            device_id: 0x20,
            data: 0,
            extended: 1
        }
    );
    assert!(matches!(entries[5], IvhdEntry::Unknown { entry_type: 5, bytes } if bytes.len() == 4));
    assert!(
        matches!(entries[6], IvhdEntry::Unknown { entry_type: 0x80, bytes } if bytes.len() == 16)
    );
    assert!(
        matches!(entries[7], IvhdEntry::Unknown { entry_type: 0xc0, bytes } if bytes.len() == 32)
    );
    assert!(matches!(
        entries[8],
        IvhdEntry::AcpiHid {
            uid: &[],
            uid_format: 0,
            ..
        }
    ));
    assert_eq!(
        blocks[3],
        IvrsBlock::Ivmd(Ivmd {
            block_type: IVMD_RANGE,
            flags: 1,
            device_id: 0x100,
            auxiliary_data: 0x1ff,
            start_address: 0xb_0000,
            memory_length: 0x3000,
        })
    );
    assert!(matches!(
        blocks[4],
        IvrsBlock::Unknown {
            block_type: 0x50,
            ..
        }
    ));
}

#[test]
fn ivrs_record_rules() {
    let err = |blocks: &[Vec<u8>]| Ivrs::parse(&ivrs(2, blocks)).err();
    for revision in [0, 3] {
        let bytes = patched("q35-smp4-amd-iommu", "IVRS.bin", |b| b[8] = revision);
        assert_eq!(
            Ivrs::parse(&bytes).err(),
            Some(AcpiError::UnsupportedRevision)
        );
    }
    let zero = patched("q35-smp4-amd-iommu", "IVRS.bin", |b| put16(b, 0x32, 0));
    assert_eq!(Ivrs::parse(&zero).err(), Some(AcpiError::ZeroLengthRecord));
    let mut short = ivhd(0x11, &[]);
    short[2] = 30;
    assert_eq!(err(&[short]), Some(AcpiError::ShortRecord));
    let mut over = ivhd(0x10, &[]);
    over[2] = 40;
    assert_eq!(err(&[over]), Some(AcpiError::RecordOverrun));
    assert_eq!(err(&[vec![0x10, 0]]), Some(AcpiError::RecordOverrun));
    let mut short_ivmd = ivmd(IVMD_ALL, 0, 0, 0, 0);
    short_ivmd[2] = 16;
    assert_eq!(err(&[short_ivmd]), Some(AcpiError::ShortRecord));
    assert_eq!(
        err(&[ivmd(IVMD_SELECT, 1, 0, u64::MAX, 2)]),
        Some(AcpiError::AddressOverflow)
    );
    assert_eq!(
        err(&[ivmd(IVMD_RANGE, 9, 8, 0, 0x1000)]),
        Some(AcpiError::InvalidField)
    );
    // Device entries overrunning their IVHD.
    let entry = |entries: &[&[u8]]| err(&[ivhd(0x10, entries)]);
    assert_eq!(entry(&[&[0x42, 0, 0, 0]]), Some(AcpiError::RecordOverrun));
    assert_eq!(
        entry(&[&[0xf0, 0, 0, 0, 0, 0]]),
        Some(AcpiError::RecordOverrun)
    );
    let mut hid = vec![0xf0, 0, 0, 0];
    hid.extend_from_slice(&[0; 17]);
    hid.push(200); // UID length far beyond the IVHD
    assert_eq!(entry(&[&hid]), Some(AcpiError::RecordOverrun));
    // Range pairing.
    let start: &[u8] = &[3, 0x10, 0, 0];
    let end: &[u8] = &[4, 0x20, 0, 0];
    assert_eq!(entry(&[end]), Some(AcpiError::InvalidField));
    assert_eq!(entry(&[start, start, end]), Some(AcpiError::InvalidField));
    assert_eq!(entry(&[start]), Some(AcpiError::InvalidField));
    assert_eq!(
        entry(&[&[3, 0x30, 0, 0], end]),
        Some(AcpiError::InvalidField)
    );
    assert!(entry(&[start, &[2, 0x18, 0, 0], end]).is_none());
    // Header without IVinfo.
    assert_eq!(
        Ivrs::parse(&sdt(b"IVRS", 2, &[0; 4])).err(),
        Some(AcpiError::BadLength)
    );
}
