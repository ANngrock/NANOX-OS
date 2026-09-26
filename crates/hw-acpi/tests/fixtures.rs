//! Facts read from the captured QEMU/OVMF and Lenovo 82K8 tables.

mod common;

use common::{fixture, manifest, PROFILES};
use hw_acpi::dmar::{DmarStructure, PciPathEntry, SCOPE_IOAPIC, SCOPE_PCI_ENDPOINT};
use hw_acpi::fadt::FadtFlags;
use hw_acpi::ivrs::{IvhdEntry, IvrsBlock, SPECIAL_HPET, SPECIAL_IOAPIC};
use hw_acpi::madt::{MadtEntry, Polarity, TriggerMode};
use hw_acpi::{Dmar, Fadt, Gas, Hpet, Ivrs, Madt, Mcfg, Rsdp, Sdt};

#[test]
fn every_captured_table_has_a_valid_header_and_checksum() {
    let mut count = 0;
    for profile in PROFILES {
        for table in manifest(profile) {
            let bytes = fixture(profile, &table.file);
            if table.signature == "RSDP" {
                assert!(Rsdp::parse(&bytes).is_ok());
            } else if table.signature == "FACS" {
                // FACS has a signature and length but no SDT checksum.
                assert_eq!(&bytes[..4], b"FACS");
            } else {
                let sdt = Sdt::parse(&bytes).unwrap();
                assert_eq!(&sdt.signature(), table.signature.as_bytes());
                assert_eq!(sdt.bytes().len(), table.length);
            }
            count += 1;
        }
    }
    assert_eq!(count, 10 + 10 + 9 + 5);
}

fn madt_counts(madt: &Madt) -> (usize, usize, usize, usize) {
    let mut lapics = 0;
    let mut ioapics = 0;
    let mut overrides = 0;
    let mut nmis = 0;
    for entry in madt.entries() {
        match entry {
            MadtEntry::LocalApic(lapic) => {
                assert!(lapic.enabled());
                lapics += 1;
            }
            MadtEntry::IoApic(_) => ioapics += 1,
            MadtEntry::InterruptSourceOverride(_) => overrides += 1,
            MadtEntry::LocalApicNmi(_) => nmis += 1,
            other => panic!("unexpected entry {other:?}"),
        }
    }
    (lapics, ioapics, overrides, nmis)
}

#[test]
fn q35_madt_has_four_cpus_one_ioapic_and_legacy_overrides() {
    for profile in &PROFILES[..3] {
        let bytes = fixture(profile, "APIC.bin");
        let madt = Madt::parse(&bytes).unwrap();
        assert_eq!(madt.header().revision, 3);
        assert_eq!(madt.local_apic_address(), 0xfee0_0000);
        assert_eq!(madt.effective_local_apic_address(), 0xfee0_0000);
        assert!(madt.pcat_compat());
        assert_eq!(madt_counts(&madt), (4, 1, 5, 1));
        let ids: Vec<(u8, u8)> = madt
            .entries()
            .filter_map(|e| match e {
                MadtEntry::LocalApic(l) => Some((l.processor_uid, l.apic_id)),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [(0, 0), (1, 1), (2, 2), (3, 3)]);
        let entries: Vec<MadtEntry> = madt.entries().collect();
        let MadtEntry::IoApic(ioapic) = entries[4] else {
            panic!("IOAPIC expected after the LAPICs");
        };
        assert_eq!(
            (ioapic.io_apic_id, ioapic.address, ioapic.gsi_base),
            (0, 0xfec0_0000, 0)
        );
        // IRQ0 -> GSI2 with bus-default flags; IRQ9 is level, active high (SCI).
        let MadtEntry::InterruptSourceOverride(irq0) = entries[5] else {
            panic!()
        };
        assert_eq!((irq0.source, irq0.gsi, irq0.flags.0), (0, 2, 0));
        let MadtEntry::InterruptSourceOverride(irq9) = entries[7] else {
            panic!()
        };
        assert_eq!((irq9.source, irq9.gsi), (9, 9));
        assert_eq!(irq9.flags.polarity(), Polarity::ActiveHigh);
        assert_eq!(irq9.flags.trigger_mode(), TriggerMode::Level);
        let MadtEntry::LocalApicNmi(nmi) = entries[10] else {
            panic!()
        };
        assert_eq!((nmi.processor_uid, nmi.lint), (0xff, 1));
    }
}

#[test]
fn lenovo_madt_has_sixteen_threads_two_ioapics_and_per_cpu_nmis() {
    let bytes = fixture("lenovo-82k8", "APIC.bin");
    let madt = Madt::parse(&bytes).unwrap();
    assert_eq!(madt.header().revision, 2);
    assert_eq!(&madt.header().oem_id, b"LENOVO");
    assert_eq!(madt_counts(&madt), (16, 2, 2, 16));
    let apic_ids: Vec<u8> = madt
        .entries()
        .filter_map(|e| match e {
            MadtEntry::LocalApic(l) => Some(l.apic_id),
            _ => None,
        })
        .collect();
    assert_eq!(apic_ids, (0..16).collect::<Vec<u8>>());
    let ioapics: Vec<_> = madt
        .entries()
        .filter_map(|e| match e {
            MadtEntry::IoApic(io) => Some((io.io_apic_id, io.address, io.gsi_base)),
            _ => None,
        })
        .collect();
    assert_eq!(ioapics, [(0x20, 0xfec0_0000, 0), (0x21, 0xfec0_1000, 24)]);
    // One LAPIC NMI per processor UID (not the 0xff broadcast form), LINT1,
    // edge-triggered active high.
    let nmis: Vec<_> = madt
        .entries()
        .filter_map(|e| match e {
            MadtEntry::LocalApicNmi(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(nmis.len(), 16);
    for (uid, nmi) in nmis.iter().enumerate() {
        assert_eq!(usize::from(nmi.processor_uid), uid);
        assert_eq!(nmi.lint, 1);
        assert_eq!(nmi.flags.polarity(), Polarity::ActiveHigh);
        assert_eq!(nmi.flags.trigger_mode(), TriggerMode::Edge);
    }
}

#[test]
fn mcfg_ecam_windows() {
    for profile in &PROFILES[..3] {
        let bytes = fixture(profile, "MCFG.bin");
        let mcfg = Mcfg::parse(&bytes).unwrap();
        let segments: Vec<_> = mcfg.segments().collect();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].base_address, 0xe000_0000);
        assert_eq!(
            (
                segments[0].segment,
                segments[0].start_bus,
                segments[0].end_bus
            ),
            (0, 0, 255)
        );
        assert_eq!(segments[0].bus_count(), 256);
    }
    let bytes = fixture("lenovo-82k8", "MCFG.bin");
    let mcfg = Mcfg::parse(&bytes).unwrap();
    let segment = mcfg.segments().next().unwrap();
    assert_eq!(mcfg.len(), 1);
    assert_eq!(segment.base_address, 0xf800_0000);
    assert_eq!((segment.start_bus, segment.end_bus), (0, 0x3f));
    assert!(segment.contains_bus(0x3f) && !segment.contains_bus(0x40));
}

#[test]
fn hpet_descriptions() {
    let q35 = Hpet::parse(&fixture("q35-smp4", "HPET.bin")).unwrap();
    assert_eq!(q35.base_address.address, 0xfed0_0000);
    assert_eq!(q35.base_address.address_space, Gas::SYSTEM_MEMORY);
    assert_eq!(q35.pci_vendor_id(), 0x8086);
    assert_eq!(q35.comparator_count(), 3);
    assert!(q35.counter_is_64bit());
    assert!(q35.legacy_replacement_capable());
    assert_eq!((q35.hpet_number, q35.minimum_tick), (0, 0));

    let lenovo = Hpet::parse(&fixture("lenovo-82k8", "HPET.bin")).unwrap();
    assert_eq!(lenovo.base_address.address, 0xfed0_0000);
    assert_eq!(lenovo.event_timer_block_id, 0x4353_8210);
    assert_eq!(lenovo.hardware_revision(), 0x10);
    assert_eq!(lenovo.comparator_count(), 3);
    // The AMD FCH timer reports a 32-bit main counter.
    assert!(!lenovo.counter_is_64bit());
    assert!(lenovo.legacy_replacement_capable());
    // Reported as captured: bit width 8 in the GAS, HPET number 2, 20 ticks.
    assert_eq!(lenovo.base_address.bit_width, 8);
    assert_eq!((lenovo.hpet_number, lenovo.minimum_tick), (2, 20));
}

#[test]
fn q35_fadt_revision3_uses_x_fields_and_io_reset() {
    let bytes = fixture("q35-smp4", "FACP.bin");
    let fadt = Fadt::parse(&bytes).unwrap();
    assert_eq!(fadt.revision(), 3);
    assert_eq!(fadt.minor_version(), Some(0));
    assert_eq!(fadt.dsdt(), Some(0x1f77_a000));
    // X_FIRMWARE_CTRL is zero, so the 32-bit FIRMWARE_CTRL is used.
    assert_eq!(fadt.firmware_ctrl(), Some(0x1f7d_d000));
    assert_eq!(fadt.sci_interrupt(), 9);
    assert_eq!(fadt.smi_command_port(), 0xb2);
    assert_eq!((fadt.acpi_enable(), fadt.acpi_disable()), (2, 3));
    assert_eq!(fadt.flags(), FadtFlags(0x84a5));
    let timer = fadt.pm_timer().unwrap();
    assert_eq!(timer.register.address_space, Gas::SYSTEM_IO);
    assert_eq!(
        (timer.register.address, timer.register.bit_width),
        (0x608, 32)
    );
    assert!(!timer.counter_is_32bit);
    let reset = fadt.reset_register().unwrap();
    assert_eq!(reset.register.address_space, Gas::SYSTEM_IO);
    assert_eq!((reset.register.address, reset.value), (0xcf9, 0x0f));
    assert_eq!(fadt.pm1a_event_block().unwrap().address, 0x600);
    assert_eq!(fadt.pm1a_control_block().unwrap().address, 0x604);
    assert_eq!(fadt.gpe0_block().unwrap().bit_width, 128);
    assert_eq!(fadt.pm1b_event_block(), None);
    assert_eq!(fadt.gpe1_block(), None);
    // 244-byte table: the revision 5/6 tail is absent.
    assert_eq!(fadt.sleep_control_register(), None);
    assert_eq!(fadt.hypervisor_vendor_id(), None);
}

#[test]
fn lenovo_fadt_revision6_has_zero_address_x_blocks() {
    let bytes = fixture("lenovo-82k8", "FACP.bin");
    let fadt = Fadt::parse(&bytes).unwrap();
    assert_eq!((fadt.revision(), fadt.minor_version()), (6, Some(3)));
    assert_eq!(fadt.dsdt(), Some(0xc8f9_f000));
    assert_eq!(fadt.firmware_ctrl(), Some(0xcc97_d000));
    assert_eq!(fadt.preferred_pm_profile(), 2); // mobile
    assert_eq!(fadt.iapc_boot_arch(), 3);
    let flags = fadt.flags();
    assert!(flags.contains(FadtFlags::TMR_VAL_EXT));
    assert!(flags.contains(FadtFlags::RESET_REG_SUP));
    assert!(!flags.contains(FadtFlags::HW_REDUCED_ACPI));
    assert!(!flags.contains(FadtFlags::LOW_POWER_S0_IDLE_CAPABLE));
    let timer = fadt.pm_timer().unwrap();
    assert_eq!(
        (timer.register.address, timer.register.access_size),
        (0x408, 3)
    );
    assert!(timer.counter_is_32bit);
    let reset = fadt.reset_register().unwrap();
    assert_eq!(
        (
            reset.register.address,
            reset.register.access_size,
            reset.value
        ),
        (0xcf9, 1, 6)
    );
    // X_PM1b_EVT_BLK and X_GPE1_BLK carry a space ID and access size but a
    // zero address: absent, and the 32-bit fields are zero too.
    assert_eq!(&bytes[160..164], &[1, 0, 0, 2]);
    assert_eq!(fadt.pm1b_event_block(), None);
    assert_eq!(fadt.pm1b_control_block(), None);
    assert_eq!(fadt.gpe1_block(), None);
    assert_eq!(fadt.pm2_control_block().unwrap().address, 0x800);
    let gpe0 = fadt.gpe0_block().unwrap();
    assert_eq!((gpe0.address, gpe0.bit_width), (0x420, 64));
    // Sleep registers exist in the layout but have address 0.
    assert_eq!(fadt.sleep_control_register(), None);
    assert_eq!(fadt.sleep_status_register(), None);
    assert_eq!(fadt.hypervisor_vendor_id(), Some(0));
}

#[test]
fn q35_dmar_has_one_drhd_with_ioapic_and_endpoint_scopes() {
    let bytes = fixture("q35-smp4-intel-iommu", "DMAR.bin");
    let dmar = Dmar::parse(&bytes).unwrap();
    assert_eq!(dmar.host_address_width(), 39);
    assert!(dmar.interrupt_remapping());
    assert!(!dmar.x2apic_opt_out());
    let structures: Vec<_> = dmar.structures().collect();
    assert_eq!(structures.len(), 1);
    let DmarStructure::Drhd(drhd) = &structures[0] else {
        panic!("DRHD expected");
    };
    assert_eq!((drhd.segment, drhd.register_base), (0, 0xfed9_0000));
    assert!(!drhd.include_pci_all());
    let scopes: Vec<_> = drhd.scopes.clone().collect();
    assert_eq!(scopes.len(), 5);
    assert_eq!(scopes[0].scope_type, SCOPE_IOAPIC);
    assert_eq!((scopes[0].enumeration_id, scopes[0].start_bus), (0, 0xff));
    let endpoints: Vec<Vec<PciPathEntry>> = scopes[1..]
        .iter()
        .map(|s| {
            assert_eq!((s.scope_type, s.start_bus), (SCOPE_PCI_ENDPOINT, 0));
            s.path.collect()
        })
        .collect();
    let hop = |device, function| vec![PciPathEntry { device, function }];
    assert_eq!(
        endpoints,
        [hop(0, 0), hop(0x1f, 0), hop(0x1f, 2), hop(0x1f, 3)]
    );
}

fn ivhds<'a>(ivrs: &Ivrs<'a>) -> Vec<hw_acpi::ivrs::Ivhd<'a>> {
    ivrs.blocks()
        .map(|b| match b {
            IvrsBlock::Ivhd(ivhd) => ivhd,
            other => panic!("unexpected block {other:?}"),
        })
        .collect()
}

#[test]
fn q35_ivrs_revision1_carries_ivhd_10h_and_11h() {
    let bytes = fixture("q35-smp4-amd-iommu", "IVRS.bin");
    let ivrs = Ivrs::parse(&bytes).unwrap();
    assert_eq!(ivrs.header().revision, 1);
    assert!(ivrs.efr_supported());
    assert_eq!(ivrs.physical_address_size(), 40);
    let ivhds = ivhds(&ivrs);
    assert_eq!(
        ivhds.iter().map(|h| h.block_type).collect::<Vec<_>>(),
        [0x10, 0x11]
    );
    for ivhd in &ivhds {
        assert_eq!((ivhd.device_id, ivhd.capability_offset), (0x0008, 0x40));
        assert_eq!(ivhd.base_address, 0xfed8_0000);
        let entries: Vec<_> = ivhd.entries.collect();
        let selects: Vec<u16> = entries
            .iter()
            .filter_map(|e| match e {
                IvhdEntry::Select { device_id, .. } => Some(*device_id),
                _ => None,
            })
            .collect();
        assert_eq!(selects, [0x0000, 0x0008, 0x00f8, 0x00fa, 0x00fb]);
        assert_eq!(
            entries.last(),
            Some(&IvhdEntry::Special {
                data: 0,
                handle: 0,
                device_id: 0x00a0,
                variety: SPECIAL_IOAPIC,
            })
        );
    }
    assert_eq!((ivhds[0].efr, ivhds[0].feature_info), (None, 0x44));
    assert_eq!((ivhds[1].efr, ivhds[1].efr2), (Some(0x29d3), None));
}

#[test]
fn lenovo_ivrs_has_three_ivhd_views_ranges_and_acpi_hid_uarts() {
    let bytes = fixture("lenovo-82k8", "IVRS.bin");
    let ivrs = Ivrs::parse(&bytes).unwrap();
    assert_eq!(ivrs.header().revision, 2);
    assert!(ivrs.efr_supported() && ivrs.dma_remap_supported());
    assert_eq!(ivrs.physical_address_size(), 48);
    assert_eq!(ivrs.virtual_address_size(), 64);
    assert_eq!(ivrs.guest_virtual_address_size(), 2);
    let ivhds = ivhds(&ivrs);
    assert_eq!(
        ivhds.iter().map(|h| h.block_type).collect::<Vec<_>>(),
        [0x10, 0x11, 0x40]
    );
    let common = [
        IvhdEntry::StartRange {
            device_id: 0x0008,
            data: 0,
        },
        IvhdEntry::EndRange { device_id: 0xfffe },
        IvhdEntry::AliasStartRange {
            device_id: 0xff00,
            data: 0,
            source_id: 0x00a5,
        },
        IvhdEntry::EndRange { device_id: 0xffff },
        IvhdEntry::Pad,
        IvhdEntry::Special {
            data: 0,
            handle: 0,
            device_id: 0x00a0,
            variety: SPECIAL_HPET,
        },
        IvhdEntry::Special {
            data: 0xd7,
            handle: 0x20,
            device_id: 0x00a0,
            variety: SPECIAL_IOAPIC,
        },
        IvhdEntry::Special {
            data: 0,
            handle: 0x21,
            device_id: 0x0001,
            variety: SPECIAL_IOAPIC,
        },
    ];
    for ivhd in &ivhds {
        assert_eq!((ivhd.device_id, ivhd.base_address), (0x0002, 0xfdd0_0000));
        let entries: Vec<_> = ivhd.entries.collect();
        assert_eq!(entries[..8], common);
    }
    assert_eq!(ivhds[0].feature_info, 0x8004_8f6f);
    assert_eq!(ivhds[1].efr, Some(0x206d_73ef_2225_4ade));
    assert_eq!(ivhds[1].efr2, None);
    assert_eq!(ivhds[2].efr2, Some(0));
    let hids: Vec<_> = ivhds[2].entries.skip(8).collect();
    assert_eq!(hids.len(), 4);
    for (n, hid) in hids.iter().enumerate() {
        let IvhdEntry::AcpiHid {
            device_id,
            data,
            hid,
            cid,
            uid_format,
            uid,
        } = *hid
        else {
            panic!("ACPI HID entry expected");
        };
        assert_eq!((device_id, data, uid_format), (0x00a5, 0x40, 2));
        assert_eq!(&hid, b"AMDI0020");
        assert_eq!(cid, [0; 8]);
        assert_eq!(uid, format!("\\_SB.FUR{n}").as_bytes());
    }
}
