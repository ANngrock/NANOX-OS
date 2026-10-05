//! Direct boot of a Linux bzImage into a guest without firmware, by the
//! 64-bit entry of the x86 boot protocol (Documentation/arch/x86/boot.rst):
//! the protected-mode kernel copied to its preferred address, the "zero page"
//! (`struct boot_params`) with the setup header taken from the image, the
//! command line, an initrd, the e820 memory map and the ACPI RSDP address,
//! identity-mapped page tables for the low 4 GiB and a GDT with the
//! `__BOOT_CS`/`__BOOT_DS` selectors the kernel expects.
//!
//! The result is the vCPU state at the entry: long mode, paging on, CS =
//! 0x10 (64-bit code), the data segments 0x18, interrupts off, RSI = the
//! zero page, RIP = the load address + 0x200. As in [`crate::load`],
//! everything is computed and checked before the first write, so an error
//! leaves guest memory untouched.
//!
//! Fixed low-memory layout: GDT at [`GDT_GPA`], zero page at
//! [`BOOT_PARAMS_GPA`], page tables from [`PAGE_TABLES_GPA`] (PML4, PDPT and
//! four page directories of 2 MiB pages), the command line at
//! [`CMDLINE_GPA`]. The kernel goes to its preferred address (aligned to its
//! alignment), the initrd to the top of the RAM below 4 GiB and below the
//! kernel's `initrd_addr_max`.

use crate::GuestMemory;

/// Offsets in the image and in the zero page (they coincide for the setup header).
mod off {
    pub const ACPI_RSDP_ADDR: usize = 0x070;
    pub const EXT_RAMDISK_IMAGE: usize = 0x0C0;
    pub const EXT_RAMDISK_SIZE: usize = 0x0C4;
    pub const EXT_CMD_LINE_PTR: usize = 0x0C8;
    pub const E820_ENTRIES: usize = 0x1E8;
    pub const SETUP_SECTS: usize = 0x1F1;
    pub const BOOT_FLAG: usize = 0x1FE;
    pub const JUMP: usize = 0x200;
    pub const HEADER: usize = 0x202;
    pub const VERSION: usize = 0x206;
    pub const TYPE_OF_LOADER: usize = 0x210;
    pub const LOADFLAGS: usize = 0x211;
    pub const RAMDISK_IMAGE: usize = 0x218;
    pub const RAMDISK_SIZE: usize = 0x21C;
    pub const CMD_LINE_PTR: usize = 0x228;
    pub const INITRD_ADDR_MAX: usize = 0x22C;
    pub const KERNEL_ALIGNMENT: usize = 0x230;
    pub const RELOCATABLE_KERNEL: usize = 0x234;
    pub const XLOADFLAGS: usize = 0x236;
    pub const CMDLINE_SIZE: usize = 0x238;
    pub const PREF_ADDRESS: usize = 0x258;
    pub const INIT_SIZE: usize = 0x260;
    pub const E820_TABLE: usize = 0x2D0;
}

/// "HdrS".
const HDRS: u32 = 0x5372_6448;
/// Protocol 2.12 introduced `xloadflags` and the 64-bit entry flag.
pub const MIN_VERSION: u16 = 0x020C;
const XLF_KERNEL_64: u16 = 1;
const LOADED_HIGH: u8 = 1;
/// "Undefined" boot loader id.
const LOADER_UNDEFINED: u8 = 0xFF;

pub const GDT_GPA: u64 = 0x500;
pub const BOOT_PARAMS_GPA: u64 = 0x7000;
pub const PAGE_TABLES_GPA: u64 = 0x9000;
pub const CMDLINE_GPA: u64 = 0x2_0000;
/// Conventional memory ends here (the EBDA follows, then the legacy video and BIOS areas).
pub const LOW_RAM_END: u64 = 0x9_FC00;
/// RAM above this goes above 4 GiB: the 32-bit window below 4 GiB belongs to
/// the PCI configuration window (ECAM at 0xB000_0000), BARs and the chipset.
pub const LOW_RAM_LIMIT: u64 = 0xB000_0000;
pub const HIGH_RAM_BASE: u64 = 1 << 32;
/// The kernel never goes below 1 MiB.
pub const MIN_LOAD_ADDRESS: u64 = 0x10_0000;
/// Entries of `boot_params.e820_table`.
pub const E820_MAX: usize = 128;

pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;
pub const E820_ACPI: u32 = 3;

/// GDT selectors of the boot protocol.
pub const BOOT_CS: u16 = 0x10;
pub const BOOT_DS: u16 = 0x18;

pub const CR0_PE: u64 = 1;
pub const CR0_PG: u64 = 1 << 31;
pub const CR4_PAE: u64 = 1 << 5;
pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinuxConfig<'a> {
    /// Guest RAM in bytes; above [`LOW_RAM_LIMIT`] it continues at [`HIGH_RAM_BASE`].
    pub ram_bytes: u64,
    /// The kernel command line (without the terminating NUL).
    pub cmdline: &'a [u8],
    /// Where the caller put the ACPI tables: the RSDP's address and the region they occupy
    /// (reported as ACPI memory in the e820 map). The region must lie in the RAM between
    /// 1 MiB and 4 GiB, contain the 20-byte RSDP and not overlap the kernel or the initrd.
    pub acpi: Option<Acpi>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acpi {
    pub rsdp_gpa: u64,
    pub region_gpa: u64,
    pub region_len: u64,
}

/// What the image's setup header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupHeader {
    pub version: u16,
    pub setup_sects: u8,
    pub loadflags: u8,
    pub xloadflags: u16,
    pub kernel_alignment: u32,
    pub relocatable: bool,
    pub cmdline_size: u32,
    pub initrd_addr_max: u32,
    pub pref_address: u64,
    pub init_size: u32,
    /// Where the protected-mode kernel starts in the image.
    pub payload_offset: usize,
    /// One past the last byte of the setup header (`0x202` + the jump's displacement).
    pub header_end: usize,
}

/// The vCPU state at the kernel's 64-bit entry and where things went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinuxEntry {
    pub rip: u64,
    pub rsi: u64,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub gdt_base: u64,
    pub gdt_limit: u16,
    pub cs: u16,
    pub ds: u16,
    /// Only bit 1 (reserved, always 1): interrupts off.
    pub rflags: u64,
    pub load_address: u64,
    pub initrd_gpa: u64,
    pub initrd_len: u64,
    pub e820_entries: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinuxError {
    /// No 0xAA55 boot flag or no "HdrS": not a bzImage.
    NotBzImage,
    /// The image is shorter than its own header or setup says.
    Truncated,
    /// Boot protocol older than 2.12.
    ProtocolTooOld,
    /// The kernel has no 64-bit entry point (`XLF_KERNEL_64`).
    No64BitEntry,
    /// The kernel must be loaded at an address this loader does not use (not relocatable,
    /// and its preferred address is below 1 MiB or not aligned).
    NotLoadable,
    /// The command line is longer than the kernel accepts or than the space for it.
    CmdlineTooLong,
    /// The kernel, the initrd or the tables do not fit into the RAM.
    GuestTooSmall,
    /// The ACPI region overlaps something the loader places, or is not RAM, or the RSDP
    /// is not inside it.
    BadAcpiRegion,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from(u32_at(b, o)) | u64::from(u32_at(b, o + 4)) << 32
}

/// Reads and checks the setup header of a bzImage.
pub fn parse_header(image: &[u8]) -> Result<SetupHeader, LinuxError> {
    if image.len() < off::INIT_SIZE + 4 {
        return Err(LinuxError::NotBzImage);
    }
    if u16_at(image, off::BOOT_FLAG) != 0xAA55 || u32_at(image, off::HEADER) != HDRS {
        return Err(LinuxError::NotBzImage);
    }
    let version = u16_at(image, off::VERSION);
    if version < MIN_VERSION {
        return Err(LinuxError::ProtocolTooOld);
    }
    let header_end = off::HEADER + usize::from(image[off::JUMP + 1]);
    let setup_sects = match image[off::SETUP_SECTS] {
        0 => 4,
        n => n,
    };
    let payload_offset = (usize::from(setup_sects) + 1) * 512;
    if header_end > image.len() || payload_offset >= image.len() {
        return Err(LinuxError::Truncated);
    }
    let xloadflags = u16_at(image, off::XLOADFLAGS);
    if xloadflags & XLF_KERNEL_64 == 0 {
        return Err(LinuxError::No64BitEntry);
    }
    Ok(SetupHeader {
        version,
        setup_sects,
        loadflags: image[off::LOADFLAGS],
        xloadflags,
        kernel_alignment: u32_at(image, off::KERNEL_ALIGNMENT),
        relocatable: image[off::RELOCATABLE_KERNEL] != 0,
        cmdline_size: u32_at(image, off::CMDLINE_SIZE),
        initrd_addr_max: u32_at(image, off::INITRD_ADDR_MAX),
        pref_address: u64_at(image, off::PREF_ADDRESS),
        init_size: u32_at(image, off::INIT_SIZE),
        payload_offset,
        header_end,
    })
}

/// End of RAM below 4 GiB.
fn low_ram_end(ram: u64) -> u64 {
    ram.min(LOW_RAM_LIMIT)
}

/// Is `[start, start + len)` RAM in a guest with `ram` bytes?
fn is_ram(ram: u64, start: u64, len: u64) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let in_low_conventional = end <= LOW_RAM_END;
    let in_low = start >= MIN_LOAD_ADDRESS && end <= low_ram_end(ram);
    let high_len = ram.saturating_sub(LOW_RAM_LIMIT);
    let in_high = start >= HIGH_RAM_BASE && end <= HIGH_RAM_BASE + high_len;
    in_low_conventional || in_low || in_high
}

fn overlaps(a: u64, a_len: u64, b: u64, b_len: u64) -> bool {
    a < b.saturating_add(b_len) && b < a.saturating_add(a_len)
}

/// What goes where, computed before any write.
struct Plan {
    hdr: SetupHeader,
    load: u64,
    initrd: u64,
    e820: [(u64, u64, u32); 8],
    e820_len: usize,
}

fn plan(image: &[u8], initrd_len: u64, cfg: &LinuxConfig<'_>) -> Result<Plan, LinuxError> {
    let hdr = parse_header(image)?;
    let cmd_max = u64::from(hdr.cmdline_size);
    let cmd_len = cfg.cmdline.len() as u64;
    // The command line and its NUL must fit the kernel's limit and the space below the EBDA.
    if cmd_len > cmd_max || CMDLINE_GPA + cmd_len + 1 > LOW_RAM_END {
        return Err(LinuxError::CmdlineTooLong);
    }
    let align = u64::from(hdr.kernel_alignment).max(1);
    if !align.is_power_of_two() {
        return Err(LinuxError::NotLoadable);
    }
    let load = if hdr.relocatable {
        hdr.pref_address
            .max(MIN_LOAD_ADDRESS)
            .next_multiple_of(align)
    } else if hdr.pref_address >= MIN_LOAD_ADDRESS && hdr.pref_address % align == 0 {
        hdr.pref_address
    } else {
        return Err(LinuxError::NotLoadable);
    };
    let payload_len = (image.len() - hdr.payload_offset) as u64;
    // The kernel decompresses in place: it needs init_size bytes from its load address.
    let kernel_len = payload_len.max(u64::from(hdr.init_size));
    if !is_ram(cfg.ram_bytes, load, kernel_len) {
        return Err(LinuxError::GuestTooSmall);
    }
    let kernel_end = load + kernel_len;
    // The initrd at the top of the low RAM, below initrd_addr_max, page aligned, above the kernel.
    let initrd = if initrd_len == 0 {
        0
    } else {
        let ceiling = low_ram_end(cfg.ram_bytes).min(u64::from(hdr.initrd_addr_max) + 1);
        match ceiling.checked_sub(initrd_len) {
            Some(top) if top & !0xFFF >= kernel_end => top & !0xFFF,
            _ => return Err(LinuxError::GuestTooSmall),
        }
    };
    // The e820 map: conventional memory, the EBDA and the BIOS area, low RAM (with the ACPI
    // region cut out), RAM above 4 GiB.
    let mut e820 = [(0u64, 0u64, 0u32); 8];
    let mut n = 0;
    let mut push = |start: u64, len: u64, kind: u32| {
        if len > 0 {
            e820[n] = (start, len, kind);
            n += 1;
        }
    };
    push(0, LOW_RAM_END, E820_RAM);
    push(LOW_RAM_END, 0xA_0000 - LOW_RAM_END, E820_RESERVED);
    push(0xF_0000, 0x1_0000, E820_RESERVED);
    let low_end = low_ram_end(cfg.ram_bytes);
    match cfg.acpi {
        Some(a) => {
            // Checked once: a region that overflows is not in the low RAM either.
            let region_end = a.region_gpa.saturating_add(a.region_len);
            let in_low = a.region_gpa >= MIN_LOAD_ADDRESS && region_end <= low_end;
            let rsdp_inside = a.rsdp_gpa >= a.region_gpa
                && a.rsdp_gpa.checked_add(20).is_some_and(|e| e <= region_end);
            if a.region_len == 0
                || !in_low
                || !rsdp_inside
                || overlaps(a.region_gpa, a.region_len, load, kernel_len)
                || overlaps(a.region_gpa, a.region_len, initrd, initrd_len)
            {
                return Err(LinuxError::BadAcpiRegion);
            }
            push(MIN_LOAD_ADDRESS, a.region_gpa - MIN_LOAD_ADDRESS, E820_RAM);
            push(a.region_gpa, a.region_len, E820_ACPI);
            let after = region_end;
            push(after, low_end - after, E820_RAM);
        }
        None => push(MIN_LOAD_ADDRESS, low_end - MIN_LOAD_ADDRESS, E820_RAM),
    }
    push(
        HIGH_RAM_BASE,
        cfg.ram_bytes.saturating_sub(LOW_RAM_LIMIT),
        E820_RAM,
    );
    Ok(Plan {
        hdr,
        load,
        initrd,
        e820,
        e820_len: n,
    })
}

/// A GDT descriptor: base 0, limit 0xFFFFF, granularity 4 KiB, present, DPL 0, with the given
/// type and flags (L for 64-bit code, D/B for 32-bit data).
fn descriptor(access: u8, flags: u8) -> u64 {
    0xFFFF | 0xF << 48 | u64::from(access) << 40 | u64::from(flags) << 52
}

/// Loads `image` (a bzImage) and `initrd` into the guest and returns the entry state.
pub fn load_linux<M: GuestMemory + ?Sized>(
    mem: &mut M,
    image: &[u8],
    initrd: &[u8],
    cfg: &LinuxConfig<'_>,
) -> Result<LinuxEntry, LinuxError> {
    let p = plan(image, initrd.len() as u64, cfg)?;
    let h = p.hdr;

    // The protected-mode kernel; the rest of its init_size is the kernel's own business.
    mem.write(p.load, &image[h.payload_offset..]);
    if !initrd.is_empty() {
        mem.write(p.initrd, initrd);
    }
    mem.write(CMDLINE_GPA, cfg.cmdline);
    mem.write(CMDLINE_GPA + cfg.cmdline.len() as u64, &[0]);

    // The zero page: zeros, the setup header from the image, then what the loader fills in.
    let mut zp = [0u8; 4096];
    zp[off::SETUP_SECTS..h.header_end].copy_from_slice(&image[off::SETUP_SECTS..h.header_end]);
    zp[off::TYPE_OF_LOADER] = LOADER_UNDEFINED;
    zp[off::LOADFLAGS] |= LOADED_HIGH;
    zp[off::CMD_LINE_PTR..off::CMD_LINE_PTR + 4]
        .copy_from_slice(&(CMDLINE_GPA as u32).to_le_bytes());
    zp[off::EXT_CMD_LINE_PTR..off::EXT_CMD_LINE_PTR + 4].copy_from_slice(&0u32.to_le_bytes());
    zp[off::RAMDISK_IMAGE..off::RAMDISK_IMAGE + 4]
        .copy_from_slice(&(p.initrd as u32).to_le_bytes());
    zp[off::RAMDISK_SIZE..off::RAMDISK_SIZE + 4]
        .copy_from_slice(&(initrd.len() as u32).to_le_bytes());
    zp[off::EXT_RAMDISK_IMAGE..off::EXT_RAMDISK_IMAGE + 4].copy_from_slice(&0u32.to_le_bytes());
    zp[off::EXT_RAMDISK_SIZE..off::EXT_RAMDISK_SIZE + 4].copy_from_slice(&0u32.to_le_bytes());
    if let Some(a) = cfg.acpi {
        zp[off::ACPI_RSDP_ADDR..off::ACPI_RSDP_ADDR + 8].copy_from_slice(&a.rsdp_gpa.to_le_bytes());
    }
    zp[off::E820_ENTRIES] = p.e820_len as u8;
    for (i, &(start, len, kind)) in p.e820[..p.e820_len].iter().enumerate() {
        let at = off::E820_TABLE + 20 * i;
        zp[at..at + 8].copy_from_slice(&start.to_le_bytes());
        zp[at + 8..at + 16].copy_from_slice(&len.to_le_bytes());
        zp[at + 16..at + 20].copy_from_slice(&kind.to_le_bytes());
    }
    mem.write(BOOT_PARAMS_GPA, &zp);

    // GDT: null, null, __BOOT_CS (64-bit code), __BOOT_DS (data).
    let gdt = [
        0u64,
        0,
        descriptor(0x9B, 0xA), // present, code, execute/read, accessed; G, L
        descriptor(0x93, 0xC), // present, data, read/write, accessed; G, D/B
    ];
    for (i, d) in gdt.iter().enumerate() {
        mem.write(GDT_GPA + 8 * i as u64, &d.to_le_bytes());
    }

    // Identity map of the low 4 GiB with 2 MiB pages: PML4 -> PDPT -> 4 PDs.
    let pml4 = PAGE_TABLES_GPA;
    let pdpt = pml4 + 0x1000;
    let pd0 = pdpt + 0x1000;
    let mut page = [0u8; 4096];
    page[..8].copy_from_slice(&(pdpt | 3).to_le_bytes());
    mem.write(pml4, &page);
    let mut page = [0u8; 4096];
    for i in 0..4u64 {
        let at = 8 * i as usize;
        page[at..at + 8].copy_from_slice(&((pd0 + 0x1000 * i) | 3).to_le_bytes());
    }
    mem.write(pdpt, &page);
    for d in 0..4u64 {
        let mut page = [0u8; 4096];
        for e in 0..512u64 {
            let pa = (d << 30) | (e << 21);
            let at = 8 * e as usize;
            page[at..at + 8].copy_from_slice(&(pa | 0x83).to_le_bytes()); // P | RW | PS
        }
        mem.write(pd0 + 0x1000 * d, &page);
    }

    Ok(LinuxEntry {
        rip: p.load + 0x200,
        rsi: BOOT_PARAMS_GPA,
        cr0: CR0_PE | CR0_PG,
        cr3: PAGE_TABLES_GPA,
        cr4: CR4_PAE,
        efer: EFER_LME | EFER_LMA,
        gdt_base: GDT_GPA,
        gdt_limit: (8 * gdt.len() - 1) as u16,
        cs: BOOT_CS,
        ds: BOOT_DS,
        rflags: 2,
        load_address: p.load,
        initrd_gpa: p.initrd,
        initrd_len: initrd.len() as u64,
        e820_entries: p.e820_len as u8,
    })
}
