mod common;

use hw_smp::{build_topology, ApicEntry, CpuInfo, CpuPresence, TopologyError, MAX_CPUS};

fn enabled(id: u32) -> ApicEntry {
    ApicEntry::from_madt_flags(id, 1)
}

fn online_capable(id: u32) -> ApicEntry {
    ApicEntry::from_madt_flags(id, 2)
}

fn placeholder(id: u32) -> ApicEntry {
    ApicEntry::from_madt_flags(id, 0)
}

fn ids(cpus: &[CpuInfo]) -> Vec<u32> {
    cpus.iter().map(|c| c.apic_id).collect()
}

#[test]
fn q35_smp4_fixture_yields_four_cpus() {
    let entries = common::madt_cpus("q35-smp4/APIC.bin");
    assert_eq!(entries.len(), 4, "fixture has 4 processor entries");
    let mut buf = [CpuInfo::EMPTY; 8];
    let t = build_topology(entries, 0, &mut buf).unwrap();
    assert_eq!(t.len(), 4);
    assert_eq!(t.enabled_count(), 4);
    assert_eq!(t.online_capable_count(), 0);
    assert_eq!(t.ignored_count(), 0);
    assert_eq!(ids(t.cpus()), [0, 1, 2, 3]);
    assert_eq!(t.bsp().apic_id, 0);
    assert!(!t.requires_x2apic());
    assert!(t.cpus().iter().all(|c| c.presence == CpuPresence::Enabled));
}

#[test]
fn lenovo_82k8_fixture_yields_sixteen_threads() {
    let entries = common::madt_cpus("lenovo-82k8/APIC.bin");
    assert_eq!(entries.len(), 16, "Ryzen 7 5800H: 8 cores / 16 threads");
    let mut buf = [CpuInfo::EMPTY; 32];
    let t = build_topology(entries.iter().copied(), 0, &mut buf).unwrap();
    assert_eq!(t.len(), 16);
    assert_eq!(t.enabled_count(), 16);
    assert_eq!(ids(t.cpus()), (0..16).collect::<Vec<_>>());
    assert_eq!(t.index_of(9), Some(9));
    assert_eq!(t.index_of(16), None);
    assert!(!t.requires_x2apic());
}

#[test]
fn bsp_gets_logical_zero_wherever_it_is_listed() {
    let entries = common::madt_cpus("lenovo-82k8/APIC.bin");
    let mut buf = [CpuInfo::EMPTY; 16];
    let t = build_topology(entries, 5, &mut buf).unwrap();
    let mut expected = vec![5];
    expected.extend((0..16).filter(|&i| i != 5));
    assert_eq!(ids(t.cpus()), expected);
    assert_eq!(t.index_of(5), Some(0));
}

#[test]
fn buffer_bounds_the_table_exactly() {
    let entries = common::madt_cpus("q35-smp4/APIC.bin");
    let mut exact = [CpuInfo::EMPTY; 4];
    assert!(build_topology(entries.iter().copied(), 0, &mut exact).is_ok());
    let mut short = [CpuInfo::EMPTY; 3];
    assert_eq!(
        build_topology(entries.iter().copied(), 0, &mut short).unwrap_err(),
        TopologyError::Overflow { capacity: 3 }
    );
    // BSP listed last: its reserved slot still counts against capacity.
    let late_bsp = [enabled(1), enabled(2), enabled(3), enabled(0)];
    assert_eq!(
        build_topology(late_bsp, 0, &mut short).unwrap_err(),
        TopologyError::Overflow { capacity: 3 }
    );
    let mut empty: [CpuInfo; 0] = [];
    assert_eq!(
        build_topology(entries, 0, &mut empty).unwrap_err(),
        TopologyError::NoCapacity
    );
}

#[test]
fn capacity_is_capped_at_max_cpus() {
    let mut big = vec![CpuInfo::EMPTY; MAX_CPUS + 10];
    let full = (0..MAX_CPUS as u32).map(|i| enabled(i * 2));
    let t = build_topology(full, 0, &mut big).unwrap();
    assert_eq!(t.len(), MAX_CPUS);
    assert!(t.requires_x2apic());
    let over = (0..=MAX_CPUS as u32).map(|i| enabled(i * 2));
    assert_eq!(
        build_topology(over, 0, &mut big).unwrap_err(),
        TopologyError::Overflow { capacity: MAX_CPUS }
    );
}

#[test]
fn duplicate_apic_ids_are_rejected() {
    let mut buf = [CpuInfo::EMPTY; 32];
    let mut entries = common::madt_cpus("lenovo-82k8/APIC.bin");
    entries.push(enabled(7));
    assert_eq!(
        build_topology(entries, 0, &mut buf).unwrap_err(),
        TopologyError::DuplicateApicId(7)
    );
    let bsp_twice = [enabled(0), enabled(1), enabled(0)];
    assert_eq!(
        build_topology(bsp_twice, 0, &mut buf).unwrap_err(),
        TopologyError::DuplicateApicId(0)
    );
    let across_kinds = [enabled(0), online_capable(4), enabled(4)];
    assert_eq!(
        build_topology(across_kinds, 0, &mut buf).unwrap_err(),
        TopologyError::DuplicateApicId(4)
    );
    let both_hotplug = [enabled(0), online_capable(4), online_capable(4)];
    assert_eq!(
        build_topology(both_hotplug, 0, &mut buf).unwrap_err(),
        TopologyError::DuplicateApicId(4)
    );
}

#[test]
fn bsp_must_be_listed_and_enabled() {
    let mut buf = [CpuInfo::EMPTY; 8];
    let entries = common::madt_cpus("q35-smp4/APIC.bin");
    assert_eq!(
        build_topology(entries, 9, &mut buf).unwrap_err(),
        TopologyError::BspMissing(9)
    );
    let hotplug_bsp = [online_capable(0), enabled(1)];
    assert_eq!(
        build_topology(hotplug_bsp, 0, &mut buf).unwrap_err(),
        TopologyError::BspNotEnabled(0)
    );
    let placeholder_bsp = [placeholder(0), enabled(1)];
    assert_eq!(
        build_topology(placeholder_bsp, 0, &mut buf).unwrap_err(),
        TopologyError::BspMissing(0)
    );
}

#[test]
fn online_capable_cpus_follow_enabled_ones_in_firmware_order() {
    let entries = [
        enabled(2),
        online_capable(10),
        enabled(0),
        placeholder(3),
        online_capable(11),
        enabled(1),
        placeholder(1), // duplicate ID in a placeholder is not a conflict
        online_capable(12),
    ];
    let mut buf = [CpuInfo::EMPTY; 6];
    let t = build_topology(entries, 0, &mut buf).unwrap();
    assert_eq!(ids(t.cpus()), [0, 2, 1, 10, 11, 12]);
    assert_eq!(t.enabled_count(), 3);
    assert_eq!(t.online_capable_count(), 3);
    assert_eq!(t.ignored_count(), 2);
    for (i, c) in t.cpus().iter().enumerate() {
        let want = if i < 3 {
            CpuPresence::Enabled
        } else {
            CpuPresence::OnlineCapable
        };
        assert_eq!(c.presence, want, "cpu {i}");
    }
    // One slot fewer: the sixth usable entry overflows.
    let mut five = [CpuInfo::EMPTY; 5];
    assert_eq!(
        build_topology(entries, 0, &mut five).unwrap_err(),
        TopologyError::Overflow { capacity: 5 }
    );
}

#[test]
fn ids_beyond_xapic_range_require_x2apic() {
    let entries = [enabled(0), enabled(254), enabled(255), enabled(300)];
    let mut buf = [CpuInfo::EMPTY; 4];
    let t = build_topology(entries, 0, &mut buf).unwrap();
    let flags: Vec<bool> = t.cpus().iter().map(|c| c.needs_x2apic).collect();
    // 0xFF is the xAPIC broadcast destination, so it needs x2APIC as well.
    assert_eq!(flags, [false, false, true, true]);
    assert!(t.requires_x2apic());

    let broadcast = [enabled(0), enabled(0xFFFF_FFFF)];
    assert_eq!(
        build_topology(broadcast, 0, &mut buf).unwrap_err(),
        TopologyError::InvalidApicId(0xFFFF_FFFF)
    );
}

#[test]
fn madt_flag_decoding() {
    let e = ApicEntry::from_madt_flags(5, 0b10);
    assert!(!e.enabled && e.online_capable);
    let e = ApicEntry::from_madt_flags(5, 0b01);
    assert!(e.enabled && !e.online_capable);
    let e = ApicEntry::from_madt_flags(5, 0xFFFF_FFFC);
    assert!(!e.enabled && !e.online_capable);
}
