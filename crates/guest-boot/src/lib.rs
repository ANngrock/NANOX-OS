//! Builds the NANOX M0 handoff (BootInfo v1, docs/specs/BOOT-M0.md) inside
//! a guest's memory, so the M10 VMM can start a candidate kernel ELF the way
//! the UEFI loader does, without firmware (docs/specs/M10-VMM.md, item 1).
//!
//! The result is the state the loader's transition leaves: long mode, CR3 =
//! the guest PML4, RSP = `STACK_TOP`, RDI = `HANDOFF_BASE`, RIP = the ELF
//! entry, with the kernel segments, the handoff arena and the stack mapped
//! exactly as `boot/uefi` maps them (W^X, NX on data). The reservation table
//! also names the page-table pool and a transition page, which
//! `BootInfo::validate_buffers` requires.
//!
//! Guest-physical layout from [`LOAD_BASE`]: segments, the 256 KiB arena
//! (BootInfo, reserved ranges at +4 KiB, load segments at +8 KiB, memory map
//! at +16 KiB — the loader's offsets), the 64 KiB stack, the page tables,
//! the transition page. Everything is computed and checked before the first
//! write, so an error leaves guest memory untouched.
//!
//! [`linux`] does the same for a Linux bzImage (the x86 boot protocol's 64-bit entry),
//! so the VMM can boot Linux without firmware.

#![no_std]
#![forbid(unsafe_code)]

use boot_protocol::{self as bp, elf, BootInfo};

pub mod linux;

/// Guest-physical memory of the guest being prepared.
pub trait GuestMemory {
    /// Writes `bytes` at `gpa`; the loader only writes inside
    /// [`Config::ram_bytes`].
    fn write(&mut self, gpa: u64, bytes: &[u8]);
    fn read(&mut self, gpa: u64, out: &mut [u8]);
}

/// Start of the image; the first MiB is left alone, as on a PC.
pub const LOAD_BASE: u64 = 0x10_0000;
pub const RANGES_OFFSET: u64 = 4096;
pub const SEGMENTS_OFFSET: u64 = 8192;
pub const MAP_OFFSET: u64 = 16384;
/// UEFI descriptor stride as OVMF reports it (the ABI allows >= 40).
pub const DESCRIPTOR_SIZE: u32 = 48;
const PAGE: u64 = bp::PAGE_SIZE;
const EFI_CONVENTIONAL: u32 = 7;
const EFI_LOADER_DATA: u32 = 2;
const EFI_MEMORY_WB: u64 = 8;
const MAX_RANGES: usize = bp::MAX_LOAD_SEGMENTS as usize + 4;

/// The loader contract the kernel was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `codex/m0`: page tables in a pool of exactly the pages they need.
    M0,
    /// `codex/m1-m8-continuation`: a pool of [`M1_PAGE_TABLES_POOL_PAGES`]
    /// starting with the PML4, also mapped RW/NX at
    /// [`M1_PAGE_TABLES_BASE`], where the kernel's VMM adopts it.
    M1,
}

/// `boot_protocol::PAGE_TABLES_BASE` on `codex/m1-m8-continuation`
/// (duplicated until that branch is merged).
pub const M1_PAGE_TABLES_BASE: u64 = 0xffff_ffff_9100_0000;
/// `boot_protocol::PAGE_TABLES_POOL_PAGES` on the same branch.
pub const M1_PAGE_TABLES_POOL_PAGES: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Guest RAM: guest-physical 0..ram_bytes.
    pub ram_bytes: u64,
    /// BootInfo FLAG_TEST_PROFILE (the kernel may use isa-debug-exit).
    pub test_profile: bool,
    /// BootInfo boot_epoch (M0 test scenarios select on it).
    pub boot_epoch: u64,
    pub protocol: Protocol,
}

/// Initial vCPU state and where things went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub rip: u64,
    pub rsp: u64,
    pub rdi: u64,
    pub cr3: u64,
    /// Guest-physical BootInfo (start of the arena).
    pub boot_info_gpa: u64,
    /// First guest-physical byte after the image.
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadError {
    Elf(elf::ElfError),
    /// The layout does not fit into [`Config::ram_bytes`].
    GuestTooSmall,
    /// A virtual address outside the one page directory the M0 windows
    /// share (never for inputs `elf::parse` accepts).
    Window,
    /// The page tables need more than the M1 pool holds.
    TablePool,
}

/// What goes where, computed before any write.
struct Layout {
    image: elf::ElfImage,
    segments: [u64; elf::MAX_PROGRAM_HEADERS],
    arena: u64,
    stack: u64,
    tables: u64,
    table_pages: u64,
    transition: u64,
    end: u64,
}

/// All M0 virtual windows (kernel, handoff, stack) lie under PML4 slot 511,
/// PDPT slot 510: one PML4, one PDPT, one PD and a PT per 2 MiB touched.
fn pd_slot(va: u64) -> Result<u64, LoadError> {
    if (va >> 39) & 511 != 511 || (va >> 30) & 511 != 510 || va >> 48 != 0xFFFF {
        return Err(LoadError::Window);
    }
    Ok((va >> 21) & 511)
}

fn layout(elf_bytes: &[u8], cfg: &Config) -> Result<Layout, LoadError> {
    let image = elf::parse(elf_bytes).map_err(LoadError::Elf)?;
    let mut touched = [0u64; 8];
    let mut touch = |va: u64, pages: u64| -> Result<(), LoadError> {
        for p in 0..pages {
            let slot = pd_slot(va + p * PAGE)?;
            touched[(slot / 64) as usize] |= 1 << (slot % 64);
        }
        Ok(())
    };
    let mut cursor = LOAD_BASE;
    let mut segments = [0; elf::MAX_PROGRAM_HEADERS];
    for (i, s) in image.segments().iter().enumerate() {
        touch(s.virt_start, s.page_count())?;
        segments[i] = cursor;
        cursor += s.page_count() * PAGE;
    }
    touch(bp::HANDOFF_BASE, bp::HANDOFF_MAPPED_SIZE / PAGE)?;
    touch(bp::STACK_TOP - bp::STACK_SIZE, bp::STACK_SIZE / PAGE)?;
    if cfg.protocol == Protocol::M1 {
        touch(M1_PAGE_TABLES_BASE, M1_PAGE_TABLES_POOL_PAGES)?;
    }
    let needed = 3 + touched
        .iter()
        .map(|w| u64::from(w.count_ones()))
        .sum::<u64>();
    let table_pages = match cfg.protocol {
        Protocol::M0 => needed,
        Protocol::M1 if needed <= M1_PAGE_TABLES_POOL_PAGES => M1_PAGE_TABLES_POOL_PAGES,
        Protocol::M1 => return Err(LoadError::TablePool),
    };
    let arena = cursor;
    let stack = arena + bp::HANDOFF_MAPPED_SIZE;
    let tables = stack + bp::STACK_SIZE;
    let transition = tables + table_pages * PAGE;
    let end = transition + PAGE;
    if end > cfg.ram_bytes {
        return Err(LoadError::GuestTooSmall);
    }
    Ok(Layout {
        image,
        segments,
        arena,
        stack,
        tables,
        table_pages,
        transition,
        end,
    })
}

/// Guest page tables in a preallocated pool (sized by `layout`).
struct Tables {
    base: u64,
    used: u64,
    pages: u64,
}

fn read_u64<M: GuestMemory + ?Sized>(mem: &mut M, gpa: u64) -> u64 {
    let mut b = [0u8; 8];
    mem.read(gpa, &mut b);
    u64::from_le_bytes(b)
}

fn write_u64<M: GuestMemory + ?Sized>(mem: &mut M, gpa: u64, v: u64) {
    mem.write(gpa, &v.to_le_bytes());
}

impl Tables {
    /// Maps `pages` pages at `va` to `pa`, W^X like the UEFI loader: leaf
    /// P | RW if writable | NX unless executable; parents P | RW.
    fn map<M: GuestMemory + ?Sized>(
        &mut self,
        mem: &mut M,
        va: u64,
        pa: u64,
        pages: u64,
        w: bool,
        x: bool,
    ) {
        for p in 0..pages {
            let va = va + p * PAGE;
            let mut table = self.base;
            for shift in [39, 30, 21] {
                let slot = table + 8 * ((va >> shift) & 511);
                let mut e = read_u64(mem, slot);
                if e == 0 {
                    // `layout` counted every table; exhaustion is a bug.
                    assert!(self.used < self.pages, "page-table pool exhausted");
                    e = (self.base + self.used * PAGE) | 3;
                    self.used += 1;
                    write_u64(mem, slot, e);
                }
                table = e & 0x000F_FFFF_FFFF_F000;
            }
            let slot = table + 8 * ((va >> 12) & 511);
            // Segments, arena and stack are disjoint (parse and constants).
            assert_eq!(read_u64(mem, slot), 0, "overlapping mapping");
            let leaf = (pa + p * PAGE) | 1 | u64::from(w) << 1 | u64::from(!x) << 63;
            write_u64(mem, slot, leaf);
        }
    }
}

fn zero<M: GuestMemory + ?Sized>(mem: &mut M, gpa: u64, len: u64) {
    const Z: [u8; 4096] = [0; 4096];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(4096);
        mem.write(gpa + done, &Z[..n as usize]);
        done += n;
    }
}

/// BootInfo v1 in its fixed little-endian layout (offsets asserted in
/// boot-protocol).
pub fn encode_boot_info(i: &BootInfo) -> [u8; 160] {
    let mut b = [0u8; 160];
    let mut put = |off: usize, v: &[u8]| b[off..off + v.len()].copy_from_slice(v);
    put(0, &i.magic);
    put(8, &i.major.to_le_bytes());
    put(10, &i.minor.to_le_bytes());
    put(12, &i.header_size.to_le_bytes());
    put(16, &i.total_size.to_le_bytes());
    put(24, &i.flags.to_le_bytes());
    put(32, &i.memory_map_phys.to_le_bytes());
    put(40, &i.memory_map_virt.to_le_bytes());
    put(48, &i.memory_map_len.to_le_bytes());
    put(56, &i.memory_descriptor_size.to_le_bytes());
    put(60, &i.memory_descriptor_version.to_le_bytes());
    put(64, &i.reserved_ranges_phys.to_le_bytes());
    put(72, &i.reserved_ranges_virt.to_le_bytes());
    put(80, &i.reserved_ranges_count.to_le_bytes());
    put(84, &i.reserved_ranges_stride.to_le_bytes());
    put(88, &i.rsdp_phys.to_le_bytes());
    put(96, &i.kernel_entry_virt.to_le_bytes());
    put(104, &i.load_segments_phys.to_le_bytes());
    put(112, &i.load_segments_virt.to_le_bytes());
    put(120, &i.load_segments_count.to_le_bytes());
    put(124, &i.load_segments_stride.to_le_bytes());
    put(128, &i.pml4_phys.to_le_bytes());
    put(136, &i.stack_top_virt.to_le_bytes());
    put(144, &i.serial_io_port.to_le_bytes());
    put(146, &i.reserved0);
    put(152, &i.boot_epoch.to_le_bytes());
    b
}

fn range(start: u64, bytes: u64, kind: u32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..8].copy_from_slice(&start.to_le_bytes());
    b[8..16].copy_from_slice(&(bytes / PAGE).to_le_bytes());
    b[16..20].copy_from_slice(&kind.to_le_bytes());
    b
}

fn descriptor(kind: u32, start: u64, bytes: u64) -> [u8; DESCRIPTOR_SIZE as usize] {
    let mut b = [0u8; DESCRIPTOR_SIZE as usize];
    b[0..4].copy_from_slice(&kind.to_le_bytes());
    b[8..16].copy_from_slice(&start.to_le_bytes());
    b[24..32].copy_from_slice(&(bytes / PAGE).to_le_bytes());
    b[32..40].copy_from_slice(&EFI_MEMORY_WB.to_le_bytes());
    b
}

/// Loads `elf_bytes` into guest memory and builds the M0 handoff.
pub fn load<M: GuestMemory + ?Sized>(
    elf_bytes: &[u8],
    mem: &mut M,
    cfg: &Config,
) -> Result<Entry, LoadError> {
    let l = layout(elf_bytes, cfg)?;
    zero(mem, LOAD_BASE, l.end - LOAD_BASE);

    let mut tables = Tables {
        base: l.tables,
        used: 1,
        pages: l.table_pages,
    };
    let mut ranges = [[0u8; 24]; MAX_RANGES];
    let mut nranges = 0;
    let mut add = |r: [u8; 24]| {
        ranges[nranges] = r;
        nranges += 1;
    };
    for (i, s) in l.image.segments().iter().enumerate() {
        let pa = l.segments[i];
        let data = &elf_bytes[s.file_offset as usize..(s.file_offset + s.file_size) as usize];
        mem.write(pa, data);
        let w = s.flags & bp::PF_W != 0;
        let x = s.flags & bp::PF_X != 0;
        tables.map(mem, s.virt_start, pa, s.page_count(), w, x);
        add(range(pa, s.page_count() * PAGE, bp::KIND_KERNEL));
        let seg = bp::LoadedSegment {
            phys_start: pa,
            virt_start: s.virt_start,
            memory_size: s.memory_size,
            flags: s.flags,
            reserved: 0,
        };
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&seg.phys_start.to_le_bytes());
        b[8..16].copy_from_slice(&seg.virt_start.to_le_bytes());
        b[16..24].copy_from_slice(&seg.memory_size.to_le_bytes());
        b[24..28].copy_from_slice(&seg.flags.to_le_bytes());
        mem.write(l.arena + SEGMENTS_OFFSET + 32 * i as u64, &b);
    }
    tables.map(
        mem,
        bp::HANDOFF_BASE,
        l.arena,
        bp::HANDOFF_MAPPED_SIZE / PAGE,
        true,
        false,
    );
    tables.map(
        mem,
        bp::STACK_TOP - bp::STACK_SIZE,
        l.stack,
        bp::STACK_SIZE / PAGE,
        true,
        false,
    );
    if cfg.protocol == Protocol::M1 {
        tables.map(
            mem,
            M1_PAGE_TABLES_BASE,
            l.tables,
            l.table_pages,
            true,
            false,
        );
    }
    add(range(l.arena, bp::HANDOFF_MAPPED_SIZE, bp::KIND_BOOT_INFO));
    add(range(l.stack, bp::STACK_SIZE, bp::KIND_STACK));
    add(range(l.tables, l.table_pages * PAGE, bp::KIND_PAGE_TABLES));
    add(range(l.transition, PAGE, bp::KIND_TRANSITION));
    for (i, r) in ranges[..nranges].iter().enumerate() {
        mem.write(l.arena + RANGES_OFFSET + 24 * i as u64, r);
    }

    let mut map_len = 0u64;
    for (kind, start, end) in [
        (EFI_CONVENTIONAL, 0, LOAD_BASE),
        (EFI_LOADER_DATA, LOAD_BASE, l.end),
        (EFI_CONVENTIONAL, l.end, cfg.ram_bytes & !(PAGE - 1)),
    ] {
        if end > start {
            let d = descriptor(kind, start, end - start);
            mem.write(l.arena + MAP_OFFSET + map_len, &d);
            map_len += u64::from(DESCRIPTOR_SIZE);
        }
    }

    let mut info = BootInfo::new();
    info.flags = bp::FLAG_EXIT_BOOT_SERVICES
        | if cfg.test_profile {
            bp::FLAG_TEST_PROFILE
        } else {
            0
        };
    info.memory_map_phys = l.arena + MAP_OFFSET;
    info.memory_map_virt = bp::HANDOFF_BASE + MAP_OFFSET;
    info.memory_map_len = map_len;
    info.memory_descriptor_size = DESCRIPTOR_SIZE;
    info.reserved_ranges_phys = l.arena + RANGES_OFFSET;
    info.reserved_ranges_virt = bp::HANDOFF_BASE + RANGES_OFFSET;
    info.reserved_ranges_count = nranges as u32;
    info.kernel_entry_virt = l.image.entry;
    info.load_segments_phys = l.arena + SEGMENTS_OFFSET;
    info.load_segments_virt = bp::HANDOFF_BASE + SEGMENTS_OFFSET;
    info.load_segments_count = l.image.segment_count as u32;
    info.pml4_phys = l.tables;
    info.boot_epoch = cfg.boot_epoch;
    mem.write(l.arena, &encode_boot_info(&info));

    Ok(Entry {
        rip: l.image.entry,
        rsp: bp::STACK_TOP,
        rdi: bp::HANDOFF_BASE,
        cr3: l.tables,
        boot_info_gpa: l.arena,
        end: l.end,
    })
}
