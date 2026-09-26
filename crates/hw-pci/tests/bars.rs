mod common;

use common::{bdf, segment_from, Func, Model, Q35_MCFG};
use hw_pci::{
    decode_bar_type, probe_bar, probe_bars, regs, Bar, BarKind, BarSlot, HeaderKind, PciError,
};

fn single(f: Func) -> Model {
    let mut model = Model::new(segment_from(Q35_MCFG));
    model.add(0, 4, 0, f);
    model
}

fn snapshot(model: &Model) -> Vec<u8> {
    model.func(0, 4, 0).unwrap().cfg.clone()
}

const COMMAND_ON: u16 = 0x0007;

#[test]
fn decodes_type_bits() {
    assert_eq!(decode_bar_type(0x0000_0001), Ok((BarKind::Io, false)));
    assert_eq!(decode_bar_type(0x0000_0000), Ok((BarKind::Memory32, false)));
    assert_eq!(decode_bar_type(0x0000_0008), Ok((BarKind::Memory32, true)));
    assert_eq!(decode_bar_type(0x0000_000C), Ok((BarKind::Memory64, true)));
    assert_eq!(decode_bar_type(0x0000_0002), Err(PciError::BarReservedType));
    assert_eq!(decode_bar_type(0x0000_0006), Err(PciError::BarReservedType));
}

#[test]
fn sizes_every_kind_and_restores_state() {
    let f = Func::endpoint(0x1AF4, 0x1042, 1, 0, 0)
        .command(COMMAND_ON)
        .io(0, 32, 0xC040, true)
        .mem32(1, 4096, 0xC100_1000, false)
        .mem64(2, 8 << 30, 0x40_0000_0000, true)
        .mem32(5, 1 << 20, 0, true);
    let mut model = single(f);
    let before = snapshot(&model);
    let bars = probe_bars(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint).unwrap();
    assert_eq!(
        snapshot(&model),
        before,
        "sizing must leave config space as found"
    );

    assert_eq!(
        bars.slots(),
        &[
            BarSlot::Bar(Bar {
                index: 0,
                kind: BarKind::Io,
                prefetchable: false,
                address: 0xC040,
                size: 32
            }),
            BarSlot::Bar(Bar {
                index: 1,
                kind: BarKind::Memory32,
                prefetchable: false,
                address: 0xC100_1000,
                size: 4096
            }),
            BarSlot::Bar(Bar {
                index: 2,
                kind: BarKind::Memory64,
                prefetchable: true,
                address: 0x40_0000_0000,
                size: 8 << 30
            }),
            BarSlot::Upper64,
            BarSlot::Unimplemented,
            BarSlot::Bar(Bar {
                index: 5,
                kind: BarKind::Memory32,
                prefetchable: true,
                address: 0,
                size: 1 << 20
            }),
        ]
    );
    assert_eq!(bars.iter().count(), 4);
    assert!(bars.get(3).is_none());
    assert_eq!(bars.get(2).unwrap().size, 8 << 30);
}

#[test]
fn decode_is_disabled_while_all_ones_are_written() {
    let mut model = single(
        Func::endpoint(0x8086, 0x10D3, 2, 0, 0)
            .command(COMMAND_ON)
            .mem64(0, 128 << 10, 0xFEB0_0000, false),
    );
    probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0).unwrap();
    let writes = &model.writes;
    let first_ones = writes.iter().position(|w| w.value == u32::MAX).unwrap();
    let disable = writes[..first_ones]
        .iter()
        .rposition(|w| w.offset == regs::COMMAND)
        .expect("command written before all-ones");
    assert_eq!(writes[disable].value & 0x3, 0, "I/O and memory decode off");
    assert_eq!(writes[disable].value & 0x4, 0x4, "bus master untouched");
    let last = writes.last().unwrap();
    assert_eq!(
        (last.offset, last.value),
        (regs::COMMAND, u32::from(COMMAND_ON))
    );
    // Both halves of the 64-bit BAR were sized and restored.
    assert_eq!(
        writes.iter().filter(|w| w.offset == 0x14).count(),
        2,
        "upper half: all ones then original"
    );
}

#[test]
fn unimplemented_bar_reads_as_zero() {
    let mut model = single(Func::endpoint(0x8086, 0x2930, 0x0C, 5, 0));
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 3),
        Ok(BarSlot::Unimplemented)
    );
}

#[test]
fn bar_index_outside_header_is_rejected_without_access() {
    let mut model = single(Func::bridge(0x1B36, 0x000C));
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::PciBridge, 2),
        Err(PciError::BarIndex)
    );
    assert_eq!(model.reads, 0);
    assert!(model.writes.is_empty());
}

#[test]
fn bit64_bar_in_last_slot_is_rejected_before_any_write() {
    let mut f = Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0);
    f.set32(0x24, 0x0000_0004);
    f.bar_masks[5] = 0xFFFF_F000;
    let mut model = single(f);
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 5),
        Err(PciError::Bar64InLastSlot)
    );
    assert!(model.writes.is_empty());

    // Same on a bridge, whose last slot is 1.
    let mut bridge = Func::bridge(0x1B36, 0x000C);
    bridge.set32(0x14, 0x0000_000C);
    let mut model = single(bridge);
    assert_eq!(
        probe_bars(&mut model, bdf(0, 4, 0), HeaderKind::PciBridge),
        Err(PciError::Bar64InLastSlot)
    );
    // Slot 0 was sized normally; the bad slot 1 was never written.
    assert!(model.writes.iter().all(|w| w.offset != 0x14));
}

#[test]
fn reserved_memory_type_is_rejected_before_any_write() {
    let mut f = Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0);
    f.set32(0x10, 0xFEB0_0002);
    let mut model = single(f);
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0),
        Err(PciError::BarReservedType)
    );
    assert!(model.writes.is_empty());
}

#[test]
fn bar_that_keeps_sizing_value_is_reported_and_command_restored() {
    let mut f = Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0)
        .command(COMMAND_ON)
        .mem32(0, 4096, 0xFEB0_0000, false);
    f.sticky_bars = true;
    let mut model = single(f);
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0),
        Err(PciError::BarNotRestored)
    );
    let f = model.func(0, 4, 0).unwrap();
    assert_eq!(
        f.get16(0x04),
        COMMAND_ON,
        "command restored despite the fault"
    );
}

#[test]
fn command_that_refuses_restore_is_reported() {
    let mut f = Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0)
        .command(COMMAND_ON)
        .mem32(0, 4096, 0xFEB0_0000, false);
    f.sticky_command = true;
    let mut model = single(f);
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0),
        Err(PciError::CommandNotRestored)
    );
    assert_eq!(model.func(0, 4, 0).unwrap().get32(0x10), 0xFEB0_0000);
}

fn probe_with_response(
    original: u32,
    mask: u32,
    response: u32,
) -> (Result<BarSlot, PciError>, Model, Vec<u8>) {
    let mut f = Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0).command(COMMAND_ON);
    f.set32(0x10, original);
    f.bar_masks[0] = mask;
    f.bar_response[0] = Some(response);
    let mut model = single(f);
    let before = snapshot(&model);
    let result = probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0);
    (result, model, before)
}

#[test]
fn inconsistent_responses_are_errors_and_state_is_restored() {
    let cases = [
        // Non-contiguous writable bits.
        (0xFEB0_0000, 0xFFFF_F000, 0xFF0F_F000),
        // Type bits changed between original and response.
        (0xFEB0_0000, 0xFFFF_F000, 0xFFFF_F008),
        // Memory BAR with no writable address bits.
        (0xFEB0_0000, 0xFFFF_F000, 0x0000_0000),
        // Address not aligned to the decoded size.
        (0xFEB0_0800, 0xFFFF_F000, 0xFFFF_F000),
        // I/O BAR whose response lost the I/O bit.
        (0x0000_C001, 0xFFFF_FFE0, 0xFFFF_FFE0),
        // I/O BAR with nothing writable.
        (0x0000_C001, 0xFFFF_FFE0, 0x0000_0001),
    ];
    for (original, mask, response) in cases {
        let (result, model, before) = probe_with_response(original, mask, response);
        assert_eq!(
            result,
            Err(PciError::BarResponse),
            "original {original:#x} response {response:#x}"
        );
        assert_eq!(snapshot(&model), before, "restored for {response:#x}");
    }
}

#[test]
fn sixteen_bit_and_full_width_io_decoders() {
    let (result, _, _) = probe_with_response(0x0000_E001, 0, 0x0000_FF01);
    assert_eq!(
        result,
        Ok(BarSlot::Bar(Bar {
            index: 0,
            kind: BarKind::Io,
            prefetchable: false,
            address: 0xE000,
            size: 256
        }))
    );
    let (result, _, _) = probe_with_response(0x0000_E001, 0, 0xFFFF_FF01);
    assert_eq!(
        result.map(|slot| match slot {
            BarSlot::Bar(bar) => bar.size,
            _ => 0,
        }),
        Ok(256)
    );
}

#[test]
fn bar64_ending_at_top_of_address_space_is_accepted() {
    let mut model = single(Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0).mem64(
        0,
        4 << 30,
        0xFFFF_FFFF_0000_0000,
        true,
    ));
    let slot = probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0).unwrap();
    let BarSlot::Bar(bar) = slot else {
        panic!("expected a BAR, got {slot:?}")
    };
    assert_eq!((bar.address, bar.size), (0xFFFF_FFFF_0000_0000, 4 << 30));
    assert_eq!(bar.address.checked_add(bar.size - 1), Some(u64::MAX));
}

#[test]
fn bar64_misaligned_to_its_huge_size_is_rejected() {
    // Upper half decodes only bit 63: an 8 EiB BAR cannot sit at 0x4000_0000_0000_0000.
    let mut f = Func::endpoint(0x1234, 0x5678, 0xFF, 0, 0);
    f.set32(0x10, 0x0000_0004);
    f.set32(0x14, 0x4000_0000);
    f.bar_response[0] = Some(0x0000_0004);
    f.bar_response[1] = Some(0x8000_0000);
    let mut model = single(f);
    assert_eq!(
        probe_bar(&mut model, bdf(0, 4, 0), HeaderKind::Endpoint, 0),
        Err(PciError::BarResponse)
    );
}

#[test]
fn bridge_has_two_bar_slots() {
    let mut model = single(Func::bridge(0x1B36, 0x000C).mem32(0, 4096, 0xC100_2000, false));
    let bars = probe_bars(&mut model, bdf(0, 4, 0), HeaderKind::PciBridge).unwrap();
    assert_eq!(bars.slots().len(), 2);
    assert_eq!(bars.get(0).unwrap().size, 4096);
    assert_eq!(bars.slots()[1], BarSlot::Unimplemented);
    // Bridge bus registers after the BARs were not touched.
    assert!(model.writes.iter().all(|w| w.offset < 0x18));
}
