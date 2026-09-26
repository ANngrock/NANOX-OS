//! Deterministic randomized inputs: every API must return Ok or Err, never
//! panic, never pass an out-of-contract offset to `ConfigSpace`, and every
//! successful result must satisfy its documented invariants.

mod common;

use common::{lenovo, q35, Model};
use hw_pci::{
    capabilities, enumerate, extended_capabilities, probe_bars, read_header, Bars, Bdf,
    BusNumbering, BusRange, ConfigSpace, EnumerationConfig, Function, HeaderKind, Msi, MsiX,
    PcieCapability, CAP_ID_MSI, CAP_ID_MSIX, CAP_ID_PCIE, MAX_BRIDGE_DEPTH,
};
use std::collections::{BTreeSet, HashMap};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^ (x >> 33)
}

/// Configuration space made of hashed noise, biased toward values that let
/// parsers progress (known header types, capability lists, aligned pointers).
/// Writes are remembered per byte so read-back after a write is coherent.
struct Noise {
    seed: u64,
    written: HashMap<(Bdf, u16), u8>,
    accesses: u64,
}

impl Noise {
    fn byte(&mut self, bdf: Bdf, offset: u16) -> u8 {
        if let Some(value) = self.written.get(&(bdf, offset)) {
            return *value;
        }
        let key = (u64::from(bdf.bus()) << 24)
            | (u64::from(bdf.device()) << 19)
            | (u64::from(bdf.function()) << 16)
            | u64::from(offset);
        let h = mix(self.seed ^ mix(key));
        let raw = h as u8;
        let bias = (h >> 8) & 3 != 0;
        match offset {
            // Mostly present, sometimes absent.
            0x00 | 0x01 if (h >> 10) & 1 == 0 => 0xFF,
            0x06 if bias => raw | 0x10,
            0x0E if bias => raw & 0x81,
            0x34 if bias => raw & 0xFC,
            0x41..=0xFF if offset % 4 == 1 && bias => raw & 0xFC,
            0x102..=0xFFF if offset % 4 == 3 && bias => (raw & 0xF0) | 0x01,
            _ => raw,
        }
    }

    fn check(&mut self, offset: u16, width: u16) {
        self.accesses += 1;
        assert!(offset.is_multiple_of(width) && offset + width <= 4096);
        assert!(self.accesses < 5_000_000, "unbounded access loop");
    }

    fn read(&mut self, bdf: Bdf, offset: u16, width: u16) -> u32 {
        self.check(offset, width);
        (0..width).fold(0, |value, k| {
            value | (u32::from(self.byte(bdf, offset + k)) << (8 * k))
        })
    }

    fn write(&mut self, bdf: Bdf, offset: u16, width: u16, value: u32) {
        self.check(offset, width);
        for k in 0..width {
            self.written
                .insert((bdf, offset + k), (value >> (8 * k)) as u8);
        }
    }
}

impl ConfigSpace for Noise {
    fn read_u8(&mut self, bdf: Bdf, offset: u16) -> u8 {
        self.read(bdf, offset, 1) as u8
    }
    fn read_u16(&mut self, bdf: Bdf, offset: u16) -> u16 {
        self.read(bdf, offset, 2) as u16
    }
    fn read_u32(&mut self, bdf: Bdf, offset: u16) -> u32 {
        self.read(bdf, offset, 4)
    }
    fn write_u8(&mut self, bdf: Bdf, offset: u16, value: u8) {
        self.write(bdf, offset, 1, value.into())
    }
    fn write_u16(&mut self, bdf: Bdf, offset: u16, value: u16) {
        self.write(bdf, offset, 2, value.into())
    }
    fn write_u32(&mut self, bdf: Bdf, offset: u16, value: u32) {
        self.write(bdf, offset, 4, value)
    }
}

fn check_bars(bars: &Bars) {
    for bar in bars.iter() {
        assert!(bar.size.is_power_of_two(), "{bar:?}");
        assert_eq!(bar.address % bar.size, 0, "{bar:?}");
        assert!(bar.address.checked_add(bar.size - 1).is_some());
        let floor = if bar.kind.is_memory() { 16 } else { 4 };
        assert!(bar.size >= floor, "{bar:?}");
    }
}

/// Run every per-function parser; results are checked, errors are fine.
fn exercise_function<C: ConfigSpace>(cfg: &mut C, bdf: Bdf, rng: &mut Rng) {
    let kind = match read_header(cfg, bdf) {
        Ok(Some(header)) => header.kind,
        Ok(None) => [HeaderKind::Endpoint, HeaderKind::PciBridge][rng.below(2) as usize],
        Err(_) => HeaderKind::CardBus,
    };
    let bars = probe_bars(cfg, bdf, kind);
    if let Ok(bars) = &bars {
        check_bars(bars);
    }
    let caps: Vec<_> = capabilities(cfg, bdf, kind).collect();
    assert!(caps.len() <= 49);
    assert!(caps
        .iter()
        .take(caps.len().saturating_sub(1))
        .all(Result::is_ok));
    let mut offsets: Vec<u8> = caps.iter().flatten().map(|c| c.offset).collect();
    let unique: BTreeSet<u8> = offsets.iter().copied().collect();
    assert_eq!(unique.len(), offsets.len(), "capability visited twice");
    offsets.push(rng.below(256) as u8);
    for (id, offset) in caps
        .iter()
        .flatten()
        .map(|c| (c.id, c.offset))
        .chain(offsets.last().map(|o| (0, *o)))
    {
        if id == CAP_ID_MSI || id == 0 {
            if let Ok(msi) = Msi::read(cfg, bdf, offset) {
                assert!(msi.vectors_enabled <= msi.vectors_capable && msi.vectors_capable <= 32);
                let end = msi.pending_offset.unwrap_or(msi.data_offset);
                assert!(end + 2 <= 256);
            }
        }
        if id == CAP_ID_MSIX || id == 0 {
            if let Ok(bars) = &bars {
                if let Ok(msix) = MsiX::read(cfg, bdf, offset, bars) {
                    for region in [msix.table, msix.pba] {
                        let bar = bars.get(region.bir).expect("BIR names a BAR");
                        assert!(bar.kind.is_memory());
                        assert!(u64::from(region.offset) + region.length <= bar.size);
                    }
                }
            }
        }
        if id == CAP_ID_PCIE || id == 0 {
            if let Ok(pcie) = PcieCapability::read(cfg, bdf, offset) {
                assert!(pcie.version == 1 || pcie.version == 2);
            }
        }
    }
    let ext: Vec<_> = extended_capabilities(cfg, bdf).collect();
    assert!(ext.len() <= 961);
    let ext_offsets: BTreeSet<u16> = ext.iter().flatten().map(|c| c.offset).collect();
    assert_eq!(ext_offsets.len(), ext.iter().flatten().count());
}

fn check_enumeration(functions: &[Function], range: BusRange, numbering: BusNumbering) {
    let bdfs: BTreeSet<Bdf> = functions.iter().map(|f| f.bdf).collect();
    assert_eq!(bdfs.len(), functions.len(), "function reported twice");
    let mut secondaries = BTreeSet::new();
    for f in functions {
        assert!(range.contains(f.bdf.bus()));
        assert!(f.depth <= MAX_BRIDGE_DEPTH);
        assert_eq!(f.bridge.is_some(), f.header.kind == HeaderKind::PciBridge);
        if let Some(b) = f.bridge {
            assert_eq!(b.primary, f.bdf.bus());
            assert!(b.primary < b.secondary && b.secondary <= b.subordinate);
            assert!(secondaries.insert(b.secondary), "bus scanned twice");
        }
        if let Some(parent) = f.parent {
            let p = functions
                .iter()
                .find(|p| p.bdf == parent)
                .expect("parent listed");
            let b = p.bridge.expect("parent is a bridge");
            assert_eq!(f.bdf.bus(), b.secondary);
            if numbering == BusNumbering::Validate {
                assert_eq!(f.depth, p.depth + 1);
            }
        }
    }
}

fn exercise_enumeration<C: ConfigSpace>(cfg: &mut C, range: BusRange, rng: &mut Rng) {
    let numbering = [BusNumbering::Validate, BusNumbering::Assign][rng.below(2) as usize];
    let config = EnumerationConfig {
        buses: range,
        max_depth: rng.below(u64::from(MAX_BRIDGE_DEPTH) + 2) as u8,
        numbering,
    };
    let mut out = vec![Function::default(); rng.below(80) as usize];
    if let Ok(count) = enumerate(cfg, &config, &mut out) {
        assert!(config.max_depth <= MAX_BRIDGE_DEPTH);
        assert!(count <= out.len());
        check_enumeration(&out[..count], range, numbering);
    }
}

#[test]
fn noise_config_space_never_panics() {
    let mut rng = Rng(0x4E41_4E4F_5850_4349);
    for round in 0..400 {
        let mut noise = Noise {
            seed: rng.next(),
            written: HashMap::new(),
            accesses: 0,
        };
        for _ in 0..8 {
            let bdf = Bdf::new(
                rng.below(256) as u8,
                rng.below(32) as u8,
                rng.below(8) as u8,
            )
            .unwrap();
            exercise_function(&mut noise, bdf, &mut rng);
        }
        let start = rng.below(256) as u8;
        let end = start.saturating_add(rng.below(64) as u8);
        let range = BusRange::new(start, end).unwrap();
        exercise_enumeration(&mut noise, range, &mut rng);
        assert!(noise.accesses > 0, "round {round}");
    }
}

fn mutate(model: &mut Model, rng: &mut Rng, flips: usize) {
    let targets: Vec<(usize, (u8, u8))> = model
        .buses
        .iter()
        .enumerate()
        .flat_map(|(phys, bus)| bus.keys().map(move |key| (phys, *key)))
        .collect();
    for _ in 0..flips {
        let (phys, key) = targets[rng.below(targets.len() as u64) as usize];
        let f = model.buses[phys].get_mut(&key).unwrap();
        match rng.below(8) {
            0 => {
                let slot = rng.below(6) as usize;
                f.bar_masks[slot] ^= 1 << rng.below(32);
            }
            1 => {
                let offset = 0x100 + rng.below(0xF00) as usize;
                f.cfg[offset] ^= 1 << rng.below(8);
            }
            2 => {
                let offset = [0x06, 0x0E, 0x18, 0x19, 0x1A, 0x34][rng.below(6) as usize];
                f.cfg[offset] = rng.next() as u8;
            }
            _ => {
                let offset = rng.below(0x100) as usize;
                f.cfg[offset] ^= 1 << rng.below(8);
            }
        }
    }
}

#[test]
fn mutated_topologies_never_panic() {
    let mut rng = Rng(0x5043_4930_4D39_0001);
    let mut successes = 0;
    for round in 0..600 {
        let program = rng.below(2) == 0;
        let mut model = if round % 2 == 0 {
            q35(program)
        } else {
            lenovo(program)
        };
        let flips = 1 + rng.below(12) as usize;
        mutate(&mut model, &mut rng, flips);
        let range = model.segment.buses();
        let numbering = if program {
            BusNumbering::Validate
        } else {
            BusNumbering::Assign
        };
        let config = EnumerationConfig {
            buses: range,
            max_depth: 4,
            numbering,
        };
        let mut out = [Function::default(); 32];
        if let Ok(count) = enumerate(&mut model, &config, &mut out) {
            successes += 1;
            check_enumeration(&out[..count], range, numbering);
            for f in &out[..count] {
                exercise_function(&mut model, f.bdf, &mut rng);
            }
        }
        exercise_enumeration(&mut model, range, &mut rng);
    }
    // The mutations must not be so destructive that nothing is exercised.
    assert!(successes > 50, "only {successes} successful walks");
}
