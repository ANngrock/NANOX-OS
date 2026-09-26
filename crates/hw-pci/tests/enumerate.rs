mod common;

use common::{bdf, lenovo, q35, segment_from, Func, Model, LENOVO_MCFG, Q35_MCFG};
use hw_pci::{
    enumerate, extended_capabilities, find_capability, probe_bars, read_bridge_buses, Bdf,
    BridgeBuses, BusNumbering, BusRange, EcamSegment, EnumerationConfig, Function, HeaderKind, Msi,
    MsiX, PciError, PcieCapability, PciePortType, CAP_ID_MSI, CAP_ID_MSIX, CAP_ID_PCIE,
    MAX_BRIDGE_DEPTH,
};

fn config(model: &Model, max_depth: u8, numbering: BusNumbering) -> EnumerationConfig {
    EnumerationConfig {
        buses: model.segment.buses(),
        max_depth,
        numbering,
    }
}

fn run(
    model: &mut Model,
    max_depth: u8,
    numbering: BusNumbering,
) -> Result<Vec<Function>, PciError> {
    let cfg = config(model, max_depth, numbering);
    let mut out = [Function::default(); 64];
    let count = enumerate(model, &cfg, &mut out)?;
    Ok(out[..count].to_vec())
}

fn buses(primary: u8, secondary: u8, subordinate: u8) -> Option<BridgeBuses> {
    Some(BridgeBuses {
        primary,
        secondary,
        subordinate,
    })
}

type Row = (Bdf, u16, u16, u8, Option<Bdf>, Option<BridgeBuses>);

fn rows(functions: &[Function]) -> Vec<Row> {
    functions
        .iter()
        .map(|f| {
            (
                f.bdf,
                f.header.vendor_id,
                f.header.device_id,
                f.depth,
                f.parent,
                f.bridge,
            )
        })
        .collect()
}

fn q35_expected() -> Vec<Row> {
    vec![
        (bdf(0, 0, 0), 0x8086, 0x29C0, 0, None, None),
        (bdf(0, 1, 0), 0x1234, 0x1111, 0, None, None),
        (bdf(0, 2, 0), 0x1AF4, 0x1042, 0, None, None),
        (bdf(0, 3, 0), 0x1B36, 0x000C, 0, None, buses(0, 1, 2)),
        (
            bdf(1, 0, 0),
            0x1B36,
            0x000E,
            1,
            Some(bdf(0, 3, 0)),
            buses(1, 2, 2),
        ),
        (bdf(2, 1, 0), 0x1AF4, 0x1041, 2, Some(bdf(1, 0, 0)), None),
        (bdf(0, 0x1F, 0), 0x8086, 0x2918, 0, None, None),
        (bdf(0, 0x1F, 2), 0x8086, 0x2922, 0, None, None),
        (bdf(0, 0x1F, 3), 0x8086, 0x2930, 0, None, None),
    ]
}

#[test]
fn q35_topology_validates_firmware_numbering() {
    let mut model = q35(true);
    let functions = run(&mut model, 4, BusNumbering::Validate).unwrap();
    assert_eq!(rows(&functions), q35_expected());
    assert!(model.writes.is_empty(), "validation never writes");

    let host = &functions[0].header;
    assert_eq!(
        (host.class, host.subclass, host.kind),
        (0x06, 0x00, HeaderKind::Endpoint)
    );
    let lpc = &functions[6].header;
    assert!(lpc.multi_function);
    let ahci = &functions[7].header;
    assert_eq!(
        (ahci.class, ahci.subclass, ahci.prog_if),
        (0x01, 0x06, 0x01)
    );
}

#[test]
fn q35_devices_parse_after_enumeration() {
    let mut model = q35(true);
    let functions = run(&mut model, 4, BusNumbering::Validate).unwrap();
    for f in &functions {
        probe_bars(&mut model, f.bdf, f.header.kind).unwrap();
    }

    let blk = bdf(0, 2, 0);
    let bars = probe_bars(&mut model, blk, HeaderKind::Endpoint).unwrap();
    assert_eq!(bars.get(4).unwrap().address, 0x80_0000_0000);
    let at = find_capability(&mut model, blk, HeaderKind::Endpoint, CAP_ID_MSIX)
        .unwrap()
        .unwrap();
    let msix = MsiX::read(&mut model, blk, at, &bars).unwrap();
    assert_eq!(
        (msix.table_size, msix.table.address, msix.pba.address),
        (2, 0xC100_1000, 0xC100_1800)
    );

    let net = bdf(2, 1, 0);
    let bars = probe_bars(&mut model, net, HeaderKind::Endpoint).unwrap();
    assert_eq!(bars.get(0).unwrap().size, 32);
    let at = find_capability(&mut model, net, HeaderKind::Endpoint, CAP_ID_MSIX)
        .unwrap()
        .unwrap();
    assert_eq!(
        MsiX::read(&mut model, net, at, &bars).unwrap().table_size,
        3
    );

    let port = bdf(0, 3, 0);
    let at = find_capability(&mut model, port, HeaderKind::PciBridge, CAP_ID_PCIE)
        .unwrap()
        .unwrap();
    assert_eq!(
        PcieCapability::read(&mut model, port, at)
            .unwrap()
            .port_type,
        PciePortType::RootPort
    );
    let ext: Vec<_> = extended_capabilities(&mut model, port)
        .map(|c| c.unwrap().id)
        .collect();
    assert_eq!(ext, vec![0x0001, 0x000D]);

    let ahci = bdf(0, 0x1F, 2);
    let at = find_capability(&mut model, ahci, HeaderKind::Endpoint, CAP_ID_MSI)
        .unwrap()
        .unwrap();
    assert!(Msi::read(&mut model, ahci, at).unwrap().address_64bit);
}

#[test]
fn q35_assignment_matches_firmware_numbering_and_routes() {
    let mut model = q35(false);
    assert!(
        model.func(2, 1, 0).is_none(),
        "bus 2 unreachable before assignment"
    );
    let functions = run(&mut model, 4, BusNumbering::Assign).unwrap();
    assert_eq!(rows(&functions), q35_expected());
    assert_eq!(model.func(2, 1, 0).unwrap().get16(0x02), 0x1041);
    assert_eq!(
        read_bridge_buses(&mut model, bdf(0, 3, 0)),
        Ok(BridgeBuses {
            primary: 0,
            secondary: 1,
            subordinate: 2
        })
    );
    // The result now validates as firmware-programmed numbering.
    let again = run(&mut model, 4, BusNumbering::Validate).unwrap();
    assert_eq!(again, functions);
}

#[test]
fn result_buffer_must_hold_every_function() {
    let mut model = q35(true);
    let cfg = config(&model, 4, BusNumbering::Validate);
    let mut exact = [Function::default(); 9];
    assert_eq!(enumerate(&mut model, &cfg, &mut exact), Ok(9));
    let mut short = [Function::default(); 8];
    assert_eq!(
        enumerate(&mut model, &cfg, &mut short),
        Err(PciError::BufferTooSmall)
    );
    assert_eq!(
        enumerate(&mut model, &cfg, &mut []),
        Err(PciError::BufferTooSmall)
    );
}

#[test]
fn lenovo_candidate_topology() {
    let mut model = lenovo(true);
    let functions = run(&mut model, 2, BusNumbering::Validate).unwrap();
    assert_eq!(functions.len(), 19);
    let ids: Vec<_> = functions
        .iter()
        .map(|f| (f.bdf, f.header.vendor_id, f.header.device_id))
        .collect();
    let expected = [
        (bdf(0, 0, 0), 0x1022, 0x1630),
        (bdf(0, 0, 2), 0x1022, 0x1631),
        (bdf(0, 1, 0), 0x1022, 0x1632),
        (bdf(0, 1, 1), 0x1022, 0x1633),
        (bdf(1, 0, 0), 0x10DE, 0x2560),
        (bdf(0, 2, 0), 0x1022, 0x1632),
        (bdf(0, 2, 1), 0x1022, 0x1634),
        (bdf(2, 0, 0), 0x126F, 0x2263),
        (bdf(0, 2, 2), 0x1022, 0x1634),
        (bdf(3, 0, 0), 0x8086, 0x2723),
        (bdf(0, 2, 3), 0x1022, 0x1634),
        (bdf(4, 0, 0), 0x1217, 0x8621),
        (bdf(0, 2, 4), 0x1022, 0x1634),
        (bdf(5, 0, 0), 0x144D, 0xA808),
        (bdf(0, 8, 0), 0x1022, 0x1632),
        (bdf(0, 8, 1), 0x1022, 0x1635),
        (bdf(6, 0, 0), 0x1002, 0x1638),
        (bdf(6, 0, 3), 0x1022, 0x1639),
        (bdf(6, 0, 4), 0x1022, 0x1639),
    ];
    assert_eq!(ids, expected);

    for f in &functions {
        let bars = probe_bars(&mut model, f.bdf, f.header.kind).unwrap();
        let pcie = find_capability(&mut model, f.bdf, f.header.kind, CAP_ID_PCIE).unwrap();
        if let Some(at) = pcie {
            let port = PcieCapability::read(&mut model, f.bdf, at).unwrap();
            assert_eq!(
                port.port_type.is_bridge(),
                f.header.kind == HeaderKind::PciBridge
            );
        }
        if f.header.kind == HeaderKind::PciBridge {
            assert_eq!(
                pcie.map(|_| ()),
                Some(()),
                "root ports carry a PCIe capability"
            );
            let b = f.bridge.unwrap();
            assert_eq!((b.primary, b.secondary), (0, b.subordinate));
        }
        if let Some(at) = find_capability(&mut model, f.bdf, f.header.kind, CAP_ID_MSIX).unwrap() {
            MsiX::read(&mut model, f.bdf, at, &bars).unwrap();
        }
    }

    let nvme = bdf(5, 0, 0);
    let bars = probe_bars(&mut model, nvme, HeaderKind::Endpoint).unwrap();
    let msix = MsiX::read(&mut model, nvme, 0xB0, &bars).unwrap();
    assert_eq!(msix.table_size, 33);
    assert_eq!(
        (msix.table.address, msix.pba.address),
        (0xD140_3000, 0xD140_2000)
    );
    assert_eq!(msix.pba.length, 8);

    let gpu = bdf(1, 0, 0);
    let bars = probe_bars(&mut model, gpu, HeaderKind::Endpoint).unwrap();
    assert_eq!(bars.get(1).unwrap().size, 8 << 30);
    assert_eq!(bars.iter().count(), 4);
}

#[test]
fn lenovo_assignment_reproduces_numbering() {
    let mut firmware = lenovo(true);
    let expected = run(&mut firmware, 2, BusNumbering::Validate).unwrap();
    let mut model = lenovo(false);
    let assigned = run(&mut model, 2, BusNumbering::Assign).unwrap();
    assert_eq!(assigned, expected);
}

/// Root bus 0 of a q35-sized segment with the given bridges on it; each
/// bridge gets an empty downstream bus.
/// (device, (primary, secondary, subordinate)) of a bridge on the root bus.
type BridgeSpec = (u8, (u8, u8, u8));

fn root_with_bridges(bridges: &[BridgeSpec]) -> Model {
    let mut model = Model::new(segment_from(Q35_MCFG));
    model.add(0, 0, 0, Func::endpoint(0x8086, 0x29C0, 0x06, 0, 0));
    for &(device, (p, s, sub)) in bridges {
        model.add_bridge(0, device, 0, Func::bridge(0x1B36, 0x000C).buses(p, s, sub));
    }
    model
}

#[test]
fn bridge_numbering_errors() {
    let cases: &[(&[BridgeSpec], PciError)] = &[
        (&[(1, (0, 1, 1)), (2, (0, 1, 1))], PciError::BusConflict),
        (&[(1, (0, 1, 3)), (2, (0, 3, 4))], PciError::BusConflict),
        (&[(1, (0, 2, 4)), (2, (0, 1, 3))], PciError::BusConflict),
        (&[(1, (0, 3, 2))], PciError::SubordinateBelowSecondary),
        (&[(1, (5, 6, 6))], PciError::BridgePrimaryMismatch),
        (&[(1, (0, 0, 0))], PciError::BusConflict),
        (&[(1, (0, 0, 4))], PciError::BusConflict),
    ];
    for (bridges, error) in cases {
        let mut model = root_with_bridges(bridges);
        assert_eq!(
            run(&mut model, 4, BusNumbering::Validate),
            Err(*error),
            "{bridges:?}"
        );
    }
    // Disjoint siblings in any order are fine.
    let mut model = root_with_bridges(&[(1, (0, 5, 6)), (2, (0, 1, 4)), (3, (0, 7, 7))]);
    assert_eq!(run(&mut model, 4, BusNumbering::Validate).unwrap().len(), 4);
}

#[test]
fn child_bridge_must_stay_inside_parent_window() {
    for (child, error) in [
        ((1, 3, 3), PciError::BusOutsideWindow),
        ((1, 2, 3), PciError::BusOutsideWindow),
        ((1, 1, 1), PciError::BusConflict),
        ((1, 0, 0), PciError::BusConflict),
    ] {
        let mut model = root_with_bridges(&[]);
        let bus1 = model.add_bridge(0, 1, 0, Func::bridge(0x1B36, 0x000C).buses(0, 1, 2));
        model.add_bridge(
            bus1,
            0,
            0,
            Func::bridge(0x1B36, 0x000E).buses(child.0, child.1, child.2),
        );
        assert_eq!(
            run(&mut model, 4, BusNumbering::Validate),
            Err(error),
            "{child:?}"
        );
    }
}

#[test]
fn bridge_outside_segment_bus_range() {
    let mut model = Model::new(segment_from(LENOVO_MCFG));
    model.add_bridge(0, 1, 0, Func::bridge(0x1022, 0x1634).buses(0, 0x3F, 0x40));
    assert_eq!(
        run(&mut model, 4, BusNumbering::Validate),
        Err(PciError::BusOutOfRange)
    );
}

/// Chain of `length` bridges: bus d holds a bridge to bus d + 1.
fn chain(length: u8) -> Model {
    let mut model = Model::new(segment_from(Q35_MCFG));
    let mut phys = 0;
    for depth in 0..length {
        phys = model.add_bridge(
            phys,
            0,
            0,
            Func::bridge(0x1B36, 0x000E).buses(depth, depth + 1, length),
        );
    }
    model.add(phys, 0, 0, Func::endpoint(0x1AF4, 0x1041, 0x02, 0, 0));
    model
}

#[test]
fn bridge_depth_is_bounded() {
    let functions = run(&mut chain(5), 5, BusNumbering::Validate).unwrap();
    assert_eq!(functions.len(), 6);
    assert_eq!(functions[5].depth, 5);
    assert_eq!(functions[5].parent, Some(bdf(4, 0, 0)));
    assert_eq!(
        run(&mut chain(5), 4, BusNumbering::Validate),
        Err(PciError::DepthExceeded)
    );
    assert_eq!(
        run(&mut chain(5), 4, BusNumbering::Assign),
        Err(PciError::DepthExceeded)
    );
    assert_eq!(
        run(&mut chain(40), MAX_BRIDGE_DEPTH, BusNumbering::Validate),
        Err(PciError::DepthExceeded)
    );
    assert_eq!(
        run(
            &mut chain(MAX_BRIDGE_DEPTH),
            MAX_BRIDGE_DEPTH,
            BusNumbering::Validate
        )
        .unwrap()
        .len(),
        usize::from(MAX_BRIDGE_DEPTH) + 1
    );
    assert_eq!(
        run(&mut chain(1), MAX_BRIDGE_DEPTH + 1, BusNumbering::Validate),
        Err(PciError::InvalidDepthLimit)
    );
    assert_eq!(
        run(&mut chain(1), 0, BusNumbering::Validate),
        Err(PciError::DepthExceeded)
    );
}

#[test]
fn assignment_runs_out_of_bus_numbers() {
    let segment = EcamSegment::new(0xE000_0000, 0, 0, 2).unwrap();
    let mut model = Model::new(segment);
    for device in 1..=3 {
        model.add_bridge(0, device, 0, Func::bridge(0x1B36, 0x000C));
    }
    assert_eq!(
        run(&mut model, 4, BusNumbering::Assign),
        Err(PciError::BusExhausted)
    );
}

#[test]
fn assignment_detects_bridge_that_ignores_writes() {
    let mut model = root_with_bridges(&[]);
    let mut stuck = Func::bridge(0x1B36, 0x000C);
    stuck.writable[0x19] = 0;
    model.add_bridge(0, 1, 0, stuck);
    assert_eq!(
        run(&mut model, 4, BusNumbering::Assign),
        Err(PciError::BridgeNotProgrammed)
    );
}

#[test]
fn enumeration_range_starting_above_zero() {
    let segment = EcamSegment::new(0xC000_0000, 1, 0x80, 0x83).unwrap();
    let mut model = Model::new(segment);
    model.add(0, 0, 0, Func::endpoint(0x8086, 0x29C0, 0x06, 0, 0));
    let behind = model.add_bridge(0, 1, 0, Func::bridge(0x1B36, 0x000C));
    model.add(behind, 0, 0, Func::endpoint(0x1AF4, 0x1042, 0x01, 0, 0));
    let assigned = run(&mut model, 4, BusNumbering::Assign).unwrap();
    assert_eq!(assigned[1].bridge, buses(0x80, 0x81, 0x81));
    assert_eq!(assigned[2].bdf, bdf(0x81, 0, 0));
    assert_eq!(
        run(&mut model, 4, BusNumbering::Validate).unwrap(),
        assigned
    );
    assert_eq!(BusRange::new(0x80, 0x83).unwrap(), model.segment.buses());
}

#[test]
fn functions_beyond_zero_need_multi_function_bit() {
    let mut model = Model::new(segment_from(Q35_MCFG));
    model.add(0, 5, 0, Func::endpoint(0x8086, 0x2918, 0x06, 0x01, 0));
    model.add(0, 5, 1, Func::endpoint(0x8086, 0x2922, 0x01, 0x06, 0x01));
    // Function 1 without function 0 is not a device.
    model.add(0, 6, 1, Func::endpoint(0x8086, 0x2930, 0x0C, 0x05, 0));
    let functions = run(&mut model, 4, BusNumbering::Validate).unwrap();
    assert_eq!(
        functions.iter().map(|f| f.bdf).collect::<Vec<_>>(),
        vec![bdf(0, 5, 0)]
    );

    model.func_mut(0, 5, 0).unwrap().cfg[0x0E] |= 0x80;
    let functions = run(&mut model, 4, BusNumbering::Validate).unwrap();
    assert_eq!(
        functions.iter().map(|f| f.bdf).collect::<Vec<_>>(),
        vec![bdf(0, 5, 0), bdf(0, 5, 1)]
    );
}

#[test]
fn unknown_header_layout_and_vendor_zero() {
    let mut model = Model::new(segment_from(Q35_MCFG));
    let mut odd = Func::endpoint(0x1234, 0x0001, 0xFF, 0, 0);
    odd.cfg[0x0E] = 0x03;
    model.add(0, 7, 0, odd);
    assert_eq!(
        run(&mut model, 4, BusNumbering::Validate),
        Err(PciError::UnsupportedHeaderType(3))
    );

    let mut model = Model::new(segment_from(Q35_MCFG));
    model.add(0, 7, 0, Func::endpoint(0x0000, 0x0001, 0xFF, 0, 0));
    assert_eq!(run(&mut model, 4, BusNumbering::Validate).unwrap().len(), 0);
}
