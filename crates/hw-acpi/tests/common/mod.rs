//! Shared test support: fixture loading, a physical-memory model built from
//! the capture manifests, and builders for synthetic tables.
#![allow(dead_code)]

use std::path::PathBuf;

use hw_acpi::{PhysRead, PhysReadError};

pub const PROFILES: [&str; 4] = [
    "q35-smp4",
    "q35-smp4-intel-iommu",
    "q35-smp4-amd-iommu",
    "lenovo-82k8",
];

pub fn fixture_dir(profile: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/acpi")
        .join(profile)
}

pub fn fixture(profile: &str, file: &str) -> Vec<u8> {
    let path = fixture_dir(profile).join(file);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// One `tables[]` record of a capture manifest.
#[derive(Clone, Debug)]
pub struct ManifestTable {
    pub signature: String,
    pub file: String,
    pub phys: Option<u64>,
    pub length: usize,
}

fn string_field(object: &str, key: &str) -> Option<String> {
    let at = object.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = object[at..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

fn number_field(object: &str, key: &str) -> Option<usize> {
    let at = object.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = object[at..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// Minimal reader for the flat `tables` array of the capture manifests
/// (no external JSON crate is allowed in this workspace).
pub fn manifest(profile: &str) -> Vec<ManifestTable> {
    let text = std::fs::read_to_string(fixture_dir(profile).join("manifest.json")).unwrap();
    let tables = &text[text.find("\"tables\"").expect("tables key")..];
    tables
        .split('}')
        .filter_map(|object| {
            Some(ManifestTable {
                signature: string_field(object, "signature")?,
                file: string_field(object, "file")?,
                phys: string_field(object, "phys").map(|hex| {
                    u64::from_str_radix(hex.trim_start_matches("0x"), 16).expect("hex phys")
                }),
                length: number_field(object, "length")?,
            })
        })
        .collect()
}

/// Sparse physical memory: a read succeeds only inside one region.
#[derive(Clone, Default)]
pub struct Memory {
    pub regions: Vec<(u64, Vec<u8>)>,
}

impl Memory {
    pub fn insert(&mut self, phys: u64, bytes: Vec<u8>) {
        self.regions.push((phys, bytes));
    }

    pub fn region_mut(&mut self, phys: u64) -> &mut Vec<u8> {
        &mut self
            .regions
            .iter_mut()
            .find(|(base, _)| *base == phys)
            .expect("region")
            .1
    }

    /// Place every manifest table at its captured physical address and
    /// return the RSDP address.
    pub fn from_manifest(profile: &str) -> (Self, u64) {
        let mut memory = Self::default();
        let mut rsdp = None;
        for table in manifest(profile) {
            let bytes = fixture(profile, &table.file);
            assert_eq!(bytes.len(), table.length, "{profile}/{}", table.file);
            let phys = table.phys.expect("manifest has physical addresses");
            if table.signature == "RSDP" {
                rsdp = Some(phys);
            }
            memory.insert(phys, bytes);
        }
        (memory, rsdp.expect("RSDP in manifest"))
    }
}

impl PhysRead for Memory {
    fn read(&self, phys: u64, buf: &mut [u8]) -> Result<(), PhysReadError> {
        for (base, data) in &self.regions {
            let Some(offset) = phys.checked_sub(*base) else {
                continue;
            };
            let Ok(offset) = usize::try_from(offset) else {
                continue;
            };
            if let Some(src) = offset
                .checked_add(buf.len())
                .and_then(|end| data.get(offset..end))
            {
                buf.copy_from_slice(src);
                return Ok(());
            }
        }
        Err(PhysReadError)
    }
}

pub fn fix_checksum(table: &mut [u8]) {
    table[9] = 0;
    let sum = table.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    table[9] = 0u8.wrapping_sub(sum);
}

pub fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A checksummed SDT with the given signature, revision and body.
pub fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let mut table = Vec::with_capacity(36 + body.len());
    table.extend_from_slice(signature);
    table.extend_from_slice(&((36 + body.len()) as u32).to_le_bytes());
    table.push(revision);
    table.push(0);
    table.extend_from_slice(b"NANOX ");
    table.extend_from_slice(b"SYNTHETC");
    table.extend_from_slice(&1u32.to_le_bytes());
    table.extend_from_slice(b"NXTS");
    table.extend_from_slice(&1u32.to_le_bytes());
    table.extend_from_slice(body);
    fix_checksum(&mut table);
    table
}

pub fn rsdp_v0(rsdt: u32) -> Vec<u8> {
    let mut rsdp = Vec::new();
    rsdp.extend_from_slice(b"RSD PTR ");
    rsdp.push(0);
    rsdp.extend_from_slice(b"NANOX ");
    rsdp.push(0);
    rsdp.extend_from_slice(&rsdt.to_le_bytes());
    fix_rsdp(&mut rsdp);
    rsdp
}

pub fn rsdp_v2(rsdt: u32, xsdt: u64) -> Vec<u8> {
    let mut rsdp = rsdp_v0(rsdt);
    rsdp[15] = 2;
    rsdp.extend_from_slice(&36u32.to_le_bytes());
    rsdp.extend_from_slice(&xsdt.to_le_bytes());
    rsdp.extend_from_slice(&[0; 4]);
    fix_rsdp(&mut rsdp);
    rsdp
}

/// Recompute both RSDP checksums (bytes 8 and 32).
pub fn fix_rsdp(rsdp: &mut [u8]) {
    rsdp[8] = 0;
    let sum = rsdp[..20].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    rsdp[8] = 0u8.wrapping_sub(sum);
    if rsdp.len() >= 36 {
        rsdp[32] = 0;
        let sum = rsdp.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        rsdp[32] = 0u8.wrapping_sub(sum);
    }
}

pub fn rsdt(entries: &[u32]) -> Vec<u8> {
    let body: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
    sdt(b"RSDT", 1, &body)
}

pub fn xsdt(entries: &[u64]) -> Vec<u8> {
    let body: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
    sdt(b"XSDT", 1, &body)
}

/// Deterministic xorshift64* generator for the mutation loops.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}
