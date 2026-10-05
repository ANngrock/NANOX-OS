//! ACPI tables that describe the platform of [`crate::machine`] to a guest
//! OS, written without allocation into a buffer the VMM places at a guest
//! physical address of its choice ([`build`]). The tables reference each
//! other by that address. The set is what a Linux guest of a q35 machine
//! reads, cut down to what this platform has (ACPI 6.3):
//!
//! * RSDP, revision 2: the XSDT address and both checksums (no RSDT);
//! * XSDT: FADT, MADT, HPET, MCFG;
//! * FADT, revision 6.3, not hardware-reduced, pointing at the FACS and the
//!   DSDT: SCI on IRQ 9, the SMI command port with the ACPI enable/disable
//!   values, the PM1a event and control blocks, the 24-bit PM timer and GPE0
//!   at the ports [`crate::acpi_pm`] decodes (the 32-bit fields and the X_
//!   generic addresses agree); the power button is the fixed one, there is no
//!   sleep button and no RTC wake status; the reset register is port 0x92
//!   with value 1, the fast reset of [`crate::legacy`] (the q35 reset control
//!   at 0xCF9 is not modeled); boot flags: legacy devices, an 8042, no VGA,
//!   no MSI;
//! * MADT, revision 5: the local APIC page, the 8259 pair (PC-AT), one local
//!   APIC per processor, the I/O APIC (GSI base 0), the overrides IRQ0 → GSI 2
//!   and IRQ 9 (the SCI) level, active low, and LINT1 as NMI;
//! * HPET, revision 1: the event timer block id is the device's capabilities
//!   register;
//! * MCFG, revision 1: the ECAM window, segment 0, every bus it covers;
//! * DSDT, revision 2, in hand-assembled AML: `\_SB.PCI0` (PNP0A08, PNP0A03)
//!   with its bus range, the configuration ports, the I/O windows around them
//!   and one memory window for BARs, [`PCI_MMIO_BASE`]..[`PCI_MMIO_END`]
//!   (from the end of ECAM to the I/O APIC); `_PRT` for APIC mode; COM1, the
//!   keyboard controller and the RTC under it; `\_SB.HPET`; a motherboard
//!   resource (PNP0C02) reserving ECAM, which Linux checks before it uses
//!   MMCONFIG; a processor object (ACPI0007) per CPU; `\_S0_` and `\_S5_`
//!   with the SLP_TYP `acpi_pm` takes for soft-off.
//!
//! Not described: interrupt routing in PIC mode (there is no `_PIC`; a guest
//! booted with `noapic` gets no PCI interrupt from `_PRT`), `_OSC` (the OS
//! gets no native control of PCIe features), sleep states other than S5,
//! SRAT/SLIT, a VGA window.

use crate::acpi_pm;
use crate::hpet::{self, Hpet};
use crate::i8042;
use crate::ioapic;
use crate::lapic;
use crate::legacy;
use crate::machine::{self, irq, ECAM_BASE, ECAM_SIZE, PCI_ADDRESS, PCI_DATA};
use crate::rtc;
use crate::uart;

/// OEM fields of every table header (and of the RSDP).
pub const OEM_ID: [u8; 6] = *b"NANOX ";
pub const OEM_TABLE_ID: [u8; 8] = *b"NANOXVMM";
const OEM_REVISION: u32 = 1;
const CREATOR_ID: [u8; 4] = *b"NANX";
const CREATOR_REVISION: u32 = 1;

/// The FADT reset register: system control port A, where a write with bit 0 set is the fast reset.
pub const RESET_PORT: u16 = legacy::PORT_SYSTEM_A;
pub const RESET_VALUE: u8 = 1;
/// The memory the root bridge forwards to PCI BARs: from the end of ECAM up to
/// the I/O APIC (exclusive), so it stays clear of ECAM and of the I/O APIC,
/// HPET and local APIC pages above it.
pub const PCI_MMIO_BASE: u64 = ECAM_BASE + ECAM_SIZE;
pub const PCI_MMIO_END: u64 = ioapic::DEFAULT_BASE;
/// The I/O APIC id after reset, which the MADT gives.
const IOAPIC_ID: u8 = 0;

/// Bytes before the FACS: the RSDP (36) and padding to the FACS's 64-byte alignment.
const RSDP_SPACE: usize = 64;
const RSDP_LEN: u32 = 36;
const FACS_LEN: u32 = 64;
/// Alignment of the tables after the FACS.
const TABLE_ALIGN: usize = 16;

// Generic address structure: address spaces and access sizes.
const SYSTEM_MEMORY: u8 = 0;
const SYSTEM_IO: u8 = 1;
const ACCESS_BYTE: u8 = 1;
const ACCESS_WORD: u8 = 2;
const ACCESS_DWORD: u8 = 3;

/// FADT: the event block is the status and the enable register, of equal width.
const PM1_EVT_LEN: u8 = 2 * (acpi_pm::PM1_ENABLE - acpi_pm::PM1_STATUS) as u8;
const PM1_CNT_LEN: u8 = 2;
const PM_TMR_LEN: u8 = 4;
/// GPE0: the status half, then the enable half.
const GPE0_LEN: u8 = (acpi_pm::GPE0_END + 1 - acpi_pm::GPE0_STATUS) as u8;
const FADT_MINOR: u8 = 3;
/// Worst-case C2 and C3 latencies above 100 and 1000 µs: neither state is supported.
const C2_NONE: u16 = 0x0FFF;
const C3_NONE: u16 = 0x0FFF;
/// IA-PC boot architecture: legacy devices, an 8042, no VGA, no MSI.
const BOOT_ARCH: u16 = 1 | (1 << 1) | (1 << 2) | (1 << 3);
/// FADT flags: WBINVD works, C1 (HLT) on every processor, no fixed sleep
/// button, no RTC wake status in the fixed registers, the reset register is
/// there. The power button is the fixed one (bit 4 clear).
const FADT_FLAGS: u32 = 1 | (1 << 2) | (1 << 5) | (1 << 6) | (1 << 10);

/// MADT: a dual 8259 is present.
const PCAT_COMPAT: u32 = 1;
/// Interrupt source override flags: polarity active low, trigger level.
const ACTIVE_LOW: u16 = 3;
const LEVEL: u16 = 3 << 2;

/// The last PCI bus ECAM covers (1 MiB of configuration space per bus).
const LAST_BUS: u16 = ((ECAM_SIZE >> 20) - 1) as u16;
/// Ports 0xCF8..=0xCFF: the address and data registers of configuration mechanism #1.
const CONFIG_PORTS: u16 = PCI_DATA + 4 - PCI_ADDRESS;

/// What may vary between machines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    /// Processors (1..=255): local APIC ids and ACPI processor UIDs 0..cpus.
    /// [`crate::machine::Machine`] models one.
    pub cpus: u8,
}

impl Default for Platform {
    fn default() -> Self {
        Self { cpus: 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The tables need `needed` bytes and the buffer is shorter.
    BufferTooSmall { needed: usize },
    /// The base address is not a multiple of 64 (the FACS must be 64-byte aligned).
    Misaligned,
    /// The tables would not end below 4 GiB (the FADT's 32-bit FACS and DSDT fields must reach them).
    AboveFourGiB,
    /// `cpus` is 0.
    NoCpus,
}

/// Where [`build`] put each table (guest physical addresses) and how much of the buffer it used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub rsdp: u64,
    pub facs: u64,
    pub dsdt: u64,
    pub fadt: u64,
    pub madt: u64,
    pub hpet: u64,
    pub mcfg: u64,
    pub xsdt: u64,
    /// Bytes written from the start of the buffer: the region to report to the guest as ACPI data.
    pub len: usize,
}

/// Writes the tables into `buf`, which the guest sees at `base_gpa`: the RSDP
/// first (so a buffer in the BIOS area is found by the usual scan), then
/// FACS, DSDT, FADT, MADT, HPET, MCFG and XSDT. On error nothing is written.
pub fn build(buf: &mut [u8], base_gpa: u64, cfg: &Platform) -> Result<Layout, Error> {
    if cfg.cpus == 0 {
        return Err(Error::NoCpus);
    }
    if !base_gpa.is_multiple_of(64) {
        return Err(Error::Misaligned);
    }
    // A first pass over an empty buffer only counts (the size does not depend on the base).
    let needed = write(
        &mut W {
            out: &mut [],
            pos: 0,
        },
        0,
        cfg,
    )
    .len;
    if base_gpa
        .checked_add(needed as u64)
        .is_none_or(|end| end > 1 << 32)
    {
        return Err(Error::AboveFourGiB);
    }
    if buf.len() < needed {
        return Err(Error::BufferTooSmall { needed });
    }
    Ok(write(&mut W { out: buf, pos: 0 }, base_gpa, cfg))
}

fn write(w: &mut W, base: u64, cfg: &Platform) -> Layout {
    w.zeros(RSDP_SPACE);
    let facs = base + w.pos as u64;
    facs_table(w);
    let dsdt = table(w, base, |w| dsdt_table(w, cfg));
    let fadt = table(w, base, |w| fadt_table(w, facs, dsdt));
    let madt = table(w, base, |w| madt_table(w, cfg.cpus));
    let hpet = table(w, base, hpet_table);
    let mcfg = table(w, base, mcfg_table);
    let xsdt = table(w, base, |w| xsdt_table(w, &[fadt, madt, hpet, mcfg]));
    let len = w.pos;
    w.pos = 0;
    rsdp(w, xsdt);
    w.pos = len;
    Layout {
        rsdp: base,
        facs,
        dsdt,
        fadt,
        madt,
        hpet,
        mcfg,
        xsdt,
        len,
    }
}

/// Aligns, writes one table and returns its address.
fn table(w: &mut W, base: u64, f: impl FnOnce(&mut W)) -> u64 {
    w.align(TABLE_ALIGN);
    let at = base + w.pos as u64;
    f(w);
    at
}

// ------------------------------------------------------------------ writer

/// A cursor over `out`; bytes past its end are only counted (the measuring pass has an empty `out`).
struct W<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl W<'_> {
    fn byte(&mut self, b: u8) {
        if let Some(x) = self.out.get_mut(self.pos) {
            *x = b;
        }
        self.pos += 1;
    }

    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.byte(x);
        }
    }

    fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    fn zeros(&mut self, n: usize) {
        for _ in 0..n {
            self.byte(0);
        }
    }

    fn align(&mut self, a: usize) {
        while !self.pos.is_multiple_of(a) {
            self.byte(0);
        }
    }

    /// Overwrites bytes already written at `at`.
    fn put(&mut self, at: usize, b: &[u8]) {
        if let Some(d) = self.out.get_mut(at..at + b.len()) {
            d.copy_from_slice(b);
        }
    }
}

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0, |s: u8, &x| s.wrapping_add(x))
}

/// The standard table header with the length and checksum left for [`finish`]; returns where it starts.
fn header(w: &mut W, signature: &[u8; 4], revision: u8) -> usize {
    let at = w.pos;
    w.bytes(signature);
    w.zeros(4); // length
    w.byte(revision);
    w.byte(0); // checksum
    w.bytes(&OEM_ID);
    w.bytes(&OEM_TABLE_ID);
    w.u32(OEM_REVISION);
    w.bytes(&CREATOR_ID);
    w.u32(CREATOR_REVISION);
    at
}

/// Sets the length and the checksum of the table started at `at`.
fn finish(w: &mut W, at: usize) {
    w.put(at + 4, &((w.pos - at) as u32).to_le_bytes());
    if let Some(t) = w.out.get(at..w.pos) {
        let c = 0u8.wrapping_sub(sum(t));
        w.put(at + 9, &[c]);
    }
}

/// A generic address structure (bit offset 0).
fn gas(w: &mut W, space: u8, bits: u8, access: u8, address: u64) {
    w.bytes(&[space, bits, 0, access]);
    w.u64(address);
}

// ------------------------------------------------------------------ tables

fn rsdp(w: &mut W, xsdt: u64) {
    let at = w.pos;
    w.bytes(b"RSD PTR ");
    w.byte(0); // checksum of the first 20 bytes
    w.bytes(&OEM_ID);
    w.byte(2); // revision: ACPI 2.0 and later
    w.u32(0); // no RSDT
    w.u32(RSDP_LEN);
    w.u64(xsdt);
    w.byte(0); // extended checksum
    w.zeros(3);
    for (len, field) in [(20, 8), (RSDP_LEN as usize, 32)] {
        if let Some(b) = w.out.get(at..at + len) {
            let c = 0u8.wrapping_sub(sum(b));
            w.put(at + field, &[c]);
        }
    }
}

fn facs_table(w: &mut W) {
    w.bytes(b"FACS");
    w.u32(FACS_LEN);
    // Hardware signature, waking vector, global lock, flags (no S4BIOS, no 64-bit wake), 64-bit waking vector.
    w.zeros(24);
    w.byte(2); // version
    w.zeros(31); // reserved, OSPM flags, reserved
}

fn xsdt_table(w: &mut W, entries: &[u64]) {
    let t = header(w, b"XSDT", 1);
    for &e in entries {
        w.u64(e);
    }
    finish(w, t);
}

/// A block of fixed ACPI hardware: (address space, port, length in bytes, access size); all zero when absent.
type Block = (u8, u16, u8, u8);
const NO_BLOCK: Block = (0, 0, 0, 0);
/// In FADT order: PM1a_EVT, PM1b_EVT, PM1a_CNT, PM1b_CNT, PM2_CNT, PM_TMR, GPE0, GPE1.
const BLOCKS: [Block; 8] = [
    (SYSTEM_IO, acpi_pm::PM1_STATUS, PM1_EVT_LEN, ACCESS_WORD),
    NO_BLOCK,
    (SYSTEM_IO, acpi_pm::PM1_CONTROL, PM1_CNT_LEN, ACCESS_WORD),
    NO_BLOCK,
    NO_BLOCK,
    (SYSTEM_IO, acpi_pm::PM_TIMER, PM_TMR_LEN, ACCESS_DWORD),
    (SYSTEM_IO, acpi_pm::GPE0_STATUS, GPE0_LEN, ACCESS_BYTE),
    NO_BLOCK,
];

fn fadt_table(w: &mut W, facs: u64, dsdt: u64) {
    let t = header(w, b"FACP", 6);
    w.u32(facs as u32); // FIRMWARE_CTRL
    w.u32(dsdt as u32); // DSDT
    w.byte(0); // reserved
    w.byte(0); // preferred power-management profile: unspecified
    w.u16(irq::SCI.into());
    w.u32(acpi_pm::SMI_CMD.into());
    w.byte(acpi_pm::ACPI_ENABLE);
    w.byte(acpi_pm::ACPI_DISABLE);
    w.byte(0); // S4BIOS_REQ: no S4BIOS
    w.byte(0); // PSTATE_CNT: no performance-state control
    for (_, port, _, _) in BLOCKS {
        w.u32(port.into());
    }
    // PM1_EVT_LEN, PM1_CNT_LEN, PM2_CNT_LEN, PM_TMR_LEN, GPE0_BLK_LEN, GPE1_BLK_LEN, GPE1_BASE, CST_CNT.
    w.bytes(&[PM1_EVT_LEN, PM1_CNT_LEN, 0, PM_TMR_LEN, GPE0_LEN, 0, 0, 0]);
    w.u16(C2_NONE);
    w.u16(C3_NONE);
    w.zeros(8); // FLUSH_SIZE, FLUSH_STRIDE, DUTY_OFFSET, DUTY_WIDTH, DAY_ALRM, MON_ALRM: none
    w.byte(rtc::REG_CENTURY);
    w.u16(BOOT_ARCH);
    w.byte(0); // reserved
    w.u32(FADT_FLAGS);
    gas(w, SYSTEM_IO, 8, ACCESS_BYTE, RESET_PORT.into());
    w.byte(RESET_VALUE);
    w.u16(0); // ARM boot architecture
    w.byte(FADT_MINOR);
    w.u64(facs); // X_FIRMWARE_CTRL
    w.u64(dsdt); // X_DSDT
    for (space, port, len, access) in BLOCKS {
        gas(w, space, len * 8, access, port.into());
    }
    // SLEEP_CONTROL_REG and SLEEP_STATUS_REG (hardware-reduced only), hypervisor vendor identity.
    w.zeros(32);
    finish(w, t);
}

fn madt_table(w: &mut W, cpus: u8) {
    let t = header(w, b"APIC", 5);
    w.u32(lapic::DEFAULT_BASE as u32);
    w.u32(PCAT_COMPAT);
    for id in 0..cpus {
        w.bytes(&[0, 8, id, id]); // processor local APIC: ACPI processor UID, APIC id
        w.u32(1); // enabled
    }
    w.bytes(&[1, 12, IOAPIC_ID, 0]);
    w.u32(ioapic::DEFAULT_BASE as u32);
    w.u32(0); // global system interrupt base
    for (isa, flags) in [(irq::TIMER, 0), (irq::SCI, ACTIVE_LOW | LEVEL)] {
        w.bytes(&[2, 10, 0, isa]); // interrupt source override on the ISA bus
        w.u32(machine::ioapic_pin(isa).into());
        w.u16(flags);
    }
    w.bytes(&[4, 6, 0xFF]); // local APIC NMI of every processor
    w.u16(0); // flags: as the bus
    w.byte(1); // LINT1
    finish(w, t);
}

fn hpet_table(w: &mut W) {
    let t = header(w, b"HPET", 1);
    // Event timer block id: bits 31:0 of the general capabilities register.
    w.u32(Hpet::new().read(hpet::REG_ID, 4, 0).unwrap_or(0) as u32);
    gas(w, SYSTEM_MEMORY, 0, 0, hpet::DEFAULT_BASE);
    w.byte(0); // HPET number
    w.u16(0); // minimum clock tick in periodic mode
    w.byte(0); // page protection: none claimed
    finish(w, t);
}

fn mcfg_table(w: &mut W) {
    let t = header(w, b"MCFG", 1);
    w.zeros(8);
    w.u64(ECAM_BASE);
    w.u16(0); // segment group
    w.bytes(&[0, LAST_BUS as u8]);
    w.zeros(4);
    finish(w, t);
}

// --------------------------------------------------------------------- AML

const ZERO_OP: u8 = 0x00;
const ONE_OP: u8 = 0x01;
const NAME_OP: u8 = 0x08;
const BYTE_PREFIX: u8 = 0x0A;
const WORD_PREFIX: u8 = 0x0B;
const DWORD_PREFIX: u8 = 0x0C;
const STRING_PREFIX: u8 = 0x0D;
const SCOPE_OP: u8 = 0x10;
const BUFFER_OP: u8 = 0x11;
const PACKAGE_OP: u8 = 0x12;
const EXT_OP_PREFIX: u8 = 0x5B;
const DEVICE_OP: u8 = 0x82;

/// A compressed EISA id ("PNP0A08"): three letters of five bits and four hex digits, stored big-endian.
const fn eisa_id(id: &[u8; 7]) -> u32 {
    const fn letter(c: u8) -> u32 {
        (c - b'@') as u32
    }
    const fn hex(c: u8) -> u32 {
        (if c <= b'9' { c - b'0' } else { c - b'A' + 10 }) as u32
    }
    let v = (letter(id[0]) << 26)
        | (letter(id[1]) << 21)
        | (letter(id[2]) << 16)
        | (hex(id[3]) << 12)
        | (hex(id[4]) << 8)
        | (hex(id[5]) << 4)
        | hex(id[6]);
    v.swap_bytes()
}

const PNP0A08: u32 = eisa_id(b"PNP0A08");
const PNP0A03: u32 = eisa_id(b"PNP0A03");
const PNP0501: u32 = eisa_id(b"PNP0501");
const PNP0303: u32 = eisa_id(b"PNP0303");
const PNP0B00: u32 = eisa_id(b"PNP0B00");
const PNP0103: u32 = eisa_id(b"PNP0103");
const PNP0C02: u32 = eisa_id(b"PNP0C02");

/// An integer in its shortest encoding.
fn int(w: &mut W, v: u32) {
    match v {
        0 => w.byte(ZERO_OP),
        1 => w.byte(ONE_OP),
        2..=0xFF => w.bytes(&[BYTE_PREFIX, v as u8]),
        0x100..=0xFFFF => {
            w.byte(WORD_PREFIX);
            w.u16(v as u16);
        }
        _ => {
            w.byte(DWORD_PREFIX);
            w.u32(v);
        }
    }
}

fn string(w: &mut W, s: &[u8]) {
    w.byte(STRING_PREFIX);
    w.bytes(s);
    w.byte(0);
}

/// `Name (name, ...)`: the object follows.
fn name(w: &mut W, name: &[u8; 4]) {
    w.byte(NAME_OP);
    w.bytes(name);
}

fn name_int(w: &mut W, n: &[u8; 4], v: u32) {
    name(w, n);
    int(w, v);
}

/// Starts an object with a package length: room for the longest encoding, which [`close`] shrinks.
fn open(w: &mut W) -> usize {
    let at = w.pos;
    w.zeros(4);
    at
}

/// Ends the object [`open`] started: the shortest package length goes in front of the contents.
fn close(w: &mut W, at: usize) {
    let body = w.pos - at - 4;
    // n bytes encode lengths below 2^6, 2^12, 2^20, 2^28; the length counts these bytes too.
    let mut n = 1;
    while n < 4 && body + n >= 1 << (if n == 1 { 6 } else { 8 * n - 4 }) {
        n += 1;
    }
    let len = body + n;
    let lead = if n == 1 {
        len as u8
    } else {
        (((n - 1) as u8) << 6) | (len as u8 & 0x0F)
    };
    w.put(at, &[lead]);
    for i in 1..n {
        w.put(at + i, &[(len >> (8 * i - 4)) as u8]);
    }
    if let Some(b) = w.out.get_mut(at..w.pos) {
        b.copy_within(4.., n);
    }
    w.pos -= 4 - n;
}

fn scope(w: &mut W, path: &[u8]) -> usize {
    w.byte(SCOPE_OP);
    let at = open(w);
    w.bytes(path);
    at
}

fn device(w: &mut W, name: &[u8; 4]) -> usize {
    w.bytes(&[EXT_OP_PREFIX, DEVICE_OP]);
    let at = open(w);
    w.bytes(name);
    at
}

fn package(w: &mut W, elements: u8) -> usize {
    w.byte(PACKAGE_OP);
    let at = open(w);
    w.byte(elements);
    at
}

/// A buffer whose bytes `f` writes (once to measure them, once for real).
fn buffer(w: &mut W, f: impl Fn(&mut W)) {
    let mut m = W {
        out: &mut [],
        pos: 0,
    };
    f(&mut m);
    w.byte(BUFFER_OP);
    let at = open(w);
    int(w, m.pos as u32);
    f(w);
    close(w, at);
}

/// A resource template: the descriptors `f` writes and the end tag (checksum 0: not checked).
fn resources(w: &mut W, f: impl Fn(&mut W)) {
    name(w, b"_CRS");
    buffer(w, |w| {
        f(w);
        w.bytes(&[0x79, 0]);
    });
}

/// I/O port descriptor for `len` ports at a fixed `port` (16-bit decode).
fn io(w: &mut W, port: u16, len: u16) {
    w.bytes(&[0x47, 1]);
    w.u16(port);
    w.u16(port);
    w.bytes(&[1, len as u8]);
}

/// IRQ descriptor without flags: edge, active high, exclusive.
fn irq_line(w: &mut W, n: u8) {
    w.byte(0x22);
    w.u16(1 << n);
}

fn memory32_fixed(w: &mut W, base: u64, len: u64) {
    w.bytes(&[0x86, 9, 0, 1]); // read-write
    w.u32(base as u32);
    w.u32(len as u32);
}

/// Resource types of the address space descriptors.
const MEMORY_RANGE: u8 = 0;
const IO_RANGE: u8 = 1;
const BUS_RANGE: u8 = 2;
/// General flags: producer, positive decode, minimum and maximum fixed.
const FIXED_WINDOW: u8 = (1 << 2) | (1 << 3);
/// I/O: ISA and non-ISA ranges.
const ENTIRE_RANGE: u8 = 3;
/// Memory: read-write, not cacheable.
const READ_WRITE: u8 = 1;

/// Word address space descriptor of the window `min..=max` (no translation).
fn word_window(w: &mut W, kind: u8, type_flags: u8, min: u16, max: u16) {
    w.bytes(&[0x88, 13, 0, kind, FIXED_WINDOW, type_flags]);
    for v in [0, min, max, 0, max.wrapping_sub(min).wrapping_add(1)] {
        w.u16(v);
    }
}

/// DWord memory descriptor of the window `min..=max` (no translation).
fn dword_window(w: &mut W, min: u32, max: u32) {
    w.bytes(&[0x87, 23, 0, MEMORY_RANGE, FIXED_WINDOW, READ_WRITE]);
    for v in [0, min, max, 0, max - min + 1] {
        w.u32(v);
    }
}

/// A device with a compressed EISA `_HID` and the resources `f` writes.
fn simple_device(w: &mut W, n: &[u8; 4], hid: u32, f: impl Fn(&mut W)) {
    let d = device(w, n);
    name_int(w, b"_HID", hid);
    resources(w, f);
    close(w, d);
}

/// `_PRT` in APIC mode: every slot and pin to its I/O APIC input. INTA of a
/// slot is where [`machine::pci_pin`] wires it; INTB..INTD rotate like the
/// next slots' INTA (the q35 swizzle). Source 0: the entry names a global
/// system interrupt (level, active low), not a link device.
fn prt(w: &mut W) {
    name(w, b"_PRT");
    let all = package(w, 32 * 4);
    for slot in 0..32u8 {
        for pin in 0..4u8 {
            let e = package(w, 4);
            int(w, (u32::from(slot) << 16) | 0xFFFF); // any function of the device
            int(w, pin.into());
            int(w, 0);
            int(w, machine::pci_pin(slot + pin).into());
            close(w, e);
        }
    }
    close(w, all);
}

fn dsdt_table(w: &mut W, cfg: &Platform) {
    let t = header(w, b"DSDT", 2);
    let sb = scope(w, b"\\_SB_");

    let pci = device(w, b"PCI0");
    name_int(w, b"_HID", PNP0A08);
    name_int(w, b"_CID", PNP0A03);
    for n in [b"_SEG", b"_BBN", b"_UID"] {
        name_int(w, n, 0);
    }
    resources(w, |w| {
        word_window(w, BUS_RANGE, 0, 0, LAST_BUS);
        io(w, PCI_ADDRESS, CONFIG_PORTS);
        word_window(w, IO_RANGE, ENTIRE_RANGE, 0, PCI_ADDRESS - 1);
        word_window(
            w,
            IO_RANGE,
            ENTIRE_RANGE,
            PCI_ADDRESS + CONFIG_PORTS,
            0xFFFF,
        );
        dword_window(w, PCI_MMIO_BASE as u32, (PCI_MMIO_END - 1) as u32);
    });
    prt(w);
    simple_device(w, b"COM1", PNP0501, |w| {
        io(w, uart::BASE, uart::PORTS);
        irq_line(w, irq::COM1);
    });
    simple_device(w, b"KBD_", PNP0303, |w| {
        io(w, i8042::PORT_DATA, 1);
        io(w, i8042::PORT_STATUS, 1);
        irq_line(w, irq::KEYBOARD);
    });
    simple_device(w, b"RTC_", PNP0B00, |w| {
        io(w, rtc::PORT_INDEX, rtc::PORT_DATA + 1 - rtc::PORT_INDEX);
        irq_line(w, irq::RTC);
    });
    close(w, pci);

    simple_device(w, b"HPET", PNP0103, |w| {
        memory32_fixed(w, hpet::DEFAULT_BASE, hpet::SIZE)
    });
    simple_device(w, b"MBRD", PNP0C02, |w| {
        memory32_fixed(w, ECAM_BASE, ECAM_SIZE)
    });
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for i in 0..cfg.cpus {
        let d = device(
            w,
            &[
                b'C',
                b'0',
                HEX[usize::from(i >> 4)],
                HEX[usize::from(i & 0xF)],
            ],
        );
        name(w, b"_HID");
        string(w, b"ACPI0007");
        name_int(w, b"_UID", i.into());
        close(w, d);
    }
    close(w, sb);

    // Sleep states: SLP_TYPa, SLP_TYPb, two reserved.
    for (n, typ) in [(b"_S0_", 0), (b"_S5_", acpi_pm::SLEEP_S5)] {
        name(w, n);
        let p = package(w, 4);
        for v in [typ, typ, 0, 0] {
            int(w, v.into());
        }
        close(w, p);
    }
    finish(w, t);
}
