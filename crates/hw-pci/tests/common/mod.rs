//! Configuration space model shared by the integration tests.
//!
//! Devices sit on physical buses; a bus number reaches a physical bus only
//! through bridges whose secondary/subordinate registers route it, as on real
//! hardware, so bus-number assignment changes what is reachable. Accesses go
//! through `EcamSegment::address`/`decode`; unreachable functions read all
//! ones. Every access asserts the offset contract documented on
//! `ConfigSpace`, so a contract violation by the crate fails the test.

#![allow(dead_code)]

use hw_pci::{AccessWidth, Bdf, ConfigSpace, EcamSegment};
use std::collections::BTreeMap;

pub const CLASS_BRIDGE: u8 = 0x06;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    pub bdf: Bdf,
    pub offset: u16,
    pub width: u16,
    pub value: u32,
}

#[derive(Clone)]
pub struct Func {
    pub cfg: Vec<u8>,
    pub writable: Vec<u8>,
    pub bar_masks: [u32; 6],
    pub bar_slots: usize,
    pub downstream: Option<usize>,
    /// Fault: after all ones are written to a BAR it ignores further writes.
    pub sticky_bars: bool,
    latched: [bool; 6],
    /// Fault: value returned by a BAR while it holds the all-ones pattern.
    pub bar_response: [Option<u32>; 6],
    sizing: [bool; 6],
    /// Fault: the command register accepts one write only.
    pub sticky_command: bool,
    command_written: bool,
    last_cap: u8,
    last_ext: u16,
}

impl Func {
    fn blank(vendor: u16, device: u16, class: u8, subclass: u8, prog_if: u8, kind: u8) -> Self {
        let mut f = Func {
            cfg: vec![0; 4096],
            writable: vec![0; 4096],
            bar_masks: [0; 6],
            bar_slots: if kind == 1 { 2 } else { 6 },
            downstream: None,
            sticky_bars: false,
            latched: [false; 6],
            bar_response: [None; 6],
            sizing: [false; 6],
            sticky_command: false,
            command_written: false,
            last_cap: 0,
            last_ext: 0,
        };
        f.set16(0x00, vendor);
        f.set16(0x02, device);
        f.cfg[0x09] = prog_if;
        f.cfg[0x0A] = subclass;
        f.cfg[0x0B] = class;
        f.cfg[0x0E] = kind;
        f.writable[0x04] = 0xFF;
        f.writable[0x05] = 0x07;
        f
    }

    pub fn endpoint(vendor: u16, device: u16, class: u8, subclass: u8, prog_if: u8) -> Self {
        Self::blank(vendor, device, class, subclass, prog_if, 0)
    }

    /// Type 1 header with writable bus-number registers, initially zero.
    pub fn bridge(vendor: u16, device: u16) -> Self {
        let mut f = Self::blank(vendor, device, CLASS_BRIDGE, 0x04, 0, 1);
        for offset in 0x18..0x1C {
            f.writable[offset] = 0xFF;
        }
        f
    }

    pub fn set16(&mut self, offset: usize, value: u16) {
        self.cfg[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    pub fn set32(&mut self, offset: usize, value: u32) {
        self.cfg[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    pub fn get32(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.cfg[offset..offset + 4].try_into().unwrap())
    }

    pub fn get16(&self, offset: usize) -> u16 {
        u16::from_le_bytes(self.cfg[offset..offset + 2].try_into().unwrap())
    }

    pub fn multi(mut self) -> Self {
        self.cfg[0x0E] |= 0x80;
        self
    }

    pub fn command(mut self, value: u16) -> Self {
        self.set16(0x04, value);
        self
    }

    pub fn buses(mut self, primary: u8, secondary: u8, subordinate: u8) -> Self {
        self.cfg[0x18] = primary;
        self.cfg[0x19] = secondary;
        self.cfg[0x1A] = subordinate;
        self
    }

    pub fn mem32(mut self, index: usize, size: u32, address: u32, prefetchable: bool) -> Self {
        let flags = if prefetchable { 0x8 } else { 0 };
        self.bar_masks[index] = !(size - 1) & !0xF;
        self.set32(0x10 + 4 * index, address | flags);
        self
    }

    pub fn mem64(mut self, index: usize, size: u64, address: u64, prefetchable: bool) -> Self {
        let flags = 0x4 | if prefetchable { 0x8 } else { 0 };
        let mask = !(size - 1) & !0xF;
        self.bar_masks[index] = mask as u32;
        self.bar_masks[index + 1] = (mask >> 32) as u32;
        self.set32(0x10 + 4 * index, address as u32 | flags);
        self.set32(0x10 + 4 * (index + 1), (address >> 32) as u32);
        self
    }

    pub fn io(mut self, index: usize, size: u32, address: u32, sixteen_bit: bool) -> Self {
        let mut mask = !(size - 1) & !0x3;
        if sixteen_bit {
            mask &= 0xFFFF;
        }
        self.bar_masks[index] = mask;
        self.set32(0x10 + 4 * index, address | 0x1);
        self
    }

    /// Append a conventional capability; `body` starts at `offset + 2`.
    pub fn cap(mut self, offset: u8, id: u8, body: &[u8]) -> Self {
        let at = offset as usize;
        self.cfg[at] = id;
        self.cfg[at + 1] = 0;
        self.cfg[at + 2..at + 2 + body.len()].copy_from_slice(body);
        if self.last_cap == 0 {
            self.cfg[0x34] = offset;
            let status = self.get16(0x06) | 0x10;
            self.set16(0x06, status);
        } else {
            self.cfg[self.last_cap as usize + 1] = offset;
        }
        self.last_cap = offset;
        self
    }

    pub fn msi(self, offset: u8, wide: bool, masking: bool, capable_log2: u16) -> Self {
        let control =
            (capable_log2 << 1) | if wide { 1 << 7 } else { 0 } | if masking { 1 << 8 } else { 0 };
        let mut body = vec![0u8; 22];
        body[..2].copy_from_slice(&control.to_le_bytes());
        let length = match (wide, masking) {
            (false, false) => 8,
            (true, false) => 12,
            (false, true) => 18,
            (true, true) => 22,
        };
        body.truncate(length);
        self.cap(offset, 0x05, &body)
    }

    pub fn msix(self, offset: u8, entries: u16, table: (u8, u32), pba: (u8, u32)) -> Self {
        let mut body = Vec::new();
        body.extend_from_slice(&(entries - 1).to_le_bytes());
        body.extend_from_slice(&(table.1 | u32::from(table.0)).to_le_bytes());
        body.extend_from_slice(&(pba.1 | u32::from(pba.0)).to_le_bytes());
        self.cap(offset, 0x11, &body)
    }

    pub fn pcie(self, offset: u8, port_type: u16, version: u16) -> Self {
        let flags = version | (port_type << 4);
        self.cap(offset, 0x10, &flags.to_le_bytes())
    }

    /// Append an extended capability header at `offset` (>= 0x100).
    pub fn ext_cap(mut self, offset: u16, id: u16, version: u8) -> Self {
        let header = u32::from(id) | (u32::from(version) << 16);
        self.set32(offset as usize, header);
        if self.last_ext != 0 {
            let at = self.last_ext as usize;
            let prev = self.get32(at) & 0x000F_FFFF;
            self.set32(at, prev | (u32::from(offset) << 20));
        }
        self.last_ext = offset;
        self
    }

    fn bar_index(&self, offset: usize) -> Option<usize> {
        (0x10..0x10 + 4 * self.bar_slots)
            .contains(&offset)
            .then(|| (offset - 0x10) / 4)
    }

    fn read(&self, offset: usize, width: usize) -> u32 {
        if width == 4 {
            if let Some(index) = self.bar_index(offset) {
                if self.sizing[index] {
                    if let Some(response) = self.bar_response[index] {
                        return response;
                    }
                }
            }
        }
        let mut value = 0u32;
        for k in 0..width {
            value |= u32::from(self.cfg[offset + k]) << (8 * k);
        }
        value
    }

    fn write(&mut self, offset: usize, width: usize, value: u32) {
        if let Some(index) = self.bar_index(offset) {
            assert_eq!(width, 4, "BARs are written as dwords");
            if self.sticky_bars && self.latched[index] {
                return;
            }
            let mask = self.bar_masks[index];
            let stored = (self.get32(offset) & !mask) | (value & mask);
            self.set32(offset, stored);
            self.sizing[index] = value == u32::MAX;
            self.latched[index] = value == u32::MAX;
            return;
        }
        if offset == 0x04 {
            if self.sticky_command && self.command_written {
                return;
            }
            self.command_written = true;
        }
        for k in 0..width {
            let byte = (value >> (8 * k)) as u8;
            let mask = self.writable[offset + k];
            self.cfg[offset + k] = (self.cfg[offset + k] & !mask) | (byte & mask);
        }
    }
}

pub struct Model {
    pub segment: EcamSegment,
    /// Physical buses; index 0 is the root bus. Key: (device, function).
    pub buses: Vec<BTreeMap<(u8, u8), Func>>,
    pub writes: Vec<Write>,
    pub reads: usize,
}

impl Model {
    pub fn new(segment: EcamSegment) -> Self {
        Model {
            segment,
            buses: vec![BTreeMap::new()],
            writes: Vec::new(),
            reads: 0,
        }
    }

    pub fn add(&mut self, phys: usize, device: u8, function: u8, f: Func) {
        assert!(device < 32 && function < 8);
        self.buses[phys].insert((device, function), f);
    }

    /// Add a bridge and return the physical bus behind it.
    pub fn add_bridge(&mut self, phys: usize, device: u8, function: u8, mut f: Func) -> usize {
        let downstream = self.buses.len();
        self.buses.push(BTreeMap::new());
        f.downstream = Some(downstream);
        self.add(phys, device, function, f);
        downstream
    }

    /// Physical bus currently reached by bus number `bus`.
    pub fn resolve(&self, bus: u8) -> Option<usize> {
        if bus == self.segment.buses().start() {
            return Some(0);
        }
        let mut phys = 0;
        for _ in 0..self.buses.len() {
            let (downstream, secondary) = self.buses[phys].values().find_map(|f| {
                let (secondary, subordinate) = (f.cfg[0x19], f.cfg[0x1A]);
                let downstream = f.downstream?;
                (secondary != 0 && secondary <= bus && bus <= subordinate)
                    .then_some((downstream, secondary))
            })?;
            if secondary == bus {
                return Some(downstream);
            }
            phys = downstream;
        }
        None
    }

    pub fn func(&self, bus: u8, device: u8, function: u8) -> Option<&Func> {
        self.buses[self.resolve(bus)?].get(&(device, function))
    }

    pub fn func_mut(&mut self, bus: u8, device: u8, function: u8) -> Option<&mut Func> {
        let phys = self.resolve(bus)?;
        self.buses[phys].get_mut(&(device, function))
    }

    fn locate(&mut self, bdf: Bdf, offset: u16, width: AccessWidth) -> Option<(&mut Func, usize)> {
        let size = width.bytes();
        assert!(
            offset.is_multiple_of(size) && offset + size <= 4096,
            "crate passed offset {offset:#x} width {size}"
        );
        let address = self.segment.address(bdf, offset, width).ok()?;
        let (decoded, register) = self
            .segment
            .decode(address)
            .expect("decode inverts address");
        assert_eq!((decoded, register), (bdf, offset));
        let phys = self.resolve(bdf.bus())?;
        let f = self.buses[phys].get_mut(&(bdf.device(), bdf.function()))?;
        Some((f, usize::from(offset)))
    }

    fn read(&mut self, bdf: Bdf, offset: u16, width: AccessWidth) -> u32 {
        self.reads += 1;
        let bytes = usize::from(width.bytes());
        match self.locate(bdf, offset, width) {
            Some((f, at)) => f.read(at, bytes),
            None => u32::MAX >> (32 - 8 * bytes),
        }
    }

    fn write(&mut self, bdf: Bdf, offset: u16, width: AccessWidth, value: u32) {
        self.writes.push(Write {
            bdf,
            offset,
            width: width.bytes(),
            value,
        });
        let bytes = usize::from(width.bytes());
        if let Some((f, at)) = self.locate(bdf, offset, width) {
            f.write(at, bytes, value);
        }
    }
}

impl ConfigSpace for Model {
    fn read_u8(&mut self, bdf: Bdf, offset: u16) -> u8 {
        self.read(bdf, offset, AccessWidth::Byte) as u8
    }
    fn read_u16(&mut self, bdf: Bdf, offset: u16) -> u16 {
        self.read(bdf, offset, AccessWidth::Word) as u16
    }
    fn read_u32(&mut self, bdf: Bdf, offset: u16) -> u32 {
        self.read(bdf, offset, AccessWidth::Dword)
    }
    fn write_u8(&mut self, bdf: Bdf, offset: u16, value: u8) {
        self.write(bdf, offset, AccessWidth::Byte, value.into())
    }
    fn write_u16(&mut self, bdf: Bdf, offset: u16, value: u16) {
        self.write(bdf, offset, AccessWidth::Word, value.into())
    }
    fn write_u32(&mut self, bdf: Bdf, offset: u16, value: u32) {
        self.write(bdf, offset, AccessWidth::Dword, value)
    }
}

pub fn bdf(bus: u8, device: u8, function: u8) -> Bdf {
    Bdf::new(bus, device, function).unwrap()
}

/// (base, segment, start bus, end bus) of the first MCFG allocation.
pub fn mcfg_allocation(table: &[u8]) -> (u64, u16, u8, u8) {
    assert_eq!(&table[0..4], b"MCFG");
    let length = u32::from_le_bytes(table[4..8].try_into().unwrap()) as usize;
    assert_eq!(length, table.len());
    assert_eq!(table.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
    let entry = &table[44..60];
    (
        u64::from_le_bytes(entry[0..8].try_into().unwrap()),
        u16::from_le_bytes(entry[8..10].try_into().unwrap()),
        entry[10],
        entry[11],
    )
}

pub const Q35_MCFG: &[u8] = include_bytes!("../../../../tests/fixtures/acpi/q35-smp4/MCFG.bin");
pub const LENOVO_MCFG: &[u8] =
    include_bytes!("../../../../tests/fixtures/acpi/lenovo-82k8/MCFG.bin");

pub fn segment_from(table: &[u8]) -> EcamSegment {
    let (base, segment, start, end) = mcfg_allocation(table);
    EcamSegment::new(base, segment, start, end).unwrap()
}

/// q35-like topology. IDs follow QEMU devices; BAR addresses and capability
/// offsets are illustrative, not captured from a running guest.
///
/// bus 0: 00:00.0 host bridge 8086:29C0, 00:01.0 VGA 1234:1111,
///        00:02.0 virtio-blk 1AF4:1042 (MSI-X in BAR1),
///        00:03.0 PCIe root port 1B36:000C -> bus 1,
///        00:1F.0/2/3 ICH9 LPC 8086:2918, AHCI 8086:2922, SMBus 8086:2930
/// bus 1: 01:00.0 PCIe-to-PCI bridge 1B36:000E -> bus 2
/// bus 2: 02:01.0 virtio-net 1AF4:1041 (I/O BAR, MSI-X)
pub fn q35(program_buses: bool) -> Model {
    let mut model = Model::new(segment_from(Q35_MCFG));
    model.add(0, 0, 0, Func::endpoint(0x8086, 0x29C0, 0x06, 0x00, 0));
    model.add(
        0,
        1,
        0,
        Func::endpoint(0x1234, 0x1111, 0x03, 0x00, 0)
            .mem32(0, 16 << 20, 0xC000_0000, true)
            .mem32(2, 4096, 0xC100_0000, false),
    );
    model.add(
        0,
        2,
        0,
        Func::endpoint(0x1AF4, 0x1042, 0x01, 0x00, 0)
            .command(0x0007)
            .mem32(1, 4096, 0xC100_1000, false)
            .mem64(4, 16 << 10, 0x80_0000_0000, true)
            .msix(0x98, 2, (1, 0), (1, 0x800))
            .cap(0x84, 0x09, &[0x14, 0x05])
            .cap(0x70, 0x09, &[0x14, 0x04])
            .cap(0x60, 0x09, &[0x10, 0x02])
            .cap(0x50, 0x09, &[0x10, 0x01]),
    );
    let mut port = Func::bridge(0x1B36, 0x000C)
        .mem32(0, 4096, 0xC100_2000, false)
        .pcie(0x40, 0x4, 2)
        .msix(0x80, 1, (0, 0), (0, 0x800))
        .ext_cap(0x100, 0x0001, 2)
        .ext_cap(0x148, 0x000D, 1);
    if program_buses {
        port = port.buses(0, 1, 2);
    }
    let bus1 = model.add_bridge(0, 3, 0, port);
    let mut pci_bridge = Func::bridge(0x1B36, 0x000E).pcie(0x40, 0x7, 2);
    if program_buses {
        pci_bridge = pci_bridge.buses(1, 2, 2);
    }
    let bus2 = model.add_bridge(bus1, 0, 0, pci_bridge);
    model.add(
        bus2,
        1,
        0,
        Func::endpoint(0x1AF4, 0x1041, 0x02, 0x00, 0)
            .io(0, 32, 0xC000, true)
            .mem32(1, 4096, 0xC120_0000, false)
            .msix(0x98, 3, (1, 0), (1, 0x800)),
    );
    model.add(
        0,
        0x1F,
        0,
        Func::endpoint(0x8086, 0x2918, 0x06, 0x01, 0).multi(),
    );
    model.add(
        0,
        0x1F,
        2,
        Func::endpoint(0x8086, 0x2922, 0x01, 0x06, 0x01)
            .mem32(5, 4096, 0xC100_3000, false)
            .msi(0x80, true, false, 0)
            .cap(0xA8, 0x12, &[0x10, 0x00]),
    );
    model.add(
        0,
        0x1F,
        3,
        Func::endpoint(0x8086, 0x2930, 0x0C, 0x05, 0).io(4, 64, 0x0700, true),
    );
    model
}

/// Model of the Lenovo 82K8 candidate (docs/specs/M9-HARDWARE.md §2.2).
/// Vendor/device IDs of endpoints come from that list; the root complex IDs
/// 1022:1630/1631/1632/1635, bus numbers, BARs and capability layout are a
/// plausible Cezanne arrangement, not a capture from the machine.
pub fn lenovo(program_buses: bool) -> Model {
    let mut model = Model::new(segment_from(LENOVO_MCFG));
    model.add(
        0,
        0,
        0,
        Func::endpoint(0x1022, 0x1630, 0x06, 0x00, 0).multi(),
    );
    model.add(0, 0, 2, Func::endpoint(0x1022, 0x1631, 0x08, 0x06, 0));
    let root_port = |device: u16, secondary: u8| {
        let f = Func::bridge(0x1022, device)
            .pcie(0x50, 0x4, 2)
            .msi(0xA0, false, false, 0);
        if program_buses {
            f.buses(0, secondary, secondary)
        } else {
            f
        }
    };
    let endpoint_pcie = |f: Func| f.pcie(0x60, 0x0, 2);

    model.add(
        0,
        1,
        0,
        Func::endpoint(0x1022, 0x1632, 0x06, 0x00, 0).multi(),
    );
    let gpu_bus = model.add_bridge(0, 1, 1, root_port(0x1633, 1));
    model.add(
        gpu_bus,
        0,
        0,
        endpoint_pcie(
            Func::endpoint(0x10DE, 0x2560, 0x03, 0x00, 0)
                .mem32(0, 16 << 20, 0xD000_0000, false)
                .mem64(1, 8 << 30, 0xFC_0000_0000, true)
                .mem64(3, 32 << 20, 0xFE_0000_0000, true)
                .io(5, 128, 0xF000, true),
        )
        .msi(0xA0, true, false, 0),
    );

    model.add(
        0,
        2,
        0,
        Func::endpoint(0x1022, 0x1632, 0x06, 0x00, 0).multi(),
    );
    let nvme0 = model.add_bridge(0, 2, 1, root_port(0x1634, 2));
    model.add(
        nvme0,
        0,
        0,
        endpoint_pcie(Func::endpoint(0x126F, 0x2263, 0x01, 0x08, 0x02).mem64(
            0,
            16 << 10,
            0xD110_0000,
            false,
        ))
        .msix(0xB0, 9, (0, 0x2000), (0, 0x3000)),
    );
    let wifi = model.add_bridge(0, 2, 2, root_port(0x1634, 3));
    model.add(
        wifi,
        0,
        0,
        endpoint_pcie(Func::endpoint(0x8086, 0x2723, 0x02, 0x80, 0).mem64(
            0,
            16 << 10,
            0xD120_0000,
            false,
        ))
        .msix(0xB0, 16, (0, 0x2000), (0, 0x3000)),
    );
    let sd = model.add_bridge(0, 2, 3, root_port(0x1634, 4));
    model.add(
        sd,
        0,
        0,
        endpoint_pcie(
            Func::endpoint(0x1217, 0x8621, 0x08, 0x05, 0x01)
                .mem32(0, 4096, 0xD130_0000, false)
                .mem32(1, 2048, 0xD130_1000, false),
        )
        .msi(0xB0, true, true, 0),
    );
    let nvme1 = model.add_bridge(0, 2, 4, root_port(0x1634, 5));
    model.add(
        nvme1,
        0,
        0,
        endpoint_pcie(Func::endpoint(0x144D, 0xA808, 0x01, 0x08, 0x02).mem64(
            0,
            16 << 10,
            0xD140_0000,
            false,
        ))
        .msix(0xB0, 33, (0, 0x3000), (0, 0x2000)),
    );

    model.add(
        0,
        8,
        0,
        Func::endpoint(0x1022, 0x1632, 0x06, 0x00, 0).multi(),
    );
    let internal = model.add_bridge(0, 8, 1, root_port(0x1635, 6));
    model.add(
        internal,
        0,
        0,
        endpoint_pcie(
            Func::endpoint(0x1002, 0x1638, 0x03, 0x00, 0)
                .multi()
                .mem64(0, 256 << 20, 0xE_0000_0000, true)
                .mem64(2, 2 << 20, 0xF0_0000_0000, true)
                .io(4, 256, 0xE000, true)
                .mem32(5, 512 << 10, 0xD150_0000, false),
        )
        .msix(0xA0, 4, (5, 0), (5, 0x1000)),
    );
    for function in [3u8, 4] {
        model.add(
            internal,
            0,
            function,
            endpoint_pcie(Func::endpoint(0x1022, 0x1639, 0x0C, 0x03, 0x30).mem64(
                0,
                1 << 20,
                0xD160_0000 + (u64::from(function) << 20),
                false,
            ))
            .msix(0xA0, 8, (0, 0xFE000), (0, 0xFF000)),
        );
    }
    model
}
