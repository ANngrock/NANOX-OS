//! Deterministic mutation loops over the captured tables. No input may
//! panic; every accepted table must iterate to completion within its bounds.
//! Most mutants get a repaired checksum so the record-level rules, not just
//! the checksum, are exercised.

mod common;

use std::collections::BTreeMap;

use common::{fix_rsdp, fixture, manifest, Memory, Rng, PROFILES};
use hw_acpi::{AcpiTables, Dmar, Fadt, Hpet, Ivrs, Madt, Mcfg, Sdt};

const ITERATIONS: usize = 4000;

/// Repair the checksum over the declared length (clamped to the buffer).
fn repair(table: &mut [u8]) {
    if table.len() < 10 {
        return;
    }
    let declared = u32::from_le_bytes(table[4..8].try_into().unwrap()) as usize;
    let end = declared.clamp(10, table.len());
    table[9] = 0;
    let sum = table[..end].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    table[9] = 0u8.wrapping_sub(sum);
}

fn mutate(rng: &mut Rng, original: &[u8]) -> Vec<u8> {
    let mut bytes = original.to_vec();
    let boundary = [
        0u32,
        1,
        2,
        3,
        4,
        6,
        8,
        0x7f,
        0x80,
        0xff,
        0xffff,
        0xffff_ffff,
    ];
    for _ in 0..1 + rng.below(4) {
        match rng.below(5) {
            0 => {
                let at = rng.below(bytes.len());
                bytes[at] = rng.next_u64() as u8;
            }
            1 => {
                let at = rng.below(bytes.len());
                bytes[at] = boundary[rng.below(boundary.len())] as u8;
            }
            2 if bytes.len() >= 4 => {
                let at = rng.below(bytes.len() - 3);
                let value = boundary[rng.below(boundary.len())];
                bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            3 => {
                let len = rng.below(bytes.len() + 32);
                bytes.resize(len.max(1), rng.next_u64() as u8);
            }
            _ if bytes.len() >= 8 => {
                // Keep the buffer, lie about the table length.
                let declared = rng.below(bytes.len() + 64) as u32;
                bytes[4..8].copy_from_slice(&declared.to_le_bytes());
            }
            _ => {}
        }
    }
    if rng.below(8) != 0 {
        repair(&mut bytes);
    }
    bytes
}

/// Run the parser matching the signature and walk everything it exposes.
/// Returns whether the typed parser accepted the table.
fn exercise(bytes: &[u8]) -> bool {
    let Ok(sdt) = Sdt::parse(bytes) else {
        return false;
    };
    let len = sdt.bytes().len();
    assert!(len >= 36 && len <= bytes.len());
    match &sdt.signature() {
        b"APIC" => Madt::from_sdt(sdt).is_ok_and(|madt| {
            let _ = madt.effective_local_apic_address();
            // Every entry is at least 2 bytes.
            madt.entries().count() <= (len - 44) / 2
        }),
        b"MCFG" => Mcfg::from_sdt(sdt).is_ok_and(|mcfg| {
            mcfg.segments()
                .all(|s| s.start_bus <= s.end_bus && s.bus_count() >= 1)
                && mcfg.segments().count() == mcfg.len()
        }),
        b"HPET" => Hpet::from_sdt(sdt).is_ok_and(|hpet| hpet.comparator_count() >= 1),
        b"FACP" => Fadt::from_sdt(sdt).is_ok_and(|fadt| {
            let _ = (
                fadt.dsdt(),
                fadt.firmware_ctrl(),
                fadt.pm_timer(),
                fadt.reset_register(),
            );
            let _ = (
                fadt.pm1a_event_block(),
                fadt.gpe0_block(),
                fadt.gpe1_block(),
            );
            let _ = (fadt.sleep_control_register(), fadt.hypervisor_vendor_id());
            let _ = (fadt.minor_version(), fadt.flags(), fadt.iapc_boot_arch());
            true
        }),
        b"DMAR" => Dmar::from_sdt(sdt).is_ok_and(|dmar| {
            let mut count = 0;
            for structure in dmar.structures() {
                count += 1;
                if let hw_acpi::dmar::DmarStructure::Drhd(drhd) = structure {
                    for scope in drhd.scopes {
                        assert!(scope
                            .path
                            .clone()
                            .all(|hop| hop.device < 32 && hop.function < 8));
                        assert!(!scope.path.is_empty());
                    }
                }
            }
            count <= (len - 48) / 4
        }),
        b"IVRS" => Ivrs::from_sdt(sdt).is_ok_and(|ivrs| {
            let mut count = 0;
            for block in ivrs.blocks() {
                count += 1;
                if let hw_acpi::ivrs::IvrsBlock::Ivhd(ivhd) = block {
                    assert!(ivhd.entries.count() <= len / 4);
                }
            }
            count <= (len - 48) / 4
        }),
        _ => true,
    }
}

#[test]
fn mutated_fixture_tables_never_panic() {
    let mut rng = Rng::new(0x4e41_4e4f_5841_4350);
    // (accepted, rejected) per signature.
    let mut outcome: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for profile in PROFILES {
        for table in manifest(profile) {
            if ["RSDP", "FACS", "DSDT", "RSDT", "WAET"].contains(&table.signature.as_str()) {
                continue;
            }
            let original = fixture(profile, &table.file);
            assert!(exercise(&original), "{profile}/{} must parse", table.file);
            for _ in 0..ITERATIONS {
                let mutant = mutate(&mut rng, &original);
                let entry = outcome.entry(table.signature.clone()).or_default();
                if exercise(&mutant) {
                    entry.0 += 1;
                } else {
                    entry.1 += 1;
                }
            }
        }
    }
    println!("accepted/rejected per signature: {outcome:?}");
    // Both outcomes must occur for every parser, otherwise the loop is not
    // reaching past the checksum or never producing a bad record.
    for signature in ["APIC", "FACP", "HPET", "MCFG", "DMAR", "IVRS"] {
        let (accepted, rejected) = outcome[signature];
        assert!(
            accepted > 0 && rejected > 0,
            "{signature}: {accepted}/{rejected}"
        );
    }
}

#[test]
fn random_bytes_behind_valid_headers_never_panic() {
    let mut rng = Rng::new(7);
    for signature in [b"APIC", b"MCFG", b"HPET", b"FACP", b"DMAR", b"IVRS"] {
        for _ in 0..ITERATIONS {
            let len = 36 + rng.below(300);
            let mut bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            bytes[..4].copy_from_slice(signature);
            bytes[4..8].copy_from_slice(&(len as u32).to_le_bytes());
            bytes[8] = 1 + rng.below(3) as u8;
            repair(&mut bytes);
            exercise(&bytes);
        }
    }
}

#[test]
fn mutated_root_tables_never_panic() {
    let mut rng = Rng::new(0x5244_5354);
    let mut outcomes = [0usize; 2];
    for profile in &PROFILES[..3] {
        let (memory, rsdp_phys) = Memory::from_manifest(profile);
        let rsdt_phys = 0x1f77_d000;
        let rsdp = memory
            .regions
            .iter()
            .find(|(p, _)| *p == rsdp_phys)
            .unwrap()
            .1
            .clone();
        let rsdt = memory
            .regions
            .iter()
            .find(|(p, _)| *p == rsdt_phys)
            .unwrap()
            .1
            .clone();
        for _ in 0..ITERATIONS {
            let mut mutated = memory.clone();
            if rng.below(2) == 0 {
                let mut r = rsdp.clone();
                let at = rng.below(r.len());
                r[at] = rng.next_u64() as u8;
                if rng.below(4) != 0 {
                    fix_rsdp(&mut r);
                }
                *mutated.region_mut(rsdp_phys) = r;
            } else {
                *mutated.region_mut(rsdt_phys) = mutate(&mut rng, &rsdt);
            }
            let mut root_buf = [0u8; 256];
            let Ok(tables) = AcpiTables::new(&mutated, rsdp_phys, &mut root_buf) else {
                outcomes[1] += 1;
                continue;
            };
            outcomes[0] += 1;
            for signature in [*b"APIC", *b"FACP", *b"HPET", *b"MCFG", *b"DMAR", *b"IVRS"] {
                let mut buf = [0u8; 512];
                if let Ok(Some(sdt)) = tables.find(signature, 0, &mut buf) {
                    exercise(sdt.bytes());
                }
            }
        }
    }
    println!("root walks accepted/rejected: {outcomes:?}");
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}
