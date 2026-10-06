//! Direct boot of a Linux bzImage: the setup header, the zero page, the e820
//! map, the GDT, the identity page tables and the entry state, checked byte by
//! byte on synthetic images; and the header of a real kernel when one is on
//! the machine (the Proxmox VE installer's, extracted to ~/mut/kernel/bzImage).

use std::collections::HashMap;

use guest_boot::linux::*;
use guest_boot::GuestMemory;

/// Sparse guest memory: untouched bytes read as `fill`; counts writes.
struct Sparse {
    pages: HashMap<u64, Box<[u8; 4096]>>,
    fill: u8,
    writes: usize,
}

impl Sparse {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            fill: 0xEE,
            writes: 0,
        }
    }
    fn byte(&self, gpa: u64) -> u8 {
        self.pages
            .get(&(gpa >> 12))
            .map_or(self.fill, |p| p[(gpa & 0xFFF) as usize])
    }
    fn bytes(&self, gpa: u64, n: usize) -> Vec<u8> {
        (0..n as u64).map(|i| self.byte(gpa + i)).collect()
    }
    fn u64(&self, gpa: u64) -> u64 {
        u64::from_le_bytes(self.bytes(gpa, 8).try_into().unwrap())
    }
    fn u32(&self, gpa: u64) -> u32 {
        u32::from_le_bytes(self.bytes(gpa, 4).try_into().unwrap())
    }
}

impl GuestMemory for Sparse {
    fn write(&mut self, gpa: u64, bytes: &[u8]) {
        self.writes += 1;
        for (i, b) in bytes.iter().enumerate() {
            let a = gpa + i as u64;
            let fill = self.fill;
            self.pages
                .entry(a >> 12)
                .or_insert_with(|| Box::new([fill; 4096]))[(a & 0xFFF) as usize] = *b;
        }
    }
    fn read(&mut self, gpa: u64, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.byte(gpa + i as u64);
        }
    }
}

const MIB: u64 = 1 << 20;

/// A synthetic bzImage: `setup_sects` sectors of setup (+ the boot sector) and a payload.
#[derive(Clone)]
struct Img {
    setup_sects: u8,
    version: u16,
    xloadflags: u16,
    loadflags: u8,
    align: u32,
    relocatable: bool,
    cmdline_size: u32,
    initrd_max: u32,
    pref: u64,
    init_size: u32,
    jump: u8,
    payload: usize,
}

impl Img {
    fn new() -> Self {
        Self {
            setup_sects: 4,
            version: 0x020F,
            xloadflags: 0x7F,
            loadflags: 0x01,
            align: 0x20_0000,
            relocatable: true,
            cmdline_size: 2047,
            initrd_max: 0x7FFF_FFFF,
            pref: 0x100_0000,
            init_size: 0x30_0000,
            jump: 0x6A,
            payload: 0x1234,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let sects = if self.setup_sects == 0 {
            4
        } else {
            self.setup_sects
        };
        let off = (usize::from(sects) + 1) * 512;
        let mut b = vec![0u8; off + self.payload];
        // recognisable filler in the setup area, so a copied header shows where it came from
        for (i, x) in b[..off].iter_mut().enumerate() {
            *x = (i as u8).wrapping_mul(13) ^ 0x5A;
        }
        b[0x1F1] = self.setup_sects;
        b[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        b[0x200] = 0xEB;
        b[0x201] = self.jump;
        b[0x202..0x206].copy_from_slice(b"HdrS");
        b[0x206..0x208].copy_from_slice(&self.version.to_le_bytes());
        b[0x210] = 0;
        b[0x211] = self.loadflags;
        b[0x218..0x220].fill(0);
        b[0x228..0x22C].fill(0);
        b[0x22C..0x230].copy_from_slice(&self.initrd_max.to_le_bytes());
        b[0x230..0x234].copy_from_slice(&self.align.to_le_bytes());
        b[0x234] = u8::from(self.relocatable);
        b[0x236..0x238].copy_from_slice(&self.xloadflags.to_le_bytes());
        b[0x238..0x23C].copy_from_slice(&self.cmdline_size.to_le_bytes());
        b[0x258..0x260].copy_from_slice(&self.pref.to_le_bytes());
        b[0x260..0x264].copy_from_slice(&self.init_size.to_le_bytes());
        for (i, x) in b[off..].iter_mut().enumerate() {
            *x = (i as u8).wrapping_mul(7) ^ 0xA5;
        }
        b
    }
}

fn cfg(ram: u64) -> LinuxConfig<'static> {
    LinuxConfig {
        ram_bytes: ram,
        cmdline: b"console=ttyS0 quiet",
        acpi: None,
        reserved: &[],
    }
}

fn e820(m: &Sparse) -> Vec<(u64, u64, u32)> {
    let n = m.byte(BOOT_PARAMS_GPA + 0x1E8) as u64;
    (0..n)
        .map(|i| {
            let at = BOOT_PARAMS_GPA + 0x2D0 + 20 * i;
            (m.u64(at), m.u64(at + 8), m.u32(at + 16))
        })
        .collect()
}

// ------------------------------------------------------------------ header

#[test]
fn the_setup_header_is_read_field_by_field() {
    let img = Img {
        setup_sects: 7,
        jump: 0x70,
        payload: 999,
        ..Img::new()
    };
    let h = parse_header(&img.bytes()).unwrap();
    assert_eq!(h.version, 0x020F);
    assert_eq!(h.setup_sects, 7);
    assert_eq!(h.payload_offset, 8 * 512);
    assert_eq!(h.header_end, 0x202 + 0x70);
    assert_eq!(h.loadflags, 0x01);
    assert_eq!(h.xloadflags, 0x7F);
    assert_eq!(h.kernel_alignment, 0x20_0000);
    assert!(h.relocatable);
    assert_eq!(h.cmdline_size, 2047);
    assert_eq!(h.initrd_addr_max, 0x7FFF_FFFF);
    assert_eq!(h.pref_address, 0x100_0000);
    assert_eq!(h.init_size, 0x30_0000);
    // setup_sects 0 means 4
    let h = parse_header(
        &Img {
            setup_sects: 0,
            ..Img::new()
        }
        .bytes(),
    )
    .unwrap();
    assert_eq!((h.setup_sects, h.payload_offset), (4, 5 * 512));
    // a non-relocatable image and a 64-bit preferred address
    let img = Img {
        relocatable: false,
        pref: 0x1_2340_0000,
        ..Img::new()
    };
    let h = parse_header(&img.bytes()).unwrap();
    assert!(!h.relocatable);
    assert_eq!(h.pref_address, 0x1_2340_0000);
}

#[test]
fn what_is_not_a_bootable_64_bit_bzimage_is_refused_with_the_reason() {
    let good = Img::new().bytes();
    assert_eq!(
        parse_header(&good[..0x263]),
        Err(LinuxError::NotBzImage),
        "shorter than the header"
    );
    let mut b = good.clone();
    b[0x1FE] = 0x54;
    assert_eq!(parse_header(&b), Err(LinuxError::NotBzImage), "boot flag");
    let mut b = good.clone();
    b[0x1FF] = 0x00;
    assert_eq!(
        parse_header(&b),
        Err(LinuxError::NotBzImage),
        "boot flag high byte"
    );
    let mut b = good.clone();
    b[0x205] = b'Z';
    assert_eq!(parse_header(&b), Err(LinuxError::NotBzImage), "HdrS");
    assert_eq!(
        parse_header(
            &Img {
                version: 0x020B,
                ..Img::new()
            }
            .bytes()
        ),
        Err(LinuxError::ProtocolTooOld)
    );
    assert!(
        parse_header(
            &Img {
                version: 0x020C,
                ..Img::new()
            }
            .bytes()
        )
        .is_ok(),
        "2.12 is enough"
    );
    assert_eq!(
        parse_header(
            &Img {
                xloadflags: 0x7E,
                ..Img::new()
            }
            .bytes()
        ),
        Err(LinuxError::No64BitEntry)
    );
    assert!(parse_header(
        &Img {
            xloadflags: 0x01,
            ..Img::new()
        }
        .bytes()
    )
    .is_ok());
    // the payload must start inside the image
    let short = Img {
        setup_sects: 4,
        payload: 0,
        ..Img::new()
    }
    .bytes();
    assert_eq!(parse_header(&short), Err(LinuxError::Truncated));
    let mut one = Img {
        setup_sects: 4,
        payload: 1,
        ..Img::new()
    }
    .bytes();
    assert!(parse_header(&one).is_ok(), "one byte of payload");
    // the header must end inside the image
    one.truncate(0x264);
    one[0x201] = 0x7F;
    one[0x1F1] = 0;
    assert_eq!(parse_header(&one), Err(LinuxError::Truncated));
}

// -------------------------------------------------------------------- load

#[test]
fn the_kernel_command_line_and_entry_state() {
    let img = Img::new();
    let image = img.bytes();
    let mut m = Sparse::new();
    let e = load_linux(&mut m, &image, &[], &cfg(256 * MIB)).unwrap();
    assert_eq!(e.load_address, 0x100_0000);
    assert_eq!(e.rip, 0x100_0200);
    assert_eq!(e.rsi, BOOT_PARAMS_GPA);
    assert_eq!(e.cr0, CR0_PE | CR0_PG);
    assert_eq!(e.cr0, 0x8000_0001);
    assert_eq!(e.cr3, PAGE_TABLES_GPA);
    assert_eq!(e.cr4, 0x20);
    assert_eq!(e.efer, 0x500);
    assert_eq!((e.gdt_base, e.gdt_limit), (GDT_GPA, 31));
    assert_eq!((e.cs, e.ds), (0x10, 0x18));
    assert_eq!(e.rflags, 2);
    assert_eq!((e.initrd_gpa, e.initrd_len), (0, 0));
    // the payload, exactly, at the load address
    assert_eq!(m.bytes(0x100_0000, img.payload), image[5 * 512..]);
    assert_eq!(
        m.byte(0x100_0000 + img.payload as u64),
        0xEE,
        "nothing beyond it"
    );
    // the command line and its NUL
    assert_eq!(m.bytes(CMDLINE_GPA, 20), b"console=ttyS0 quiet\0");
}

#[test]
fn the_zero_page_is_the_image_header_plus_what_the_loader_fills_in() {
    let img = Img {
        jump: 0x66,
        ..Img::new()
    };
    let image = img.bytes();
    let mut m = Sparse::new();
    let acpi = Acpi {
        rsdp_gpa: 0x7F0_0010,
        region_gpa: 0x7F0_0000,
        region_len: 0x1_0000,
    };
    let initrd: Vec<u8> = (0..5000u32).map(|i| (i * 3) as u8).collect();
    let c = LinuxConfig {
        acpi: Some(acpi),
        ..cfg(256 * MIB)
    };
    let e = load_linux(&mut m, &image, &initrd, &c).unwrap();
    let zp = m.bytes(BOOT_PARAMS_GPA, 4096);
    let end = 0x202 + 0x66;
    let mut want = vec![0u8; 4096];
    want[0x1F1..end].copy_from_slice(&image[0x1F1..end]);
    want[0x210] = 0xFF;
    want[0x211] |= 1;
    want[0x228..0x22C].copy_from_slice(&(CMDLINE_GPA as u32).to_le_bytes());
    want[0x218..0x21C].copy_from_slice(&(e.initrd_gpa as u32).to_le_bytes());
    want[0x21C..0x220].copy_from_slice(&5000u32.to_le_bytes());
    want[0x070..0x078].copy_from_slice(&0x7F0_0010u64.to_le_bytes());
    want[0x1E8] = e.e820_entries;
    for (i, (s, l, k)) in e820(&m).into_iter().enumerate() {
        let at = 0x2D0 + 20 * i;
        want[at..at + 8].copy_from_slice(&s.to_le_bytes());
        want[at + 8..at + 16].copy_from_slice(&l.to_le_bytes());
        want[at + 16..at + 20].copy_from_slice(&k.to_le_bytes());
    }
    assert_eq!(zp, want, "every byte of the zero page");
    // the header ends where the jump says: the byte after it is the loader's (zero)
    assert_eq!(zp[end], 0);
    assert_ne!(image[end], 0);
    // loadflags keeps the image's other bits
    let img = Img {
        loadflags: 0xC0,
        ..Img::new()
    };
    let mut m = Sparse::new();
    load_linux(&mut m, &img.bytes(), &[], &cfg(256 * MIB)).unwrap();
    assert_eq!(m.byte(BOOT_PARAMS_GPA + 0x211), 0xC1);
}

#[test]
fn the_e820_map_describes_the_platform() {
    let mut m = Sparse::new();
    let e = load_linux(&mut m, &Img::new().bytes(), &[], &cfg(256 * MIB)).unwrap();
    assert_eq!(
        e820(&m),
        [
            (0, 0x9_FC00, E820_RAM),
            (0x9_FC00, 0x400, E820_RESERVED),
            (0xF_0000, 0x1_0000, E820_RESERVED),
            (0x10_0000, 256 * MIB - 0x10_0000, E820_RAM),
        ]
    );
    assert_eq!(e.e820_entries, 4);
    assert_eq!([E820_RAM, E820_RESERVED, E820_ACPI], [1, 2, 3]);
    // with ACPI tables in the middle of the low RAM, and RAM above the 32-bit hole
    let mut m = Sparse::new();
    let acpi = Acpi {
        rsdp_gpa: 0x8000_0000,
        region_gpa: 0x8000_0000,
        region_len: 0x2_0000,
    };
    let c = LinuxConfig {
        acpi: Some(acpi),
        ..cfg(4 << 30)
    };
    let e = load_linux(&mut m, &Img::new().bytes(), &[], &c).unwrap();
    assert_eq!(
        e820(&m),
        [
            (0, 0x9_FC00, E820_RAM),
            (0x9_FC00, 0x400, E820_RESERVED),
            (0xF_0000, 0x1_0000, E820_RESERVED),
            (0x10_0000, 0x8000_0000 - 0x10_0000, E820_RAM),
            (0x8000_0000, 0x2_0000, E820_ACPI),
            (0x8002_0000, 0xB000_0000 - 0x8002_0000, E820_RAM),
            (1 << 32, (4 << 30) - 0xB000_0000, E820_RAM),
        ]
    );
    assert_eq!(e.e820_entries, 7);
    // ACPI tables at the very end of the low RAM leave no empty RAM entry after them
    let mut m = Sparse::new();
    let acpi = Acpi {
        rsdp_gpa: 255 * MIB,
        region_gpa: 255 * MIB,
        region_len: MIB,
    };
    let c = LinuxConfig {
        acpi: Some(acpi),
        ..cfg(256 * MIB)
    };
    load_linux(&mut m, &Img::new().bytes(), &[], &c).unwrap();
    assert_eq!(e820(&m).last(), Some(&(255 * MIB, MIB, E820_ACPI)));
    assert_eq!(e820(&m).len(), 5);
    // RAM ending exactly at the hole: no high entry
    let mut m = Sparse::new();
    load_linux(&mut m, &Img::new().bytes(), &[], &cfg(LOW_RAM_LIMIT)).unwrap();
    assert_eq!(e820(&m).last(), Some(&(MIB, LOW_RAM_LIMIT - MIB, E820_RAM)));
}

#[test]
fn the_gdt_has_the_boot_protocol_selectors() {
    let mut m = Sparse::new();
    load_linux(&mut m, &Img::new().bytes(), &[], &cfg(64 * MIB)).unwrap();
    assert_eq!(m.u64(GDT_GPA), 0);
    assert_eq!(m.u64(GDT_GPA + 8), 0);
    assert_eq!(
        m.u64(GDT_GPA + 0x10),
        0x00AF_9B00_0000_FFFF,
        "__BOOT_CS: 64-bit code"
    );
    assert_eq!(
        m.u64(GDT_GPA + 0x18),
        0x00CF_9300_0000_FFFF,
        "__BOOT_DS: flat data"
    );
    assert_eq!(m.byte(GDT_GPA + 0x20), 0xEE, "four entries only");
}

#[test]
fn the_low_4_gib_are_identity_mapped_with_2_mib_pages() {
    let mut m = Sparse::new();
    let e = load_linux(&mut m, &Img::new().bytes(), &[], &cfg(64 * MIB)).unwrap();
    let pml4 = e.cr3;
    assert_eq!(m.u64(pml4), (pml4 + 0x1000) | 3);
    for i in 1..512 {
        assert_eq!(m.u64(pml4 + 8 * i), 0, "PML4 entry {i}");
    }
    let pdpt = pml4 + 0x1000;
    for i in 0..4 {
        assert_eq!(
            m.u64(pdpt + 8 * i),
            (pdpt + 0x1000 * (i + 1)) | 3,
            "PDPT entry {i}"
        );
    }
    for i in 4..512 {
        assert_eq!(m.u64(pdpt + 8 * i), 0);
    }
    // walk some addresses through the tables
    for va in [
        0u64,
        0x1F_FFFF,
        0x20_0000,
        0x100_0200,
        0x7FFF_FFFF,
        0xB000_0000,
        0xFEE0_0000,
        0xFFFF_FFFF,
    ] {
        let l4 = m.u64(pml4 + 8 * ((va >> 39) & 511));
        let l3 = m.u64((l4 & !0xFFF) + 8 * ((va >> 30) & 511));
        let l2 = m.u64((l3 & !0xFFF) + 8 * ((va >> 21) & 511));
        assert_eq!(l2 & 0xFFF, 0x83, "present, writable, 2 MiB page at {va:#x}");
        assert_eq!((l2 & !0xFFF) | (va & 0x1F_FFFF), va, "identity at {va:#x}");
    }
    assert_eq!(
        e.cr3 + 6 * 0x1000,
        0xF000,
        "six table pages below the command line"
    );
}

#[test]
fn the_load_address_honours_relocation_and_alignment() {
    let cases: [(Img, Result<u64, LinuxError>); 9] = [
        (Img::new(), Ok(0x100_0000)),
        (
            Img {
                pref: 0x8_0000,
                ..Img::new()
            },
            Ok(0x20_0000),
        ),
        (
            Img {
                pref: 0x8_0000,
                align: 0x1000,
                ..Img::new()
            },
            Ok(0x10_0000),
        ),
        (
            Img {
                pref: 0x110_0000,
                ..Img::new()
            },
            Ok(0x120_0000),
        ),
        (
            Img {
                pref: 0x110_0000,
                align: 0,
                ..Img::new()
            },
            Ok(0x110_0000),
        ),
        (
            Img {
                relocatable: false,
                pref: 0x140_0000,
                ..Img::new()
            },
            Ok(0x140_0000),
        ),
        (
            Img {
                relocatable: false,
                pref: 0x8_0000,
                align: 0x1000,
                ..Img::new()
            },
            Err(LinuxError::NotLoadable),
        ),
        (
            Img {
                relocatable: false,
                pref: 0x110_0000,
                ..Img::new()
            },
            Err(LinuxError::NotLoadable),
        ),
        (
            Img {
                align: 0x30_0000,
                ..Img::new()
            },
            Err(LinuxError::NotLoadable),
        ),
    ];
    for (i, (img, want)) in cases.into_iter().enumerate() {
        let mut m = Sparse::new();
        let got = load_linux(&mut m, &img.bytes(), &[], &cfg(256 * MIB)).map(|e| e.load_address);
        assert_eq!(got, want, "case {i}");
    }
}

#[test]
fn the_kernel_needs_init_size_or_its_payload_whichever_is_larger() {
    // init_size 3 MiB at 16 MiB: RAM of 19 MiB is just enough, one byte less is not
    let fits = |ram: u64, img: &Img| {
        let mut m = Sparse::new();
        load_linux(&mut m, &img.bytes(), &[], &cfg(ram)).map(|e| e.load_address)
    };
    let img = Img::new();
    assert_eq!(fits(0x130_0000, &img), Ok(0x100_0000));
    assert_eq!(fits(0x12F_FFFF, &img), Err(LinuxError::GuestTooSmall));
    // a payload larger than init_size counts instead
    let img = Img {
        init_size: 0x100,
        payload: 0x2_0000,
        ..Img::new()
    };
    assert_eq!(fits(0x102_0000, &img), Ok(0x100_0000));
    assert_eq!(fits(0x101_FFFF, &img), Err(LinuxError::GuestTooSmall));
}

#[test]
fn the_initrd_goes_to_the_top_of_the_low_ram_below_its_limit() {
    let initrd = vec![0x42u8; 0x1_2345];
    let mut m = Sparse::new();
    let e = load_linux(&mut m, &Img::new().bytes(), &initrd, &cfg(256 * MIB)).unwrap();
    assert_eq!(e.initrd_gpa, (256 * MIB - 0x1_2345) & !0xFFF);
    assert_eq!(e.initrd_len, 0x1_2345);
    assert_eq!(m.bytes(e.initrd_gpa, 0x1_2345), initrd);
    assert_eq!(m.u32(BOOT_PARAMS_GPA + 0x218), e.initrd_gpa as u32);
    assert_eq!(m.u32(BOOT_PARAMS_GPA + 0x21C), 0x1_2345);
    // the kernel's initrd_addr_max lowers the ceiling (it is the last usable byte)
    let img = Img {
        initrd_max: 0x37FF_FFFF,
        ..Img::new()
    };
    let e = load_linux(&mut Sparse::new(), &img.bytes(), &initrd, &cfg(2 << 30)).unwrap();
    assert_eq!(e.initrd_gpa, (0x3800_0000 - 0x1_2345) & !0xFFF);
    // and the 32-bit hole does with more RAM than fits below it
    let e = load_linux(
        &mut Sparse::new(),
        &Img {
            initrd_max: u32::MAX,
            ..Img::new()
        }
        .bytes(),
        &initrd,
        &cfg(8 << 30),
    )
    .unwrap();
    assert_eq!(e.initrd_gpa, (LOW_RAM_LIMIT - 0x1_2345) & !0xFFF);
    // an initrd that would reach into the kernel does not fit
    let img = Img::new();
    let just = 0x130_0000 + 0x1000;
    let e = load_linux(&mut Sparse::new(), &img.bytes(), &[1u8; 0x1000], &cfg(just)).unwrap();
    assert_eq!(e.initrd_gpa, 0x130_0000, "exactly after the kernel");
    assert_eq!(
        load_linux(&mut Sparse::new(), &img.bytes(), &[1u8; 0x1001], &cfg(just)),
        Err(LinuxError::GuestTooSmall)
    );
    assert_eq!(
        load_linux(
            &mut Sparse::new(),
            &img.bytes(),
            &[1u8; 0x1000],
            &cfg(just - 1)
        ),
        Err(LinuxError::GuestTooSmall),
        "page alignment pushes it down into the kernel"
    );
    let big = vec![0u8; 0x3000];
    let tiny = Img {
        initrd_max: 0x1FFF,
        ..Img::new()
    };
    assert_eq!(
        load_linux(&mut Sparse::new(), &tiny.bytes(), &big, &cfg(256 * MIB)),
        Err(LinuxError::GuestTooSmall),
        "larger than everything below its limit"
    );
}

#[test]
fn the_command_line_limit_is_the_kernels_and_the_low_memorys() {
    let img = Img {
        cmdline_size: 10,
        ..Img::new()
    };
    let ok = LinuxConfig {
        cmdline: b"0123456789",
        ..cfg(64 * MIB)
    };
    let mut m = Sparse::new();
    assert!(load_linux(&mut m, &img.bytes(), &[], &ok).is_ok());
    assert_eq!(m.bytes(CMDLINE_GPA, 11), b"0123456789\0");
    let long = LinuxConfig {
        cmdline: b"0123456789A",
        ..cfg(64 * MIB)
    };
    assert_eq!(
        load_linux(&mut Sparse::new(), &img.bytes(), &[], &long),
        Err(LinuxError::CmdlineTooLong)
    );
    // a kernel that accepts more than fits below the EBDA
    let img = Img {
        cmdline_size: u32::MAX,
        ..Img::new()
    };
    let fits = vec![b'x'; (LOW_RAM_END - CMDLINE_GPA - 1) as usize];
    let c = LinuxConfig {
        cmdline: &fits,
        ..cfg(64 * MIB)
    };
    assert!(load_linux(&mut Sparse::new(), &img.bytes(), &[], &c).is_ok());
    let over = vec![b'x'; (LOW_RAM_END - CMDLINE_GPA) as usize];
    let c = LinuxConfig {
        cmdline: &over,
        ..cfg(64 * MIB)
    };
    assert_eq!(
        load_linux(&mut Sparse::new(), &img.bytes(), &[], &c),
        Err(LinuxError::CmdlineTooLong)
    );
    // an empty command line is a single NUL
    let mut m = Sparse::new();
    load_linux(
        &mut m,
        &Img::new().bytes(),
        &[],
        &LinuxConfig {
            cmdline: b"",
            ..cfg(64 * MIB)
        },
    )
    .unwrap();
    assert_eq!(m.byte(CMDLINE_GPA), 0);
    assert_eq!(m.byte(CMDLINE_GPA + 1), 0xEE);
}

#[test]
fn a_bad_acpi_region_is_refused() {
    let initrd = [7u8; 0x2000];
    let try_acpi = |a: Acpi| {
        let c = LinuxConfig {
            acpi: Some(a),
            ..cfg(256 * MIB)
        };
        load_linux(&mut Sparse::new(), &Img::new().bytes(), &initrd, &c).map(|_| ())
    };
    let initrd_at = (256 * MIB - 0x2000) & !0xFFF;
    let good = Acpi {
        rsdp_gpa: 0x80_0000,
        region_gpa: 0x80_0000,
        region_len: 0x1000,
    };
    assert_eq!(try_acpi(good), Ok(()));
    for (name, a) in [
        (
            "empty",
            Acpi {
                region_len: 0,
                ..good
            },
        ),
        (
            "below 1 MiB",
            Acpi {
                rsdp_gpa: 0xF_0000,
                region_gpa: 0xF_0000,
                region_len: 0x100,
            },
        ),
        (
            "straddles 1 MiB",
            Acpi {
                rsdp_gpa: 0x10_0000,
                region_gpa: 0xF_F000,
                region_len: 0x2000,
            },
        ),
        (
            "beyond the RAM",
            Acpi {
                rsdp_gpa: 256 * MIB,
                region_gpa: 256 * MIB,
                region_len: 0x1000,
            },
        ),
        (
            "overflows",
            Acpi {
                rsdp_gpa: 0x80_0000,
                region_gpa: 0x80_0000,
                region_len: u64::MAX,
            },
        ),
        (
            "RSDP before the region",
            Acpi {
                rsdp_gpa: 0x7F_FFFF,
                ..good
            },
        ),
        (
            "RSDP runs past the end",
            Acpi {
                rsdp_gpa: 0x80_0FED,
                ..good
            },
        ),
        (
            "over the kernel",
            Acpi {
                rsdp_gpa: 0x12F_F000,
                region_gpa: 0x12F_F000,
                region_len: 0x1000,
            },
        ),
        (
            "over the initrd",
            Acpi {
                rsdp_gpa: initrd_at,
                region_gpa: initrd_at,
                region_len: 0x1000,
            },
        ),
    ] {
        assert_eq!(try_acpi(a), Err(LinuxError::BadAcpiRegion), "{name}");
    }
    // the edges that are allowed
    assert_eq!(
        try_acpi(Acpi {
            rsdp_gpa: 0x80_0FEC,
            ..good
        }),
        Ok(()),
        "RSDP ends at the region's end"
    );
    assert_eq!(
        try_acpi(Acpi {
            rsdp_gpa: 0x130_0000,
            region_gpa: 0x130_0000,
            region_len: 0x1000
        }),
        Ok(()),
        "right after the kernel"
    );
    assert_eq!(
        try_acpi(Acpi {
            rsdp_gpa: 0x10_0000,
            region_gpa: 0x10_0000,
            region_len: 0x1000
        }),
        Ok(()),
        "at 1 MiB"
    );
    assert_eq!(
        try_acpi(Acpi {
            rsdp_gpa: initrd_at - 0x1000,
            region_gpa: initrd_at - 0x1000,
            region_len: 0x1000
        }),
        Ok(()),
        "right below the initrd"
    );
}

#[test]
fn an_error_writes_nothing() {
    for (img, ram) in [
        (
            Img {
                version: 0x0200,
                ..Img::new()
            },
            64 * MIB,
        ),
        (
            Img {
                xloadflags: 0,
                ..Img::new()
            },
            64 * MIB,
        ),
        (Img::new(), MIB),
        (
            Img {
                relocatable: false,
                pref: 0x1234,
                ..Img::new()
            },
            64 * MIB,
        ),
    ] {
        let mut m = Sparse::new();
        assert!(load_linux(&mut m, &img.bytes(), &[1, 2, 3], &cfg(ram)).is_err());
        assert_eq!(m.writes, 0);
    }
}

#[test]
fn the_header_of_a_real_kernel_when_one_is_available() {
    let path = std::env::var("NANOX_TEST_BZIMAGE")
        .unwrap_or_else(|_| "/home/holod/mut/kernel/bzImage".into());
    let Ok(image) = std::fs::read(&path) else {
        eprintln!("no kernel at {path}: skipped");
        return;
    };
    let h = parse_header(&image).unwrap();
    assert!(h.version >= MIN_VERSION);
    assert_ne!(h.xloadflags & 1, 0);
    assert!(h.relocatable);
    let mut m = Sparse::new();
    let initrd = [0u8; 4096];
    let c = LinuxConfig {
        cmdline: b"console=ttyS0 earlyprintk=serial",
        ..cfg(1 << 30)
    };
    let e = load_linux(&mut m, &image, &initrd, &c).unwrap();
    assert_eq!(e.rip, e.load_address + 0x200);
    assert_eq!(e.load_address % u64::from(h.kernel_alignment), 0);
    assert_eq!(
        m.bytes(e.load_address, 64),
        image[h.payload_offset..h.payload_offset + 64]
    );
    assert_eq!(m.bytes(BOOT_PARAMS_GPA + 0x202, 4), b"HdrS");
}

// ---------------------------------------------- edges found by the mutants

#[test]
fn the_fixed_low_memory_layout() {
    assert_eq!(
        [GDT_GPA, BOOT_PARAMS_GPA, PAGE_TABLES_GPA, CMDLINE_GPA],
        [0x500, 0x7000, 0x9000, 0x2_0000]
    );
    assert_eq!(
        [LOW_RAM_END, LOW_RAM_LIMIT, HIGH_RAM_BASE, MIN_LOAD_ADDRESS],
        [0x9_FC00, 0xB000_0000, 1 << 32, MIB]
    );
    // nothing written right after the zero page or after the six table pages
    let mut m = Sparse::new();
    load_linux(&mut m, &Img::new().bytes(), &[], &cfg(64 * MIB)).unwrap();
    assert_eq!(m.byte(BOOT_PARAMS_GPA + 4096), 0xEE);
    assert_eq!(m.byte(BOOT_PARAMS_GPA - 1), 0xEE);
    assert_eq!(m.byte(PAGE_TABLES_GPA + 6 * 4096), 0xEE);
    assert_eq!(m.byte(PAGE_TABLES_GPA - 1), 0xEE);
}

#[test]
fn load_address_edges() {
    let at = |img: Img, ram: u64| {
        load_linux(&mut Sparse::new(), &img.bytes(), &[], &cfg(ram)).map(|e| e.load_address)
    };
    // no alignment requirement: the preferred address as it is, even an odd one
    assert_eq!(
        at(
            Img {
                pref: 0x110_0001,
                align: 0,
                ..Img::new()
            },
            256 * MIB
        ),
        Ok(0x110_0001)
    );
    // a fixed kernel exactly at 1 MiB
    assert_eq!(
        at(
            Img {
                relocatable: false,
                pref: MIB,
                align: 0x1000,
                ..Img::new()
            },
            256 * MIB
        ),
        Ok(MIB)
    );
    // a relocatable kernel that prefers an address above 4 GiB does not get one: only the low
    // 4 GiB are mapped at the entry
    assert_eq!(
        at(
            Img {
                pref: 0x1_0000_0000,
                ..Img::new()
            },
            8 << 30
        ),
        Err(LinuxError::GuestTooSmall)
    );
}

#[test]
fn initrd_ceiling_edges() {
    let place = |max: u32, len: usize, ram: u64, init_size: u32| {
        let img = Img {
            initrd_max: max,
            init_size,
            ..Img::new()
        };
        load_linux(&mut Sparse::new(), &img.bytes(), &vec![9u8; len], &cfg(ram))
            .map(|e| e.initrd_gpa)
    };
    // initrd_addr_max is the last usable byte
    assert_eq!(
        place(0x37FF_FFFF, 0x1000, 2 << 30, 0x30_0000),
        Ok(0x37FF_F000)
    );
    assert_eq!(
        place(0x37FF_FFFE, 0x1000, 2 << 30, 0x30_0000),
        Ok(0x37FF_E000)
    );
    // a place that only exists unaligned is no place: the kernel ends at 0x12F_E400
    assert_eq!(
        place(u32::MAX, 0x1000, 0x12F_E800 + 0x1000, 0x2F_E400),
        Err(LinuxError::GuestTooSmall)
    );
    assert_eq!(
        place(u32::MAX, 0x1000, 0x12F_F000 + 0x1000, 0x2F_E400),
        Ok(0x12F_F000)
    );
}

#[test]
fn a_one_byte_ram_entry_is_still_an_entry() {
    let mut m = Sparse::new();
    let acpi = Acpi {
        rsdp_gpa: 255 * MIB,
        region_gpa: 255 * MIB,
        region_len: MIB - 1,
    };
    let c = LinuxConfig {
        acpi: Some(acpi),
        ..cfg(256 * MIB)
    };
    load_linux(&mut m, &Img::new().bytes(), &[], &c).unwrap();
    assert_eq!(e820(&m).last(), Some(&(256 * MIB - 1, 1, E820_RAM)));
}

// ------------------------------------------------------------ reserved ranges

#[test]
fn reserved_device_ranges_go_into_the_e820_map_in_order() {
    let ecam = (0xB000_0000u64, 0x1000_0000u64);
    let mut m = Sparse::new();
    let c = LinuxConfig {
        reserved: &[ecam, (0xFEC0_0000, 0x1000)],
        ..cfg(8 << 30)
    };
    let e = load_linux(&mut m, &Img::new().bytes(), &[], &c).unwrap();
    assert_eq!(
        e820(&m),
        [
            (0, 0x9_FC00, E820_RAM),
            (0x9_FC00, 0x400, E820_RESERVED),
            (0xF_0000, 0x1_0000, E820_RESERVED),
            (MIB, LOW_RAM_LIMIT - MIB, E820_RAM),
            (0xB000_0000, 0x1000_0000, E820_RESERVED),
            (0xFEC0_0000, 0x1000, E820_RESERVED),
            (1 << 32, (8 << 30) - LOW_RAM_LIMIT, E820_RAM),
        ]
    );
    assert_eq!(e.e820_entries, 7);
    // as many as allowed, back to back, the last one ending exactly at 4 GiB
    let four = [
        (0xB000_0000, 0x1000),
        (0xB000_1000, 0x1000),
        (0xC000_0000, 0x10),
        (0xFFFF_F000, 0x1000),
    ];
    let mut m = Sparse::new();
    assert!(load_linux(
        &mut m,
        &Img::new().bytes(),
        &[],
        &LinuxConfig {
            reserved: &four,
            ..cfg(256 * MIB)
        }
    )
    .is_ok());
    assert_eq!(e820(&m).len(), 4 + 4);
    assert_eq!(MAX_RESERVED, 4);
}

#[test]
fn bad_reserved_ranges_are_refused() {
    let five = [
        (0xB000_0000, 1),
        (0xB000_0001, 1),
        (0xB000_0002, 1),
        (0xB000_0003, 1),
        (0xB000_0004, 1),
    ];
    for (name, r) in [
        ("empty", vec![(0xB000_0000u64, 0u64)]),
        ("in the low RAM", vec![(LOW_RAM_LIMIT - 0x1000, 0x2000)]),
        ("below the RAM limit", vec![(0x8000_0000, 0x1000)]),
        ("beyond 4 GiB", vec![(0xFFFF_F000, 0x1001)]),
        ("at 4 GiB", vec![(1 << 32, 0x1000)]),
        ("overflows", vec![(0xB000_0000, u64::MAX)]),
        (
            "out of order",
            vec![(0xC000_0000, 0x1000), (0xB000_0000, 0x1000)],
        ),
        (
            "overlapping",
            vec![(0xB000_0000, 0x2000), (0xB000_1000, 0x1000)],
        ),
        ("too many", five.to_vec()),
    ] {
        let c = LinuxConfig {
            reserved: &r,
            ..cfg(256 * MIB)
        };
        let mut m = Sparse::new();
        assert_eq!(
            load_linux(&mut m, &Img::new().bytes(), &[], &c),
            Err(LinuxError::BadReserved),
            "{name}"
        );
        assert_eq!(m.writes, 0, "{name}: nothing written");
    }
}
