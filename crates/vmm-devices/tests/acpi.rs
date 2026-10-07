//! The ACPI tables read back by an independent parser (table headers, the
//! FADT/MADT/HPET/MCFG layouts of ACPI 6.3, AML and resource descriptors) and
//! checked against the device models: every port, address, IRQ and value the
//! tables give the guest is the one the `Machine` actually answers.

use std::collections::BTreeMap;

use vmm_devices::acpi::{self, build, Error, Layout, Platform};
use vmm_devices::acpi_pm::{self, AcpiPm, SLEEP_S5, STS_PWRBTN};
use vmm_devices::hpet;
use vmm_devices::i8042::{self, I8042};
use vmm_devices::ioapic;
use vmm_devices::lapic::{self, reg};
use vmm_devices::machine::{self, irq, slot, Machine, ECAM_BASE, ECAM_SIZE, PCI_ADDRESS, PCI_DATA};
use vmm_devices::rtc::{self, Rtc};
use vmm_devices::uart::{self, Uart};

/// A base in the BIOS area, where a legacy guest scans for the RSDP.
const BASE: u64 = 0x000E_0000;
const FILL: u8 = 0xA5;

fn tables_at(base: u64, cpus: u8) -> (Vec<u8>, Layout) {
    let mut b = vec![FILL; 0x10000];
    let l = build(&mut b, base, &Platform { cpus }).unwrap();
    assert!(
        b[l.len..].iter().all(|&x| x == FILL),
        "nothing is written past len"
    );
    b.truncate(l.len);
    (b, l)
}

fn tables() -> (Vec<u8>, Layout) {
    tables_at(BASE, 1)
}

fn rd(b: &[u8], at: usize, n: usize) -> u64 {
    b[at..at + n]
        .iter()
        .rev()
        .fold(0, |v, &x| (v << 8) | u64::from(x))
}

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |s, &x| s.wrapping_add(x))
}

/// The table at `gpa`: inside the buffer, its length from its header, summing to zero.
fn table<'a>(b: &'a [u8], l: &Layout, gpa: u64, sig: &[u8; 4]) -> &'a [u8] {
    let off = usize::try_from(gpa - l.rsdp).unwrap();
    assert_eq!(&b[off..off + 4], sig);
    let len = rd(b, off + 4, 4) as usize;
    let t = &b[off..off + len];
    assert_eq!(sum(t), 0, "checksum of {}", String::from_utf8_lossy(sig));
    t
}

/// Reads fields and remembers which bytes were looked at, so that the rest can be required to be zero.
struct Fields<'a> {
    t: &'a [u8],
    seen: Vec<bool>,
}

impl<'a> Fields<'a> {
    /// A table whose 36-byte header is checked elsewhere.
    fn new(t: &'a [u8]) -> Self {
        let mut seen = vec![false; t.len()];
        seen[..36].fill(true);
        Self { t, seen }
    }

    fn at(&mut self, off: usize, n: usize) -> u64 {
        self.seen[off..off + n].fill(true);
        rd(self.t, off, n)
    }

    fn rest_is_zero(&self, what: &str) {
        for (i, (&b, &s)) in self.t.iter().zip(&self.seen).enumerate() {
            if !s {
                assert_eq!(b, 0, "{what}: byte {i} is reserved or unused");
            }
        }
    }
}

fn machine() -> Machine {
    Machine::new(0, 100_000_000)
}

// ------------------------------------------------------------- AML parser

const ZERO: u8 = 0x00;
const ONE: u8 = 0x01;
const NAME: u8 = 0x08;
const BYTE: u8 = 0x0A;
const WORD: u8 = 0x0B;
const DWORD: u8 = 0x0C;
const STRING: u8 = 0x0D;
const QWORD: u8 = 0x0E;
const SCOPE: u8 = 0x10;
const BUFFER: u8 = 0x11;
const PACKAGE: u8 = 0x12;
const EXT: u8 = 0x5B;
const DEVICE: u8 = 0x82;

#[derive(Clone, Debug, PartialEq)]
enum Obj {
    Int(u64),
    Str(String),
    Buf(Vec<u8>),
    Pkg(Vec<Obj>),
}

#[derive(Default)]
struct Ns {
    objs: BTreeMap<String, Obj>,
    devices: Vec<String>,
    /// The longest package-length encoding met (1..=4 bytes).
    widest: usize,
}

impl Ns {
    fn get(&self, path: &str) -> &Obj {
        self.objs
            .get(path)
            .unwrap_or_else(|| panic!("{path} is not defined"))
    }

    fn int(&self, path: &str) -> u64 {
        match self.get(path) {
            Obj::Int(v) => *v,
            o => panic!("{path} is {o:?}"),
        }
    }

    fn buf(&self, path: &str) -> &[u8] {
        match self.get(path) {
            Obj::Buf(v) => v,
            o => panic!("{path} is {o:?}"),
        }
    }

    fn pkg(&self, path: &str) -> &[Obj] {
        match self.get(path) {
            Obj::Pkg(v) => v,
            o => panic!("{path} is {o:?}"),
        }
    }
}

struct Aml<'a> {
    b: &'a [u8],
    i: usize,
    widest: usize,
}

impl Aml<'_> {
    fn byte(&mut self) -> u8 {
        self.i += 1;
        self.b[self.i - 1]
    }

    fn le(&mut self, n: usize) -> u64 {
        self.i += n;
        rd(self.b, self.i - n, n)
    }

    /// A PkgLength; returns where the package ends. Only the shortest encoding is accepted.
    fn pkg_end(&mut self) -> usize {
        let start = self.i;
        let lead = self.byte();
        let extra = usize::from(lead >> 6);
        let mut len = usize::from(lead & if extra == 0 { 0x3F } else { 0x0F });
        if extra > 0 {
            assert_eq!(lead & 0x30, 0, "reserved bits of the PkgLength at {start}");
        }
        for k in 0..extra {
            len |= usize::from(self.byte()) << (4 + 8 * k);
        }
        let bytes = extra + 1;
        let shortest = (1..=4usize)
            .find(|&n| len - bytes + n < [0x40, 0x1000, 0x10_0000, 0x1000_0000][n - 1])
            .unwrap();
        assert_eq!(
            bytes, shortest,
            "the PkgLength at {start} ({len}) has a shorter encoding"
        );
        self.widest = self.widest.max(bytes);
        start + len
    }

    fn seg(&mut self) -> String {
        let s = &self.b[self.i..self.i + 4];
        self.i += 4;
        assert!(s[0].is_ascii_uppercase() || s[0] == b'_', "{s:?}");
        assert!(
            s[1..]
                .iter()
                .all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_'),
            "{s:?}"
        );
        String::from_utf8(s.to_vec()).unwrap()
    }

    /// A NameString resolved against `scope`.
    fn path(&mut self, scope: &str) -> String {
        let root = self.b[self.i] == b'\\';
        if root {
            self.i += 1;
        }
        assert_ne!(self.b[self.i], b'^', "no parent prefixes are used");
        let n = match self.b[self.i] {
            0x00 => 0,
            0x2E => 2,
            0x2F => usize::from(self.b[self.i + 1]),
            _ => 1,
        };
        self.i += match self.b[self.i] {
            0x00 | 0x2E => 1,
            0x2F => 2,
            _ => 0,
        };
        let mut p = if root {
            String::from("\\")
        } else {
            scope.to_string()
        };
        for _ in 0..n {
            if !p.ends_with('\\') {
                p.push('.');
            }
            let s = self.seg();
            p.push_str(&s);
        }
        p
    }

    /// A data object; integers must use their shortest encoding.
    fn object(&mut self) -> Obj {
        let at = self.i;
        match self.byte() {
            ZERO => Obj::Int(0),
            ONE => Obj::Int(1),
            BYTE => {
                let v = self.le(1);
                assert!(v > 1, "{v} at {at} has a shorter encoding");
                Obj::Int(v)
            }
            WORD => {
                let v = self.le(2);
                assert!(v > 0xFF, "{v:#x} at {at} has a shorter encoding");
                Obj::Int(v)
            }
            DWORD => {
                let v = self.le(4);
                assert!(v > 0xFFFF, "{v:#x} at {at} has a shorter encoding");
                Obj::Int(v)
            }
            QWORD => {
                let v = self.le(8);
                assert!(v > 0xFFFF_FFFF, "{v:#x} at {at} has a shorter encoding");
                Obj::Int(v)
            }
            STRING => {
                let n = self.b[self.i..].iter().position(|&c| c == 0).unwrap();
                let s = &self.b[self.i..self.i + n];
                assert!(s.iter().all(|c| (0x20..0x7F).contains(c)));
                self.i += n + 1;
                Obj::Str(String::from_utf8(s.to_vec()).unwrap())
            }
            BUFFER => {
                let end = self.pkg_end();
                let Obj::Int(n) = self.object() else {
                    panic!("buffer size at {at}")
                };
                let data = self.b[self.i..end].to_vec();
                assert_eq!(data.len() as u64, n, "buffer size at {at}");
                self.i = end;
                Obj::Buf(data)
            }
            PACKAGE => {
                let end = self.pkg_end();
                let n = self.byte();
                let mut v = Vec::new();
                while self.i < end {
                    v.push(self.object());
                }
                assert_eq!(self.i, end, "package at {at}");
                assert_eq!(v.len(), usize::from(n), "element count at {at}");
                Obj::Pkg(v)
            }
            op => panic!("unexpected data opcode {op:#x} at {at}"),
        }
    }

    fn terms(&mut self, end: usize, scope: &str, ns: &mut Ns) {
        while self.i < end {
            let at = self.i;
            match self.byte() {
                SCOPE => {
                    let e = self.pkg_end();
                    let p = self.path(scope);
                    self.terms(e, &p, ns);
                }
                EXT => {
                    assert_eq!(
                        self.byte(),
                        DEVICE,
                        "only Device follows ExtOpPrefix ({at})"
                    );
                    let e = self.pkg_end();
                    let p = self.path(scope);
                    assert!(!ns.devices.contains(&p), "{p} declared twice");
                    ns.devices.push(p.clone());
                    self.terms(e, &p, ns);
                }
                NAME => {
                    let p = self.path(scope);
                    let o = self.object();
                    assert!(ns.objs.insert(p.clone(), o).is_none(), "{p} defined twice");
                }
                op => panic!("unexpected opcode {op:#x} at {at}"),
            }
        }
        assert_eq!(self.i, end, "a term runs past its scope");
    }
}

fn namespace(b: &[u8], l: &Layout) -> Ns {
    let t = table(b, l, l.dsdt, b"DSDT");
    let mut a = Aml {
        b: t,
        i: 36,
        widest: 0,
    };
    let mut ns = Ns::default();
    a.terms(t.len(), "\\", &mut ns);
    ns.widest = a.widest;
    ns
}

/// A compressed EISA id, computed independently of the builder.
fn eisa(id: &str) -> u64 {
    let b = id.as_bytes();
    let l = |c: u8| u32::from(c - 0x40);
    let h = |c: u8| (c as char).to_digit(16).unwrap();
    let v = (l(b[0]) << 26)
        | (l(b[1]) << 21)
        | (l(b[2]) << 16)
        | (h(b[3]) << 12)
        | (h(b[4]) << 8)
        | (h(b[5]) << 4)
        | h(b[6]);
    u64::from(u32::from_le_bytes(v.to_be_bytes()))
}

// --------------------------------------------------- resource descriptors

#[derive(Debug, PartialEq)]
enum Res {
    Io {
        min: u64,
        max: u64,
        align: u64,
        len: u64,
    },
    Irq(u64),
    Fixed32 {
        writable: bool,
        base: u64,
        len: u64,
    },
    Window {
        kind: u8,
        flags: u8,
        type_flags: u8,
        min: u64,
        max: u64,
        len: u64,
        /// 2 for a Word descriptor, 4 for a DWord one.
        width: usize,
    },
}

const RES_MEMORY: u8 = 0;
const RES_IO: u8 = 1;
const RES_BUS: u8 = 2;
/// General flags: bit 2 minimum fixed, bit 3 maximum fixed; bit 0 clear: producer; bit 1 clear: positive decode.
const MIN_FIXED: u8 = 1 << 2;
const MAX_FIXED: u8 = 1 << 3;

/// The descriptors of a resource template, which must end with the end tag.
fn resources(b: &[u8]) -> Vec<Res> {
    let mut v = Vec::new();
    let mut i = 0;
    loop {
        let tag = b[i];
        if tag & 0x80 == 0 {
            let len = usize::from(tag & 7);
            let d = &b[i + 1..i + 1 + len];
            i += 1 + len;
            match (tag >> 3, len) {
                (0x08, 7) => {
                    assert_eq!(d[0], 1, "16-bit decode");
                    v.push(Res::Io {
                        min: rd(d, 1, 2),
                        max: rd(d, 3, 2),
                        align: rd(d, 5, 1),
                        len: rd(d, 6, 1),
                    });
                }
                (0x04, 2) => v.push(Res::Irq(rd(d, 0, 2))),
                (0x0F, 1) => {
                    assert_eq!(d[0], 0, "end tag: no checksum");
                    assert_eq!(i, b.len(), "the end tag is last");
                    return v;
                }
                _ => panic!("small descriptor {tag:#x}"),
            }
        } else {
            let len = rd(b, i + 1, 2) as usize;
            let d = &b[i + 3..i + 3 + len];
            i += 3 + len;
            let w = match (tag, len) {
                (0x86, 9) => {
                    assert_eq!(d[0] & !1, 0);
                    v.push(Res::Fixed32 {
                        writable: d[0] == 1,
                        base: rd(d, 1, 4),
                        len: rd(d, 5, 4),
                    });
                    continue;
                }
                (0x88, 13) => 2,
                (0x87, 23) => 4,
                _ => panic!("large descriptor {tag:#x}"),
            };
            assert_eq!(rd(d, 3, w), 0, "granularity");
            assert_eq!(rd(d, 3 + 3 * w, w), 0, "translation");
            v.push(Res::Window {
                kind: d[0],
                flags: d[1],
                type_flags: d[2],
                min: rd(d, 3 + w, w),
                max: rd(d, 3 + 2 * w, w),
                len: rd(d, 3 + 4 * w, w),
                width: w,
            });
        }
    }
}

// ------------------------------------------------- RSDP, XSDT, all tables

/// Everything that must hold for any build: pointers, checksums, alignment, no overlap, no stray bytes.
fn check_all(b: &[u8], l: &Layout, cpus: u8) {
    assert_eq!(l.len, b.len());
    // RSDP.
    let r = &b[..36];
    assert_eq!(&r[..8], b"RSD PTR ");
    assert_eq!(sum(&r[..20]), 0, "RSDP checksum");
    assert_eq!(sum(r), 0, "RSDP extended checksum");
    assert_eq!(&r[9..15], b"NANOX ");
    assert_eq!(r[15], 2, "revision: ACPI 2.0+");
    assert_eq!(rd(r, 16, 4), 0, "no RSDT");
    assert_eq!(rd(r, 20, 4), 36);
    assert_eq!(rd(r, 24, 8), l.xsdt);
    assert_eq!(&r[33..36], [0, 0, 0]);
    assert_eq!(l.rsdp % 16, 0);
    // XSDT: the four tables, each once.
    let x = table(b, l, l.xsdt, b"XSDT");
    assert_eq!(x.len(), 36 + 4 * 8);
    let entries: Vec<u64> = (0..4).map(|k| rd(x, 36 + 8 * k, 8)).collect();
    assert_eq!(entries, [l.fadt, l.madt, l.hpet, l.mcfg]);
    let mut spans = vec![(l.rsdp, 36)];
    for (gpa, sig, rev) in [
        (l.xsdt, b"XSDT", 1),
        (l.fadt, b"FACP", 6),
        (l.madt, b"APIC", 5),
        (l.hpet, b"HPET", 1),
        (l.mcfg, b"MCFG", 1),
        (l.dsdt, b"DSDT", 2),
    ] {
        let t = table(b, l, gpa, sig);
        assert_eq!(t[8], rev, "revision of {}", String::from_utf8_lossy(sig));
        assert_eq!(&t[10..16], b"NANOX ");
        assert_eq!(&t[16..24], b"NANOXVMM");
        assert_eq!(rd(t, 24, 4), 1, "OEM revision");
        assert_eq!(&t[28..32], b"NANX");
        assert_eq!(rd(t, 32, 4), 1, "creator revision");
        assert_eq!(gpa % 16, 0, "aligned");
        spans.push((gpa, t.len()));
    }
    // FACS: 64 bytes, 64-byte aligned, version 2, everything else zero.
    let f = &b[(l.facs - l.rsdp) as usize..][..64];
    assert_eq!(l.facs % 64, 0);
    assert_eq!(&f[..4], b"FACS");
    assert_eq!(rd(f, 4, 4), 64);
    assert_eq!(f[32], 2, "version");
    let mut fields = Fields {
        t: f,
        seen: vec![false; 64],
    };
    fields.at(0, 8);
    fields.at(32, 1);
    fields.rest_is_zero("FACS");
    spans.push((l.facs, 64));
    // No overlap; every byte outside the tables is padding.
    spans.sort();
    for w in spans.windows(2) {
        assert!(w[0].0 + w[0].1 as u64 <= w[1].0, "{w:?} overlap");
    }
    let mut used = vec![false; b.len()];
    for (gpa, len) in &spans {
        let off = (gpa - l.rsdp) as usize;
        used[off..off + len].fill(true);
    }
    for (i, &u) in used.iter().enumerate() {
        assert!(u || b[i] == 0, "padding byte {i} is {:#x}", b[i]);
    }
    assert!(used[b.len() - 1], "the buffer ends with a table");
    // One local APIC and one processor object per CPU, by the same UID.
    let madt = table(b, l, l.madt, b"APIC");
    let ns = namespace(b, l);
    for id in 0..cpus {
        let e = &madt[44 + 8 * usize::from(id)..][..8];
        assert_eq!(e, [0, 8, id, id, 1, 0, 0, 0], "local APIC {id}");
        let dev = format!("\\_SB_.C0{id:02X}");
        assert!(ns.devices.contains(&dev), "{dev}");
        assert_eq!(*ns.get(&format!("{dev}._HID")), Obj::Str("ACPI0007".into()));
        assert_eq!(ns.int(&format!("{dev}._UID")), u64::from(id));
    }
    assert_eq!(madt[44 + 8 * usize::from(cpus)], 1, "then the I/O APIC");
    assert_eq!(
        ns.devices
            .iter()
            .filter(|d| d.starts_with("\\_SB_.C0"))
            .count(),
        usize::from(cpus)
    );
}

#[test]
fn the_rsdp_leads_to_every_table_and_each_sums_to_zero() {
    let (b, l) = tables();
    assert_eq!(l.rsdp, BASE, "the RSDP is first");
    check_all(&b, &l, 1);
    assert_eq!(l.facs, BASE + 64);
    assert_eq!(l.dsdt, BASE + 128, "the DSDT follows the FACS");
}

#[test]
fn every_processor_count_gives_consistent_tables() {
    let (mut last, mut widest) = (0, 0);
    for cpus in 1..=255u8 {
        let (b, l) = tables_at(BASE, cpus);
        check_all(&b, &l, cpus);
        assert!(l.len > last, "more processors, longer tables");
        last = l.len;
        // \_SB_ grows past 4095 bytes: its package length needs a third byte.
        let ns = namespace(&b, &l);
        assert!(ns.widest >= widest, "{cpus} CPUs");
        widest = ns.widest;
        match cpus {
            1 => assert_eq!(widest, 2),
            255 => assert_eq!(widest, 3),
            _ => {}
        }
    }
}

#[test]
fn the_tables_move_with_the_base_address() {
    let (b1, l1) = tables_at(BASE, 2);
    for base in [0, 0x40, 0x7FF0_0000, 0xFFFF_0000 - 0x1000] {
        let (b2, l2) = tables_at(base, 2);
        check_all(&b2, &l2, 2);
        assert_eq!(l2.rsdp, base);
        for (a, c) in [
            (l1.facs, l2.facs),
            (l1.dsdt, l2.dsdt),
            (l1.fadt, l2.fadt),
            (l1.madt, l2.madt),
            (l1.hpet, l2.hpet),
            (l1.mcfg, l2.mcfg),
            (l1.xsdt, l2.xsdt),
        ] {
            assert_eq!(a - BASE, c - base);
        }
        assert_eq!(b1.len(), b2.len());
        // Only the address fields and checksums differ: the DSDT has none.
        let d = |b: &[u8], l: &Layout| table(b, l, l.dsdt, b"DSDT").to_vec();
        assert_eq!(d(&b1, &l1), d(&b2, &l2));
    }
}

// --------------------------------------------------------------------- FADT

/// FADT field offsets (ACPI 6.3, table 5-34) and the blocks, in FADT order:
/// (32-bit address, length byte, X_ generic address).
const PM1A_EVT: usize = 0;
const PM1A_CNT: usize = 2;
const PM_TMR: usize = 5;
const GPE0: usize = 6;
const BLOCKS: [(usize, usize, usize); 8] = [
    (56, 88, 148),
    (60, 88, 160),
    (64, 89, 172),
    (68, 89, 184),
    (72, 90, 196),
    (76, 91, 208),
    (80, 92, 220),
    (84, 93, 232),
];
/// The FADT's view of a block: (port, length in bytes).
fn block(fadt: &[u8], k: usize) -> (u16, u16) {
    (
        rd(fadt, BLOCKS[k].0, 4) as u16,
        u16::from(fadt[BLOCKS[k].1]),
    )
}

/// IAPC_BOOT_ARCH bits.
const LEGACY_DEVICES: u64 = 1 << 0;
const HAS_8042: u64 = 1 << 1;
const VGA_NOT_PRESENT: u64 = 1 << 2;
const MSI_NOT_SUPPORTED: u64 = 1 << 3;
/// FADT flags.
const WBINVD: u64 = 1 << 0;
const PROC_C1: u64 = 1 << 2;
const SLP_BUTTON: u64 = 1 << 5;
const FIX_RTC: u64 = 1 << 6;
const RESET_REG_SUP: u64 = 1 << 10;
/// GAS address spaces and access sizes.
const SYSTEM_MEMORY: u8 = 0;
const SYSTEM_IO: u8 = 1;
const ACCESS_BYTE: u8 = 1;
const ACCESS_WORD: u8 = 2;
const ACCESS_DWORD: u8 = 3;
/// PM1 control: SLP_TYP (bits 12:10) and SLP_EN (bit 13), SCI_EN (bit 0).
const SLP_TYP_SHIFT: u32 = 10;
const SLP_TYP: u32 = 7 << SLP_TYP_SHIFT;
const SLP_EN: u32 = 1 << 13;
const SCI_EN: u32 = 1;

#[test]
fn the_fadt_names_the_power_management_hardware_of_the_machine() {
    let (b, l) = tables();
    let t = table(&b, &l, l.fadt, b"FACP");
    assert_eq!(t.len(), 276, "ACPI 6 FADT");
    let mut f = Fields::new(t);
    assert_eq!(f.at(131, 1), 3, "minor version: 6.3");
    assert_eq!(f.at(36, 4), l.facs, "FIRMWARE_CTRL");
    assert_eq!(f.at(132, 8), l.facs, "X_FIRMWARE_CTRL");
    assert_eq!(f.at(40, 4), l.dsdt, "DSDT");
    assert_eq!(f.at(140, 8), l.dsdt, "X_DSDT");
    assert_eq!(f.at(46, 2), u64::from(irq::SCI), "SCI_INT");
    assert_eq!(f.at(48, 4), u64::from(acpi_pm::SMI_CMD));
    assert_eq!(f.at(52, 1), u64::from(acpi_pm::ACPI_ENABLE));
    assert_eq!(f.at(53, 1), u64::from(acpi_pm::ACPI_DISABLE));
    // The blocks: the 32-bit fields and the X_ generic addresses agree.
    let expected = [
        (
            PM1A_EVT,
            acpi_pm::PM1_STATUS,
            2 * (acpi_pm::PM1_ENABLE - acpi_pm::PM1_STATUS),
            ACCESS_WORD,
        ),
        (PM1A_CNT, acpi_pm::PM1_CONTROL, 2, ACCESS_WORD),
        (PM_TMR, acpi_pm::PM_TIMER, 4, ACCESS_DWORD),
        (
            GPE0,
            acpi_pm::GPE0_STATUS,
            acpi_pm::GPE0_END + 1 - acpi_pm::GPE0_STATUS,
            ACCESS_BYTE,
        ),
    ];
    for (k, &(legacy, len, x)) in BLOCKS.iter().enumerate() {
        let (port, n, access) = expected
            .iter()
            .find(|e| e.0 == k)
            .map_or((0, 0, 0), |e| (e.1, e.2, e.3));
        assert_eq!(f.at(legacy, 4), u64::from(port), "block {k}");
        // PM1b shares the length byte of PM1a.
        if ![1, 3].contains(&k) {
            assert_eq!(f.at(len, 1), u64::from(n), "block {k} length");
        }
        let space = if n == 0 { 0 } else { SYSTEM_IO };
        assert_eq!(
            [
                f.at(x, 1) as u8,
                f.at(x + 1, 1) as u8,
                f.at(x + 2, 1) as u8,
                f.at(x + 3, 1) as u8
            ],
            [space, (8 * n) as u8, 0, access],
            "X_ block {k}"
        );
        assert_eq!(f.at(x + 4, 8), u64::from(port), "X_ block {k} address");
    }
    assert_eq!(
        acpi_pm::GPE0_ENABLE - acpi_pm::GPE0_STATUS,
        block(t, GPE0).1 / 2,
        "GPE0: status half, enable half"
    );
    assert!(f.at(96, 2) > 100, "no C2");
    assert!(f.at(98, 2) > 1000, "no C3");
    assert_eq!(f.at(108, 1), u64::from(rtc::REG_CENTURY));
    assert_eq!(
        f.at(109, 2),
        LEGACY_DEVICES | HAS_8042 | VGA_NOT_PRESENT | MSI_NOT_SUPPORTED
    );
    // Not hardware-reduced, fixed power button (bit 4 clear), 24-bit timer (bit 8 clear).
    assert_eq!(
        f.at(112, 4),
        WBINVD | PROC_C1 | SLP_BUTTON | FIX_RTC | RESET_REG_SUP
    );
    f.at(116, 13); // the reset register: its own test
    f.rest_is_zero("FADT");
}

#[test]
fn the_fadt_blocks_are_the_ports_the_pm_device_answers() {
    let (b, l) = tables();
    let t = table(&b, &l, l.fadt, b"FACP");
    for (k, size) in [(PM1A_EVT, 2), (PM1A_CNT, 2), (PM_TMR, 4), (GPE0, 1)] {
        let (port, len) = block(t, k);
        let mut pm = AcpiPm::new();
        for p in port..port + len {
            assert!(AcpiPm::owns(p), "{p:#x} of block {k}");
        }
        // An access of the declared size at every register of the block is answered.
        assert_eq!(
            usize::from(t[BLOCKS[k].2 + 3]),
            [0, 1, 2, 4].iter().position(|&s| s == size).unwrap()
        );
        for p in (port..port + len).step_by(size) {
            assert!(pm.read(p, size as u8, 0).is_some(), "{p:#x}");
        }
        assert!(
            pm.read(port, 2 * size as u8, 0).is_none() || k == GPE0,
            "a wider access straddles registers"
        );
    }
    for k in [PM1A_CNT, PM_TMR, GPE0] {
        let (port, len) = block(t, k);
        assert!(
            !AcpiPm::owns(port + len),
            "block {k} ends with its register"
        );
    }

    // ACPI mode through the SMI command, seen in PM1a control.
    let mut m = machine();
    let (cnt, _) = block(t, PM1A_CNT);
    let smi = rd(t, 48, 4) as u16;
    assert_eq!(m.io_in(cnt, 2, 0) & SCI_EN, 0);
    m.io_out(smi, 1, u32::from(t[52]), 0);
    assert_eq!(m.io_in(cnt, 2, 0) & SCI_EN, SCI_EN);
    m.io_out(smi, 1, u32::from(t[53]), 0);
    assert_eq!(m.io_in(cnt, 2, 0) & SCI_EN, 0);

    // The timer is 24 bits wide (TMR_VAL_EXT clear).
    let (tmr, _) = block(t, PM_TMR);
    let v = m.io_in(tmr, 4, 10_000_000_000);
    assert!(v > 0 && v < 1 << 24, "{v:#x}");

    // GPE0: the enable half follows the status half.
    let (gpe, len) = block(t, GPE0);
    m.io_out(smi, 1, u32::from(t[52]), 0);
    m.io_out(gpe + len / 2, 1, 1, 0);
    m.pm.raise_gpe(0);
    assert!(m.pm.sci(0));
    m.io_out(gpe, 1, 1, 0);
    assert!(!m.pm.sci(0), "status cleared by writing 1");
}

#[test]
fn the_fadt_reset_register_resets_the_machine() {
    let (b, l) = tables();
    let t = table(&b, &l, l.fadt, b"FACP");
    let gas = &t[116..128];
    assert_eq!(
        gas[..4],
        [SYSTEM_IO, 8, 0, ACCESS_BYTE],
        "an 8-bit I/O register"
    );
    let port = rd(gas, 4, 8) as u16;
    let value = t[128];
    assert_eq!((port, value), (acpi::RESET_PORT, acpi::RESET_VALUE));
    let mut m = machine();
    assert!(!m.take_reset());
    m.io_out(port, 1, u32::from(value), 0);
    assert!(m.take_reset(), "the write ACPI's reset makes resets");
    // Why not the q35 reset control: port 0xCF9 is the PCI data window here.
    m.io_out(0xCF9, 1, 0x06, 0);
    assert!(!m.take_reset());
}

#[test]
fn the_s5_package_powers_the_machine_off() {
    let (b, l) = tables();
    let t = table(&b, &l, l.fadt, b"FACP");
    let ns = namespace(&b, &l);
    let typ = |name: &str| -> Vec<u64> {
        ns.pkg(name)
            .iter()
            .map(|o| match o {
                Obj::Int(v) => *v,
                o => panic!("{o:?}"),
            })
            .collect()
    };
    let s5 = typ("\\_S5_");
    assert_eq!(s5.len(), 4);
    assert_eq!(s5[1], s5[0], "SLP_TYPb as SLP_TYPa (there is no PM1b)");
    assert_eq!(s5[2..], [0, 0]);
    assert_eq!(typ("\\_S0_"), [0, 0, 0, 0]);

    // What ACPICA does to enter S5: SLP_TYP first, then SLP_TYP with SLP_EN.
    let mut m = machine();
    let (cnt, _) = block(t, PM1A_CNT);
    m.io_out(rd(t, 48, 4) as u16, 1, u32::from(t[52]), 0);
    let v = m.io_in(cnt, 2, 0) & !(SLP_TYP | SLP_EN);
    let slp = (s5[0] as u32) << SLP_TYP_SHIFT;
    m.io_out(cnt, 2, v | slp, 0);
    assert_eq!(m.take_sleep(), None, "SLP_TYP alone does nothing");
    m.io_out(cnt, 2, v | slp | SLP_EN, 0);
    assert_eq!(m.take_sleep(), Some(SLEEP_S5));
    // Leaving a sleep state writes the S0 type without SLP_EN: no request.
    m.io_out(cnt, 2, v, 0);
    assert_eq!(m.take_sleep(), None);
}

// --------------------------------------------------------------------- MADT

const POLARITY_LOW: u64 = 3;
const TRIGGER_LEVEL: u64 = 3 << 2;

/// The interrupt source overrides: (ISA IRQ, GSI, flags).
fn overrides(madt: &[u8]) -> Vec<(u8, u8, u64)> {
    let mut v = Vec::new();
    let mut i = 44;
    while i < madt.len() {
        if madt[i] == 2 {
            assert_eq!(madt[i + 2], 0, "ISA bus");
            v.push((madt[i + 3], rd(madt, i + 4, 4) as u8, rd(madt, i + 8, 2)));
        }
        i += usize::from(madt[i + 1]);
    }
    assert_eq!(i, madt.len());
    v
}

#[test]
fn the_madt_describes_the_interrupt_controllers_of_the_machine() {
    let (b, l) = tables();
    let t = table(&b, &l, l.madt, b"APIC");
    assert_eq!(rd(t, 36, 4), lapic::DEFAULT_BASE);
    assert_eq!(rd(t, 40, 4), 1, "PCAT_COMPAT: the 8259 pair is there");
    let mut e = vec![0, 8, 0, 0, 1, 0, 0, 0];
    e.extend([1, 12, 0, 0]);
    e.extend((ioapic::DEFAULT_BASE as u32).to_le_bytes());
    e.extend(0u32.to_le_bytes());
    e.extend([2, 10, 0, irq::TIMER]);
    e.extend(u32::from(machine::ioapic_pin(irq::TIMER)).to_le_bytes());
    e.extend([0, 0]);
    e.extend([2, 10, 0, irq::SCI]);
    e.extend(u32::from(machine::ioapic_pin(irq::SCI)).to_le_bytes());
    e.extend(((POLARITY_LOW | TRIGGER_LEVEL) as u16).to_le_bytes());
    e.extend([4, 6, 0xFF, 0, 0, 1]); // NMI on LINT1 of every processor
    assert_eq!(t[44..], e[..]);

    // The ids are the ones the devices report.
    let mut m = machine();
    m.mmio_write(ioapic::DEFAULT_BASE, 4, u64::from(ioapic::REG_ID), 0);
    let io_id = m.mmio_read(ioapic::DEFAULT_BASE + ioapic::OFF_WINDOW, 4, 0) >> 24 & 0xF;
    assert_eq!(io_id, u64::from(t[44 + 8 + 2]), "I/O APIC id");
    let apic_id = m.mmio_read(lapic::DEFAULT_BASE + u64::from(reg::ID), 4, 0) >> 24;
    assert_eq!(apic_id, u64::from(t[44 + 3]), "local APIC id");

    // An ISA IRQ without an override is the I/O APIC pin of the same number.
    let ov = overrides(t);
    for n in 0..16u8 {
        match ov.iter().find(|o| o.0 == n) {
            Some(&(_, gsi, _)) => assert_eq!(gsi, machine::ioapic_pin(n)),
            None => assert_eq!(machine::ioapic_pin(n), n, "IRQ {n}"),
        }
    }
    assert_eq!(ov[0], (irq::TIMER, 2, 0), "IRQ0 is pin 2, as the bus says");
}

/// Programs an I/O APIC redirection entry (destination 0).
fn route(m: &mut Machine, pin: u8, low: u32) {
    for (r, v) in [(0x10 + 2 * pin + 1, 0), (0x10 + 2 * pin, low)] {
        m.mmio_write(ioapic::DEFAULT_BASE, 4, u64::from(r), 0);
        m.mmio_write(
            ioapic::DEFAULT_BASE + ioapic::OFF_WINDOW,
            4,
            u64::from(v),
            0,
        );
    }
}

/// The local APIC on, LINT0 masked (the 8259 out of the way).
fn enable_lapic(m: &mut Machine) {
    m.mmio_write(lapic::DEFAULT_BASE + u64::from(reg::SVR), 4, 0x1FF, 0);
    m.mmio_write(
        lapic::DEFAULT_BASE + u64::from(reg::LVT_LINT0),
        4,
        1 << 16,
        0,
    );
}

#[test]
fn an_io_apic_set_up_from_the_sci_override_delivers_the_power_button() {
    let (b, l) = tables();
    let madt = table(&b, &l, l.madt, b"APIC");
    let fadt = table(&b, &l, l.fadt, b"FACP");
    let sci = rd(fadt, 46, 2) as u8;
    let &(_, gsi, flags) = overrides(madt)
        .iter()
        .find(|o| o.0 == sci)
        .expect("the SCI has an override");
    assert_eq!(flags, POLARITY_LOW | TRIGGER_LEVEL);
    let entry = |polarity_low: bool| {
        let mut v = 0x41u32;
        if polarity_low {
            v |= 1 << 13;
        }
        if flags & TRIGGER_LEVEL == TRIGGER_LEVEL {
            v |= 1 << 15;
        }
        v
    };
    let (evt, len) = block(fadt, PM1A_EVT);
    let smi = rd(fadt, 48, 4) as u16;

    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, gsi, entry(flags & 3 == POLARITY_LOW));
    m.io_out(smi, 1, u32::from(fadt[52]), 0);
    m.io_out(evt + len / 2, 2, u32::from(STS_PWRBTN), 0);
    assert_eq!(m.pending(0), None, "idle");
    m.press_power_button();
    assert_eq!(m.acknowledge(0), Some(0x41));
    m.io_out(evt, 2, u32::from(STS_PWRBTN), 0);
    m.mmio_write(lapic::DEFAULT_BASE + u64::from(reg::EOI), 4, 0, 0);
    assert_eq!(m.pending(0), None, "cleared by the driver");

    // The polarity matters: taken as active high, the idle line is an interrupt.
    let mut m = machine();
    enable_lapic(&mut m);
    route(&mut m, gsi, entry(false));
    assert_eq!(m.pending(0), Some(0x41));
}

// ---------------------------------------------------------------- HPET, MCFG

#[test]
fn the_hpet_table_is_the_device_at_its_address() {
    let (b, l) = tables();
    let t = table(&b, &l, l.hpet, b"HPET");
    assert_eq!(t.len(), 56);
    let mut f = Fields::new(t);
    let mut m = machine();
    let id = m.mmio_read(hpet::DEFAULT_BASE + hpet::REG_ID, 4, 0);
    assert_eq!(
        f.at(36, 4),
        id,
        "event timer block id: the capabilities register"
    );
    // The register is the same with the counter running and later in time.
    m.mmio_write(hpet::DEFAULT_BASE + hpet::REG_CONFIG, 8, 1, 1_000);
    assert_eq!(
        m.mmio_read(hpet::DEFAULT_BASE + hpet::REG_ID, 4, 5_000_000),
        id
    );
    assert_eq!(id, hpet::CAPABILITIES & 0xFFFF_FFFF);
    assert_eq!(f.at(40, 1), u64::from(SYSTEM_MEMORY));
    assert_eq!(f.at(44, 8), hpet::DEFAULT_BASE);
    f.rest_is_zero("HPET");
}

#[test]
fn the_mcfg_is_the_ecam_window_of_the_machine() {
    let (b, l) = tables();
    let t = table(&b, &l, l.mcfg, b"MCFG");
    assert_eq!(t.len(), 60);
    let mut f = Fields::new(t);
    let base = f.at(44, 8);
    assert_eq!(base, ECAM_BASE);
    assert_eq!(f.at(52, 2), 0, "segment");
    assert_eq!(f.at(54, 1), 0, "first bus");
    let last = f.at(55, 1);
    assert_eq!((last + 1) << 20, ECAM_SIZE, "1 MiB per bus");
    f.rest_is_zero("MCFG");
    // The host bridge through the window, and the last bus inside it.
    let mut m = machine();
    assert_eq!(m.mmio_read(base, 2, 0), 0x8086);
    assert_eq!(m.mmio_read(base + (last << 20), 2, 0), 0xFFFF);
    assert_eq!(m.unclaimed_mmio, 0);
}

// --------------------------------------------------------------------- DSDT

#[test]
fn the_pci_root_bridge_and_its_windows() {
    let (b, l) = tables();
    let ns = namespace(&b, &l);
    let mcfg = table(&b, &l, l.mcfg, b"MCFG");
    assert!(ns.devices.contains(&"\\_SB_.PCI0".to_string()));
    assert_eq!(eisa("PNP0A08"), 0x080A_D041, "the well-known value");
    assert_eq!(ns.int("\\_SB_.PCI0._HID"), eisa("PNP0A08"));
    assert_eq!(ns.int("\\_SB_.PCI0._CID"), eisa("PNP0A03"));
    for n in ["_SEG", "_BBN", "_UID"] {
        assert_eq!(ns.int(&format!("\\_SB_.PCI0.{n}")), 0, "{n}");
    }
    let last_bus = u64::from(mcfg[55]);
    let config_ports = u64::from(PCI_DATA + 4 - PCI_ADDRESS);
    let window = |kind, type_flags, min: u64, max: u64, width| Res::Window {
        kind,
        flags: MIN_FIXED | MAX_FIXED,
        type_flags,
        min,
        max,
        len: max - min + 1,
        width,
    };
    let mem_min = ECAM_BASE + ECAM_SIZE;
    let mem_max = ioapic::DEFAULT_BASE - 1;
    assert_eq!(
        resources(ns.buf("\\_SB_.PCI0._CRS")),
        [
            window(RES_BUS, 0, 0, last_bus, 2),
            Res::Io {
                min: PCI_ADDRESS.into(),
                max: PCI_ADDRESS.into(),
                align: 1,
                len: config_ports
            },
            window(RES_IO, 3, 0, u64::from(PCI_ADDRESS) - 1, 2),
            window(RES_IO, 3, u64::from(PCI_ADDRESS) + config_ports, 0xFFFF, 2),
            // Read-write, not cacheable: from the end of ECAM to the I/O APIC.
            window(RES_MEMORY, 1, mem_min, mem_max, 4),
        ]
    );
    assert_eq!(
        (acpi::PCI_MMIO_BASE, acpi::PCI_MMIO_END),
        (mem_min, mem_max + 1)
    );
    for (what, base, size) in [
        ("ECAM", ECAM_BASE, ECAM_SIZE),
        ("I/O APIC", ioapic::DEFAULT_BASE, 0x1000),
        ("HPET", hpet::DEFAULT_BASE, hpet::SIZE),
        ("local APIC", lapic::DEFAULT_BASE, 0x1000),
    ] {
        assert!(
            base + size <= mem_min || base > mem_max,
            "the BAR window overlaps the {what}"
        );
    }
    // Every port of the platform's devices is behind the bridge's I/O windows.
    for p in (0..=0xFFFFu16).filter(|&p| {
        Uart::owns(p) || I8042::owns(p) || Rtc::owns(p) || AcpiPm::owns(p) || p == 0x92
    }) {
        assert!(u64::from(p) < u64::from(PCI_ADDRESS), "{p:#x}");
    }
}

#[test]
fn the_prt_routes_every_slot_and_pin_as_the_hardware_does() {
    let (b, l) = tables();
    let ns = namespace(&b, &l);
    let prt = ns.pkg("\\_SB_.PCI0._PRT");
    assert_eq!(prt.len(), 32 * 4);
    let mut seen = BTreeMap::new();
    for e in prt {
        let Obj::Pkg(e) = e else { panic!("{e:?}") };
        let [Obj::Int(addr), Obj::Int(pin), Obj::Int(0), Obj::Int(gsi)] = e.as_slice() else {
            panic!("{e:?}: address, pin, source 0 (a GSI), GSI")
        };
        let (addr, pin, gsi) = (*addr, *pin, *gsi);
        assert_eq!(addr & 0xFFFF, 0xFFFF, "every function of the device");
        let dev = (addr >> 16) as u8;
        assert!(dev < 32 && pin < 4, "{e:?}");
        // The swizzle: INTx of slot s is wired like INTA of slot s + x.
        assert_eq!(gsi, 16 + (u64::from(dev) + pin) % 4, "{e:?}");
        assert!(gsi < ioapic::PINS as u64);
        assert!(seen.insert((dev, pin), gsi).is_none(), "{e:?} twice");
    }
    assert_eq!(seen.len(), 128);
    for dev in 0..32u8 {
        assert_eq!(
            seen[&(dev, 0)],
            u64::from(machine::pci_pin(dev)),
            "INTA of {dev}"
        );
    }
    for dev in [slot::BLK, slot::NET, slot::GPU, slot::CONSOLE] {
        assert_eq!(seen[&(dev, 0)], u64::from(machine::pci_pin(dev)));
    }
    // One entry, byte for byte: DWord address, Zero, Zero, Byte GSI.
    let s = slot::BLK;
    let entry = [
        PACKAGE,
        0x0B,
        4,
        DWORD,
        0xFF,
        0xFF,
        s,
        0,
        ZERO,
        ZERO,
        BYTE,
        machine::pci_pin(s),
    ];
    assert!(b.windows(entry.len()).any(|w| w == entry));
}

/// An I/O descriptor covers exactly the ports a device decodes.
fn claims(r: &Res, owns: fn(u16) -> bool) {
    let Res::Io { min, max, len, .. } = *r else {
        panic!("{r:?}")
    };
    assert_eq!(min, max, "fixed");
    let (min, len) = (min as u16, len as u16);
    for p in min..min + len {
        assert!(owns(p), "{p:#x}");
    }
    assert!(!owns(min - 1) && !owns(min + len), "{r:?}");
}

#[test]
fn the_legacy_devices_claim_what_the_models_decode() {
    let (b, l) = tables();
    let ns = namespace(&b, &l);
    let dev = |n: &str, hid: &str| {
        let p = format!("\\_SB_.{n}");
        assert!(ns.devices.contains(&p), "{p}");
        assert_eq!(ns.int(&format!("{p}._HID")), eisa(hid), "{p}");
        resources(ns.buf(&format!("{p}._CRS")))
    };
    let com1 = dev("PCI0.COM1", "PNP0501");
    claims(&com1[0], Uart::owns);
    assert_eq!(
        com1[0],
        Res::Io {
            min: uart::BASE.into(),
            max: uart::BASE.into(),
            align: 1,
            len: 8
        }
    );
    assert_eq!(com1[1..], [Res::Irq(1 << irq::COM1)]);

    let kbd = dev("PCI0.KBD_", "PNP0303");
    assert_eq!(kbd.len(), 3);
    claims(&kbd[0], I8042::owns);
    claims(&kbd[1], I8042::owns);
    // Data port first, then command/status, as Linux's i8042 PnP probe takes them.
    assert!(matches!(kbd[0], Res::Io { min, .. } if min == u64::from(i8042::PORT_DATA)));
    assert!(matches!(kbd[1], Res::Io { min, .. } if min == u64::from(i8042::PORT_STATUS)));
    assert_eq!(kbd[2], Res::Irq(1 << irq::KEYBOARD));
    assert_eq!(dev("PCI0.PS2M", "PNP0F13"), [Res::Irq(1 << irq::MOUSE)]);

    let rtc = dev("PCI0.RTC_", "PNP0B00");
    claims(&rtc[0], Rtc::owns);
    assert!(matches!(rtc[0], Res::Io { min, .. } if min == u64::from(rtc::PORT_INDEX)));
    assert_eq!(rtc[1..], [Res::Irq(1 << irq::RTC)]);

    assert_eq!(
        dev("HPET", "PNP0103"),
        [Res::Fixed32 {
            writable: true,
            base: hpet::DEFAULT_BASE,
            len: hpet::SIZE
        }]
    );
    // The motherboard resource Linux looks for before it uses MMCONFIG.
    assert_eq!(
        dev("MBRD", "PNP0C02"),
        [Res::Fixed32 {
            writable: true,
            base: ECAM_BASE,
            len: ECAM_SIZE
        }]
    );
    let mut devices = ns.devices.clone();
    devices.sort();
    assert_eq!(
        devices,
        [
            "\\_SB_.C000",
            "\\_SB_.HPET",
            "\\_SB_.MBRD",
            "\\_SB_.PCI0",
            "\\_SB_.PCI0.COM1",
            "\\_SB_.PCI0.KBD_",
            "\\_SB_.PCI0.PS2M",
            "\\_SB_.PCI0.RTC_"
        ]
    );
    // PCI0: _HID _CID _SEG _BBN _UID _CRS _PRT; _HID and _CRS of six devices; the processor's
    // _HID and _UID; _S0_ and _S5_.
    assert_eq!(ns.objs.len(), 7 + 2 * 6 + 2 + 2, "no other names");
}

#[test]
fn aml_encodings_byte_for_byte() {
    let (b, l) = tables();
    let d = table(&b, &l, l.dsdt, b"DSDT");
    let has = |x: &[u8]| d.windows(x.len()).any(|w| w == x);
    // Name (_HID, EisaId ("PNP0A08")): a DWord.
    assert!(has(&[
        NAME, b'_', b'H', b'I', b'D', DWORD, 0x41, 0xD0, 0x0A, 0x08
    ]));
    // Name (_HID, "ACPI0007").
    assert!(has(b"\x08_HID\x0DACPI0007\x00"));
    // Name (_UID, Zero) of the processor; Name (_SEG, Zero).
    assert!(has(&[NAME, b'_', b'S', b'E', b'G', ZERO]));
    // Name (_S5_, Package (4) { 5, 5, Zero, Zero }): SLP_TYP 5 takes a BytePrefix.
    assert!(has(&[
        NAME, b'_', b'S', b'5', b'_', PACKAGE, 8, 4, BYTE, SLEEP_S5, BYTE, SLEEP_S5, ZERO, ZERO
    ]));
    // Scope (\_SB_) right after the header, its length in two bytes for one CPU.
    assert_eq!(d[36], SCOPE);
    assert_eq!(d[37] >> 6, 1);
    assert_eq!(&d[39..44], b"\\_SB_");
    // The RTC's resource template: Buffer (13) { IO (Decode16, 0x70, 0x70, 1, 2), IRQNoFlags {8}, end tag }.
    let p = rtc::PORT_INDEX as u8;
    let rtc = [
        &[BUFFER, 16, BYTE, 13][..],
        &[0x47, 1, p, 0, p, 0, 1, 2],
        &[0x22, 0, 1],
        &[0x79, 0],
    ]
    .concat();
    assert!(has(&rtc), "{rtc:x?}");
}

// ------------------------------------------------- AML encoder boundaries

/// A PkgLength decoded (ACPI 6, 20.2.4), independently of the encoder: its value and the bytes it took.
fn decode_pkg_length(b: &[u8]) -> (usize, usize) {
    let extra = usize::from(b[0] >> 6);
    if extra == 0 {
        return (usize::from(b[0] & 0x3F), 1);
    }
    assert_eq!(b[0] & 0x30, 0, "reserved bits of the lead byte");
    let mut v = usize::from(b[0] & 0x0F);
    for k in 0..extra {
        v |= usize::from(b[1 + k]) << (4 + 8 * k);
    }
    (v, 1 + extra)
}

#[test]
fn aml_integers_switch_encoding_exactly_at_the_byte_word_and_dword_limits() {
    for (v, bytes) in [
        (0u32, &[0x00][..]),
        (1, &[0x01]),
        (2, &[0x0A, 2]),
        (0x7F, &[0x0A, 0x7F]),
        (0xFF, &[0x0A, 0xFF]),
        (0x100, &[0x0B, 0x00, 0x01]),
        (0x1234, &[0x0B, 0x34, 0x12]),
        (0xFFFF, &[0x0B, 0xFF, 0xFF]),
        (0x1_0000, &[0x0C, 0, 0, 1, 0]),
        (0xDEAD_BEEF, &[0x0C, 0xEF, 0xBE, 0xAD, 0xDE]),
        (u32::MAX, &[0x0C, 0xFF, 0xFF, 0xFF, 0xFF]),
    ] {
        let (b, n) = acpi::aml_integer(v);
        assert_eq!(&b[..n], bytes, "{v:#x}");
        assert!(b[n..].iter().all(|&x| x == 0), "{v:#x}: unused bytes");
    }
    // The parser (which insists on the shortest form) reads back every value around the limits.
    for v in (0..0x400).chain(0xFF00..0x1_0100) {
        let (b, n) = acpi::aml_integer(v);
        let mut a = Aml {
            b: &b[..n],
            i: 0,
            widest: 0,
        };
        assert_eq!(a.object(), Obj::Int(v.into()));
        assert_eq!(a.i, n);
    }
}

#[test]
fn aml_package_lengths_are_the_shortest_that_hold_their_own_size() {
    // The largest body each width holds: the length counts its own bytes and has 6, 12, 20 and 28 bits.
    let limit = [0x3E, 0xFFD, 0xF_FFFC, 0x0FFF_FFFB];
    let width = |body: usize| 1 + limit.iter().take(3).filter(|&&m| body > m).count();
    let mut bodies: Vec<usize> = (0..0x2000).collect();
    for m in limit {
        bodies.extend([m - 1, m, m + 1]);
    }
    for body in bodies.into_iter().filter(|&b| b <= limit[3]) {
        let (bytes, n) = acpi::aml_package_length(body);
        assert_eq!(n, width(body), "body {body:#x}");
        assert_eq!(decode_pkg_length(&bytes), (body + n, n), "body {body:#x}");
        assert!(
            bytes[n..].iter().all(|&x| x == 0),
            "{body:#x}: unused bytes"
        );
    }
    // Exact bytes at the first and last body of each width.
    for (body, bytes) in [
        (0, &[1][..]),
        (0x3E, &[0x3F]),
        (0x3F, &[0x41, 0x04]),
        (0xFFD, &[0x4F, 0xFF]),
        (0xFFE, &[0x81, 0x00, 0x01]),
        (0xF_FFFC, &[0x8F, 0xFF, 0xFF]),
        (0xF_FFFD, &[0xC1, 0x00, 0x00, 0x01]),
        (0x0FFF_FFFB, &[0xCF, 0xFF, 0xFF, 0xFF]),
    ] {
        let (b, n) = acpi::aml_package_length(body);
        assert_eq!(&b[..n], bytes, "body {body:#x}");
    }
    // Too large for four bytes: the length is cut to 28 bits, in four bytes, without panic.
    for body in [0x0FFF_FFFC, 0x1000_0000, 0x1234_5678] {
        let (bytes, n) = acpi::aml_package_length(body);
        assert_eq!(n, 4, "{body:#x}");
        assert_eq!(
            decode_pkg_length(&bytes),
            ((body + 4) & 0x0FFF_FFFF, 4),
            "{body:#x}"
        );
    }
}

#[test]
fn the_eisa_ids_are_the_compressed_text_of_the_device_names() {
    // Known values (iasl prints EisaId ("PNP0A08") for 0x080AD041), then every id the DSDT uses.
    assert_eq!(eisa("PNP0A08"), 0x080A_D041);
    assert_eq!(eisa("PNP0303"), 0x0303_D041);
    assert_eq!(eisa("PNP0F13"), 0x130F_D041);
    let (b, l) = tables();
    let ns = namespace(&b, &l);
    for (path, id) in [
        ("PCI0._HID", "PNP0A08"),
        ("PCI0._CID", "PNP0A03"),
        ("PCI0.COM1._HID", "PNP0501"),
        ("PCI0.KBD_._HID", "PNP0303"),
        ("PCI0.PS2M._HID", "PNP0F13"),
        ("PCI0.RTC_._HID", "PNP0B00"),
        ("HPET._HID", "PNP0103"),
        ("MBRD._HID", "PNP0C02"),
    ] {
        assert_eq!(ns.int(&format!("\\_SB_.{path}")), eisa(id), "{path}");
    }
}

// ------------------------------------------------------------------ errors

#[test]
fn bad_arguments_are_errors_and_leave_the_buffer_alone() {
    let (_, l) = tables();
    let one = Platform { cpus: 1 };
    assert_eq!(Platform::default(), one);
    for n in [0, 1, 35, 64, l.len / 2, l.len - 1] {
        let mut b = vec![FILL; n];
        assert_eq!(
            build(&mut b, BASE, &one),
            Err(Error::BufferTooSmall { needed: l.len })
        );
        assert!(b.iter().all(|&x| x == FILL), "{n}: untouched");
    }
    let mut b = vec![FILL; l.len];
    assert_eq!(build(&mut b, BASE, &one), Ok(l), "an exact fit");
    let mut b = vec![FILL; 0x10000];
    for base in [BASE + 1, BASE + 16, BASE + 32, BASE + 63] {
        assert_eq!(build(&mut b, base, &one), Err(Error::Misaligned));
    }
    assert_eq!(
        build(&mut b, BASE, &Platform { cpus: 0 }),
        Err(Error::NoCpus)
    );
    for base in [0xFFFF_FFC0, 1 << 32, u64::MAX & !63] {
        assert_eq!(build(&mut b, base, &one), Err(Error::AboveFourGiB));
    }
    assert!(b.iter().all(|&x| x == FILL));
    // The highest base that keeps every byte below 4 GiB, and the next one.
    let top = ((1u64 << 32) - l.len as u64) & !63;
    assert!(build(&mut b, top, &one).is_ok());
    assert_eq!(build(&mut b, top + 64, &one), Err(Error::AboveFourGiB));
}
