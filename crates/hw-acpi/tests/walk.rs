//! RSDP -> RSDT/XSDT walks over a physical-memory model.

mod common;

use common::{fix_checksum, fix_rsdp, put32, rsdp_v0, rsdp_v2, rsdt, sdt, xsdt, Memory};
use hw_acpi::{
    load_table, read_rsdp, AcpiError, AcpiTables, Dmar, Fadt, Ivrs, Madt, PhysRead, RootKind,
    PHYS_LIMIT,
};

#[test]
fn q35_profiles_walk_from_captured_rsdp_through_rsdt() {
    for (profile, iommu) in [
        ("q35-smp4", None),
        ("q35-smp4-intel-iommu", Some(*b"DMAR")),
        ("q35-smp4-amd-iommu", Some(*b"IVRS")),
    ] {
        let (memory, rsdp_phys) = Memory::from_manifest(profile);
        assert_eq!(rsdp_phys, 0x1f77_e000);
        let mut root_buf = [0u8; 256];
        let tables = AcpiTables::new(&memory, rsdp_phys, &mut root_buf).unwrap();
        assert_eq!(tables.rsdp().revision, 0);
        assert_eq!(&tables.rsdp().oem_id, b"BOCHS ");
        assert_eq!(tables.root().kind(), RootKind::Rsdt);
        assert_eq!(tables.root_address(), 0x1f77_d000);
        assert_eq!(tables.root().len(), if iommu.is_some() { 6 } else { 5 });

        let mut buf = [0u8; 4096];
        let apic = tables.find(*b"APIC", 0, &mut buf).unwrap().unwrap();
        let madt = Madt::from_sdt(apic).unwrap();
        assert_eq!(madt.entries().count(), 11);
        assert_eq!(tables.find_address(*b"APIC", 0).unwrap(), Some(0x1f77_8000));
        assert!(tables.find(*b"APIC", 1, &mut buf).unwrap().is_none());
        assert!(tables.find(*b"SSDT", 0, &mut buf).unwrap().is_none());
        assert_eq!(tables.count(*b"WAET").unwrap(), 1);

        let facp = tables.find(*b"FACP", 0, &mut buf).unwrap().unwrap();
        let dsdt_phys = Fadt::from_sdt(facp).unwrap().dsdt().unwrap();
        let mut dsdt_buf = vec![0u8; 16 * 1024];
        let dsdt = tables.load(dsdt_phys, &mut dsdt_buf).unwrap();
        assert_eq!(&dsdt.signature(), b"DSDT");
        assert!(dsdt.bytes().len() > 8000);

        match iommu {
            Some(signature) => {
                let table = tables.find(signature, 0, &mut buf).unwrap().unwrap();
                if &signature == b"DMAR" {
                    assert!(Dmar::from_sdt(table).unwrap().structures().count() == 1);
                } else {
                    assert!(Ivrs::from_sdt(table).unwrap().blocks().count() == 2);
                }
            }
            None => {
                assert_eq!(tables.count(*b"DMAR").unwrap(), 0);
                assert_eq!(tables.count(*b"IVRS").unwrap(), 0);
            }
        }
    }
}

#[test]
fn table_buffer_must_hold_the_whole_table() {
    let (memory, rsdp_phys) = Memory::from_manifest("q35-smp4");
    let mut root_buf = [0u8; 256];
    let tables = AcpiTables::new(&memory, rsdp_phys, &mut root_buf).unwrap();
    let mut small = [0u8; 100];
    assert_eq!(
        tables.find(*b"APIC", 0, &mut small).unwrap_err(),
        AcpiError::BufferTooSmall
    );
    let mut tiny = [0u8; 8];
    assert_eq!(
        AcpiTables::new(&memory, rsdp_phys, &mut tiny).err(),
        Some(AcpiError::BufferTooSmall)
    );
}

const RSDP: u64 = 0xe0000;
const RSDT: u64 = 0x10_0000;
const XSDT: u64 = 0x20_0000;
const TABLE_A: u64 = 0x30_0000;
const TABLE_B: u64 = 0x40_0000;
const TABLE_C: u64 = 0x50_0000;

fn walk(memory: &Memory) -> Result<Vec<u64>, AcpiError> {
    let mut root_buf = [0u8; 512];
    let tables = AcpiTables::new(memory, RSDP, &mut root_buf)?;
    let mut found = Vec::new();
    for signature in [*b"SSDT", *b"APIC"] {
        let mut index = 0;
        while let Some(address) = tables.find_address(signature, index)? {
            found.push(address);
            index += 1;
        }
    }
    Ok(found)
}

fn synthetic(rsdp: Vec<u8>, root_phys: u64, root: Vec<u8>, entries: &[u64]) -> Memory {
    let mut memory = Memory::default();
    memory.insert(RSDP, rsdp);
    memory.insert(root_phys, root);
    for (n, &phys) in entries.iter().enumerate() {
        let signature = if n == 0 { b"APIC" } else { b"SSDT" };
        memory.insert(phys, sdt(signature, 1, &[n as u8; 8]));
    }
    memory
}

#[test]
fn xsdt_wins_over_rsdt_when_revision2_and_nonzero() {
    let entries = [TABLE_A, TABLE_B, TABLE_C];
    let mut memory = synthetic(rsdp_v2(RSDT as u32, XSDT), XSDT, xsdt(&entries), &entries);
    // A decoy RSDT listing a single table; it must not be used.
    memory.insert(RSDT, rsdt(&[TABLE_A as u32]));
    let mut root_buf = [0u8; 512];
    let tables = AcpiTables::new(&memory, RSDP, &mut root_buf).unwrap();
    assert_eq!(tables.root().kind(), RootKind::Xsdt);
    assert_eq!(tables.root().entries().collect::<Vec<_>>(), entries);
    // Repeated signatures are found by index in root-table order.
    assert_eq!(walk(&memory).unwrap(), [TABLE_B, TABLE_C, TABLE_A]);
    let mut buf = [0u8; 64];
    let second = tables.find(*b"SSDT", 1, &mut buf).unwrap().unwrap();
    assert_eq!(&second.body(), &[2u8; 8]);

    // Revision 2 with XSDT address 0 falls back to the RSDT.
    memory.regions[0].1 = rsdp_v2(RSDT as u32, 0);
    let mut root_buf = [0u8; 512];
    let tables = AcpiTables::new(&memory, RSDP, &mut root_buf).unwrap();
    assert_eq!(tables.root().kind(), RootKind::Rsdt);
    assert_eq!(tables.root().len(), 1);

    // Revision 0 ignores the (absent) XSDT field entirely.
    memory.regions[0].1 = rsdp_v0(RSDT as u32);
    let mut root_buf = [0u8; 512];
    let tables = AcpiTables::new(&memory, RSDP, &mut root_buf).unwrap();
    assert_eq!(tables.root().kind(), RootKind::Rsdt);
}

#[test]
fn rsdt_entries_are_widened_to_64_bits() {
    let entries = [TABLE_A, TABLE_B];
    let root = rsdt(&[TABLE_A as u32, TABLE_B as u32]);
    let memory = synthetic(rsdp_v0(RSDT as u32), RSDT, root, &entries);
    assert_eq!(walk(&memory).unwrap(), [TABLE_B, TABLE_A]);
}

#[test]
fn duplicate_root_entries_are_rejected() {
    let entries = [TABLE_A, TABLE_B, TABLE_A];
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&entries), &entries[..2]);
    assert_eq!(walk(&memory), Err(AcpiError::DuplicatePointer));
}

#[test]
fn pointers_back_into_the_rsdp_or_root_table_are_cycles() {
    // Entry pointing at the XSDT itself.
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[TABLE_A, XSDT]), &[TABLE_A]);
    assert_eq!(walk(&memory), Err(AcpiError::PointerCycle));
    // Entry pointing into the middle of the XSDT.
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[XSDT + 8]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::PointerCycle));
    // Entry pointing at the RSDP.
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[RSDP]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::PointerCycle));
    // RSDP naming itself as the root table.
    let memory = synthetic(rsdp_v2(0, RSDP), XSDT, xsdt(&[]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::PointerCycle));
    // A second copy of an XSDT listed as a table: the walk would recurse.
    let mut memory = synthetic(
        rsdp_v2(0, XSDT),
        XSDT,
        xsdt(&[TABLE_A, TABLE_B]),
        &[TABLE_A],
    );
    memory.insert(TABLE_B, xsdt(&[TABLE_A]));
    assert_eq!(walk(&memory), Err(AcpiError::PointerCycle));
}

#[test]
fn loaded_table_overlapping_the_root_is_a_cycle() {
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[TABLE_A]), &[TABLE_A]);
    let mut root_buf = [0u8; 512];
    let tables = AcpiTables::new(&memory, RSDP, &mut root_buf).unwrap();
    let mut buf = [0u8; 512];
    assert_eq!(
        tables.load(XSDT, &mut buf).err(),
        Some(AcpiError::PointerCycle)
    );
}

#[test]
fn null_and_overflowing_addresses_are_rejected() {
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[TABLE_A, 0]), &[TABLE_A]);
    assert_eq!(walk(&memory), Err(AcpiError::NullPointer));
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[u64::MAX - 16]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::AddressOverflow));
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[PHYS_LIMIT - 20]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::AddressOverflow));
    let memory = synthetic(rsdp_v2(0, PHYS_LIMIT), XSDT, xsdt(&[]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::AddressOverflow));
    // A table whose header length runs past the physical limit.
    let mut memory = Memory::default();
    let near = PHYS_LIMIT - 0x1000;
    let mut huge = sdt(b"APIC", 1, &[0; 8]);
    put32(&mut huge, 4, 0x2000);
    fix_checksum(&mut huge);
    memory.insert(near, huge);
    let mut buf = [0u8; 0x3000];
    assert_eq!(
        load_table(&memory, near, &mut buf).err(),
        Some(AcpiError::AddressOverflow)
    );
    assert_eq!(
        load_table(&memory, 0, &mut buf).err(),
        Some(AcpiError::NullPointer)
    );
    assert_eq!(
        read_rsdp(&memory, u64::MAX - 4).err(),
        Some(AcpiError::AddressOverflow)
    );
}

#[test]
fn unreadable_memory_is_a_read_failure() {
    let memory = synthetic(
        rsdp_v2(0, XSDT),
        XSDT,
        xsdt(&[TABLE_A, TABLE_B]),
        &[TABLE_A],
    );
    assert_eq!(walk(&memory), Err(AcpiError::ReadFailed));
    assert_eq!(
        read_rsdp(&Memory::default(), RSDP).err(),
        Some(AcpiError::ReadFailed)
    );
    let memory = synthetic(rsdp_v0(RSDT as u32), XSDT, xsdt(&[]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::ReadFailed));
}

#[test]
fn root_table_corruption_is_rejected() {
    let entries = [TABLE_A];
    // Checksum.
    let mut root = xsdt(&entries);
    root[20] ^= 1;
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, root, &entries);
    assert_eq!(walk(&memory), Err(AcpiError::BadChecksum));
    // Body that is not a whole number of 8-byte entries.
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, sdt(b"XSDT", 1, &[0; 12]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::BadLength));
    // An RSDT where the RSDP promises an XSDT.
    let memory = synthetic(rsdp_v2(0, XSDT), XSDT, rsdt(&[TABLE_A as u32]), &entries);
    assert_eq!(walk(&memory), Err(AcpiError::BadSignature));
    // RSDP with neither root table.
    let memory = synthetic(rsdp_v2(0, 0), XSDT, xsdt(&[]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::NoRootTable));
    // RSDP whose length field exceeds what the walker will read.
    let mut rsdp = rsdp_v2(0, XSDT);
    put32(&mut rsdp, 20, 4096);
    fix_rsdp(&mut rsdp);
    let memory = synthetic(rsdp, XSDT, xsdt(&[]), &[]);
    assert_eq!(walk(&memory), Err(AcpiError::BadLength));
}

#[test]
fn table_changed_between_probe_and_load_is_rejected() {
    // A reader that returns a different signature on the second read models
    // firmware memory that changed under the walker.
    struct Flaky {
        memory: Memory,
        reads: std::cell::Cell<u32>,
    }
    impl PhysRead for Flaky {
        fn read(&self, phys: u64, buf: &mut [u8]) -> Result<(), hw_acpi::PhysReadError> {
            self.memory.read(phys, buf)?;
            if phys == TABLE_A && buf.len() > 4 {
                self.reads.set(self.reads.get() + 1);
                if self.reads.get() == 2 {
                    buf[..4].copy_from_slice(b"SSDT");
                    fix_checksum(buf);
                }
            }
            Ok(())
        }
    }
    let flaky = Flaky {
        memory: synthetic(rsdp_v2(0, XSDT), XSDT, xsdt(&[TABLE_A]), &[TABLE_A]),
        reads: std::cell::Cell::new(0),
    };
    let mut root_buf = [0u8; 512];
    let tables = AcpiTables::new(&flaky, RSDP, &mut root_buf).unwrap();
    let mut buf = [0u8; 64];
    assert_eq!(
        tables.find(*b"APIC", 0, &mut buf).err(),
        Some(AcpiError::BadSignature)
    );
}
