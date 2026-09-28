//! The handoff is checked with boot-protocol's own validators (what the M0
//! kernel runs at entry), reading every buffer through an independent walk
//! of the guest page tables the loader built.

use boot_protocol::{self as bp, BootInfo};
use guest_boot::{encode_boot_info, load, Config, GuestMemory, LoadError, LOAD_BASE};

const VALID: &[u8] = include_bytes!("../../../tests/fixtures/valid-minimal.elf");
const TRUNCATED: &[u8] = include_bytes!("../../../tests/fixtures/truncated-header.elf");
const RAM: u64 = 4 << 20;

struct Ram(Vec<u8>);

impl Ram {
    fn new(fill: u8) -> Self {
        Self(vec![fill; RAM as usize])
    }
}

impl GuestMemory for Ram {
    fn write(&mut self, gpa: u64, bytes: &[u8]) {
        let a = gpa as usize;
        self.0[a..a + bytes.len()].copy_from_slice(bytes);
    }
    fn read(&mut self, gpa: u64, out: &mut [u8]) {
        let a = gpa as usize;
        out.copy_from_slice(&self.0[a..a + out.len()]);
    }
}

fn u64_at(ram: &Ram, pa: u64) -> u64 {
    u64::from_le_bytes(ram.0[pa as usize..pa as usize + 8].try_into().unwrap())
}

/// (physical address, writable, no-execute) through the 4-level tables.
fn walk(ram: &Ram, cr3: u64, va: u64) -> Option<(u64, bool, bool)> {
    let mut table = cr3;
    let mut w = true;
    let mut nx = false;
    for shift in [39, 30, 21, 12] {
        let e = u64_at(ram, table + 8 * ((va >> shift) & 511));
        if e & 1 == 0 {
            return None;
        }
        assert_eq!(e & (1 << 7), 0, "no large pages expected");
        w &= e & 2 != 0;
        nx |= e >> 63 != 0;
        table = e & 0x000F_FFFF_FFFF_F000;
    }
    Some((table | (va & 0xFFF), w, nx))
}

fn read_virt(ram: &Ram, cr3: u64, va: u64, len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let (pa, _, _) = walk(ram, cr3, va + i).expect("mapped");
            ram.0[pa as usize]
        })
        .collect()
}

fn decode(b: &[u8]) -> BootInfo {
    let u16_ = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
    let u32_ = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_ = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    BootInfo {
        magic: b[0..8].try_into().unwrap(),
        major: u16_(8),
        minor: u16_(10),
        header_size: u32_(12),
        total_size: u64_(16),
        flags: u64_(24),
        memory_map_phys: u64_(32),
        memory_map_virt: u64_(40),
        memory_map_len: u64_(48),
        memory_descriptor_size: u32_(56),
        memory_descriptor_version: u32_(60),
        reserved_ranges_phys: u64_(64),
        reserved_ranges_virt: u64_(72),
        reserved_ranges_count: u32_(80),
        reserved_ranges_stride: u32_(84),
        rsdp_phys: u64_(88),
        kernel_entry_virt: u64_(96),
        load_segments_phys: u64_(104),
        load_segments_virt: u64_(112),
        load_segments_count: u32_(120),
        load_segments_stride: u32_(124),
        pml4_phys: u64_(128),
        stack_top_virt: u64_(136),
        serial_io_port: u16_(144),
        reserved0: b[146..152].try_into().unwrap(),
        boot_epoch: u64_(152),
    }
}

fn cfg() -> Config {
    Config {
        ram_bytes: RAM,
        test_profile: true,
        boot_epoch: 7,
    }
}

#[test]
fn handoff_passes_the_kernels_own_validators() {
    let mut ram = Ram::new(0xA5);
    let e = load(VALID, &mut ram, &cfg()).unwrap();
    let image = bp::elf::parse(VALID).unwrap();
    assert_eq!(
        (e.rip, e.rsp, e.rdi),
        (image.entry, bp::STACK_TOP, bp::HANDOFF_BASE)
    );

    let info = decode(&read_virt(&ram, e.cr3, bp::HANDOFF_BASE, 160));
    assert_eq!(info.validate_header_at(bp::HANDOFF_BASE), Ok(()));
    let map = read_virt(&ram, e.cr3, info.memory_map_virt, info.memory_map_len);
    let ranges = read_virt(
        &ram,
        e.cr3,
        info.reserved_ranges_virt,
        u64::from(info.reserved_ranges_count) * 24,
    );
    let segments = read_virt(
        &ram,
        e.cr3,
        info.load_segments_virt,
        u64::from(info.load_segments_count) * 32,
    );
    assert_eq!(info.validate_buffers(&map, &ranges, &segments), Ok(()));
    // The kernel compares CR3 with pml4_phys.
    assert_eq!(info.pml4_phys, e.cr3);
    assert_eq!(
        info.flags,
        bp::FLAG_EXIT_BOOT_SERVICES | bp::FLAG_TEST_PROFILE
    );
    assert_eq!(info.boot_epoch, 7);
    assert_eq!(info.kernel_entry_virt, image.entry);
    // Physical and virtual views of the header agree.
    assert_eq!(
        e.boot_info_gpa,
        walk(&ram, e.cr3, bp::HANDOFF_BASE).unwrap().0
    );
    // The memory map covers guest RAM without holes.
    let mut next = 0;
    for d in map.chunks_exact(48) {
        let start = u64::from_le_bytes(d[8..16].try_into().unwrap());
        let pages = u64::from_le_bytes(d[24..32].try_into().unwrap());
        assert_eq!(start, next);
        next = start + pages * 4096;
    }
    assert_eq!(next, RAM);
}

#[test]
fn segments_are_copied_bss_is_zero_and_mappings_are_w_xor_x() {
    let mut ram = Ram::new(0xA5);
    let e = load(VALID, &mut ram, &cfg()).unwrap();
    let image = bp::elf::parse(VALID).unwrap();
    for s in image.segments() {
        let got = read_virt(&ram, e.cr3, s.virt_start, s.page_count() * 4096);
        let file = &VALID[s.file_offset as usize..(s.file_offset + s.file_size) as usize];
        assert_eq!(&got[..file.len()], file);
        assert!(
            got[file.len()..].iter().all(|&b| b == 0),
            "BSS and page tail zero"
        );
        for p in 0..s.page_count() {
            let (_, w, nx) = walk(&ram, e.cr3, s.virt_start + p * 4096).unwrap();
            assert_eq!(w, s.flags & bp::PF_W != 0);
            assert_eq!(nx, s.flags & bp::PF_X == 0);
        }
    }
    for (va, len) in [
        (bp::HANDOFF_BASE, bp::HANDOFF_MAPPED_SIZE),
        (bp::STACK_TOP - bp::STACK_SIZE, bp::STACK_SIZE),
    ] {
        for p in (0..len).step_by(4096) {
            let (_, w, nx) = walk(&ram, e.cr3, va + p).unwrap();
            assert!(w && nx, "{:#x}", va + p);
        }
        assert!(walk(&ram, e.cr3, va + len).is_none(), "nothing beyond");
    }
    // The guard page below the stack stays unmapped, as in the loader.
    assert!(walk(&ram, e.cr3, bp::STACK_TOP - bp::STACK_SIZE - 4096).is_none());
    // Nothing is identity-mapped: the kernel gets no direct map in M0.
    assert!(walk(&ram, e.cr3, LOAD_BASE).is_none());
}

#[test]
fn errors_leave_guest_memory_untouched() {
    for (elf, ram_bytes, want) in [
        (TRUNCATED, RAM, LoadError::Elf(bp::elf::ElfError::Truncated)),
        (VALID, LOAD_BASE + 64 * 1024, LoadError::GuestTooSmall),
    ] {
        let mut ram = Ram::new(0xA5);
        let c = Config { ram_bytes, ..cfg() };
        assert_eq!(load(elf, &mut ram, &c), Err(want));
        assert!(ram.0.iter().all(|&b| b == 0xA5));
    }
}

#[test]
fn smallest_guest_that_fits_is_accepted() {
    let mut ram = Ram::new(0);
    let e = load(VALID, &mut ram, &cfg()).unwrap();
    let c = Config {
        ram_bytes: e.end,
        ..cfg()
    };
    assert!(load(VALID, &mut Ram::new(0), &c).is_ok());
    let c = Config {
        ram_bytes: e.end - 1,
        ..cfg()
    };
    assert_eq!(
        load(VALID, &mut Ram::new(0), &c),
        Err(LoadError::GuestTooSmall)
    );
}

#[test]
fn boot_info_encoding_matches_the_repr_c_layout() {
    let mut i = BootInfo::new();
    // Distinct values in every field.
    i.flags = 0x0102_0304_0506_0708;
    i.memory_map_phys = 0x1111;
    i.memory_map_virt = 0x2222;
    i.memory_map_len = 0x3333;
    i.memory_descriptor_size = 0x44;
    i.memory_descriptor_version = 0x55;
    i.reserved_ranges_phys = 0x6666;
    i.reserved_ranges_virt = 0x7777;
    i.reserved_ranges_count = 0x88;
    i.reserved_ranges_stride = 0x99;
    i.rsdp_phys = 0xAAAA;
    i.kernel_entry_virt = 0xBBBB;
    i.load_segments_phys = 0xCCCC;
    i.load_segments_virt = 0xDDDD;
    i.load_segments_count = 0xEE;
    i.load_segments_stride = 0xFF;
    i.pml4_phys = 0x1_0000;
    i.stack_top_virt = 0x2_0000;
    i.serial_io_port = 0x3F8;
    i.reserved0 = [1, 2, 3, 4, 5, 6];
    i.boot_epoch = 0x0A0B_0C0D;
    // SAFETY: BootInfo is repr(C), 160 bytes, every field an integer or
    // byte array and no padding (reserved0 fills the only gap).
    let raw: [u8; 160] = unsafe { core::mem::transmute(i) };
    assert_eq!(encode_boot_info(&i), raw);
}
