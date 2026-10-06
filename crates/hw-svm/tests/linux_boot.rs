//! The pieces of a Linux boot under the NANOX VMM put together on the scripted
//! processor: the ACPI tables of the platform (`vmm_devices::acpi`), a bzImage
//! loaded with them by `guest_boot::linux`, the entry state in the VMCB
//! (`Vmcb::setup_linux_boot`) and the vCPU loop on the whole `Machine`. The
//! "kernel" is a script, not x86 code: it prints to COM1 and powers off
//! through ACPI, as a Linux guest does at the end of its run.

mod common;

use common::*;
use guest_boot::linux::{self, Acpi, LinuxConfig};
use guest_boot::GuestMemory;
use hw_svm::platform_vm::{Host, InputHost, PlatformVcpu};
use hw_svm::vmcb::{save, Vmcb};
use hw_svm::vmm::Verdict;
use vmm_devices::acpi::{self as tables, Platform};
use vmm_devices::acpi_pm::{PM1_CONTROL, SLEEP_S5};
use vmm_devices::machine::{Machine, ECAM_BASE, ECAM_SIZE};
use vmm_devices::virtio::GuestMemory as Dma;
use vmm_devices::virtio_blk::{BlockBackend, SECTOR};
use vmm_devices::virtio_console::ConsoleBackend;
use vmm_devices::virtio_gpu::{Rect, Scanout};
use vmm_devices::virtio_net::{NetBackend, MAX_FRAME};

/// A host whose devices the scripted kernel never uses: any request is a failure of the test.
#[derive(Default)]
struct Idle {
    polls: u32,
}

impl Dma for Idle {
    fn read(&self, gpa: u64, _: &mut [u8]) -> bool {
        panic!("DMA read at {gpa:#x}")
    }
    fn write(&mut self, gpa: u64, _: &[u8]) -> bool {
        panic!("DMA write at {gpa:#x}")
    }
}

impl BlockBackend for Idle {
    fn sectors(&self) -> u64 {
        0
    }
    fn read(&mut self, s: u64, _: &mut [u8; SECTOR]) -> bool {
        panic!("disk read {s}")
    }
    fn write(&mut self, s: u64, _: &[u8; SECTOR]) -> bool {
        panic!("disk write {s}")
    }
    fn flush(&mut self) -> bool {
        panic!("disk flush")
    }
}

impl NetBackend for Idle {
    fn send(&mut self, _: &[u8]) {
        panic!("frame sent")
    }
    fn recv(&mut self, _: &mut [u8; MAX_FRAME]) -> Option<usize> {
        None
    }
}

impl Scanout for Idle {
    fn create(&mut self, _: u32, _: u32, _: u32, _: u32) -> bool {
        panic!("display resource created")
    }
    fn destroy(&mut self, _: u32) {}
    fn put(&mut self, _: u32, _: u32, _: u32, _: &[u8]) {
        panic!("pixels")
    }
    fn show(&mut self, _: u32, _: u32, _: Rect) {}
    fn present(&mut self, _: u32, _: u32, _: Rect) {}
}

impl ConsoleBackend for Idle {
    fn write(&mut self, _: &[u8]) {
        panic!("agent channel written")
    }
    fn read(&mut self, _: &mut [u8]) -> usize {
        0
    }
}

impl InputHost for Idle {
    fn poll(&mut self, _: &mut Machine, _: u64) {
        self.polls += 1;
    }
}

/// 2 MiB of guest RAM: the kernel at 1 MiB, the ACPI tables at 1.5 MiB.
const PAGES: u64 = 512;
const ACPI_AT: u64 = 0x18_0000;

/// The rig's guest RAM as the loader sees it (identity GPAs, one host frame per page).
struct RigRam<'r> {
    rig: &'r mut Rig,
}

impl GuestMemory for RigRam<'_> {
    fn write(&mut self, gpa: u64, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            let a = gpa + i as u64;
            let f = self.rig.ram[(a >> 12) as usize];
            self.rig.cpu.mem.write_bytes(f + (a & 0xFFF), &[*b]);
        }
    }
    fn read(&mut self, gpa: u64, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            let a = gpa + i as u64;
            let f = self.rig.ram[(a >> 12) as usize];
            let mut one = [0u8; 1];
            self.rig.cpu.mem.read_bytes(f + (a & 0xFFF), &mut one);
            *b = one[0];
        }
    }
}

/// A minimal bzImage: protocol 2.15, 64-bit entry, relocatable, 64 KiB of init_size.
fn bzimage(payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 5 * 512];
    b[0x1F1] = 4;
    b[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
    b[0x201] = 0x6A;
    b[0x202..0x206].copy_from_slice(b"HdrS");
    b[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
    b[0x211] = 1;
    b[0x22C..0x230].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    b[0x230..0x234].copy_from_slice(&0x1000u32.to_le_bytes());
    b[0x234] = 1;
    b[0x236..0x238].copy_from_slice(&1u16.to_le_bytes());
    b[0x238..0x23C].copy_from_slice(&255u32.to_le_bytes());
    b[0x258..0x260].copy_from_slice(&0x10_0000u64.to_le_bytes());
    b[0x260..0x264].copy_from_slice(&0x1_0000u32.to_le_bytes());
    b.extend_from_slice(payload);
    b
}

fn serial(s: &str) -> Vec<Step> {
    s.bytes()
        .map(|c| Step::Out {
            port: 0x3F8,
            size: 1,
            value: u32::from(c),
        })
        .collect()
}

#[test]
fn a_loaded_kernel_starts_at_its_entry_prints_and_powers_off() {
    let mut script = serial("Linux\n");
    // S5 with SLP_EN through the FADT's PM1a control block
    script.push(Step::Out {
        port: PM1_CONTROL,
        size: 2,
        value: u32::from(SLEEP_S5) << 10 | 1 << 13,
    });
    let mut rig = Rig::platform(&script);
    rig.map_ram(PAGES);

    // the platform's ACPI tables in guest RAM
    let mut buf = vec![0u8; 0x1_0000];
    let layout = tables::build(&mut buf, ACPI_AT, &Platform::default()).unwrap();
    RigRam { rig: &mut rig }.write(ACPI_AT, &buf[..layout.len]);

    // the kernel and its zero page
    let image = bzimage(&[0x90; 0x800]);
    let initrd = [0x5Au8; 0x2000];
    let cfg = LinuxConfig {
        ram_bytes: PAGES * 4096,
        cmdline: b"console=ttyS0",
        acpi: Some(Acpi {
            rsdp_gpa: layout.rsdp,
            region_gpa: ACPI_AT,
            region_len: layout.len as u64,
        }),
        // Linux uses an ECAM window only when it finds it reserved.
        reserved: &[(ECAM_BASE, ECAM_SIZE)],
    };
    let entry = linux::load_linux(&mut RigRam { rig: &mut rig }, &image, &initrd, &cfg).unwrap();
    assert_eq!(entry.rip, 0x10_0200);

    // the entry state, and the script laid out from the kernel's entry
    rig.vmcb()
        .setup_linux_boot(entry.rip, entry.cr3, entry.gdt_base, entry.gdt_limit);
    rig.gprs.rsi = entry.rsi;
    rig.cpu.start_at(entry.rip);
    {
        let v = rig.vmcb();
        assert_eq!(v.segment(save::CS).selector, 0x10);
        assert_eq!(v.segment(save::DS).selector, 0x18);
        assert_eq!(v.segment(save::SS).selector, 0x18);
        assert_eq!(v.segment(save::GDTR).base, linux::GDT_GPA);
        assert_eq!(v.segment(save::GDTR).limit, 31);
        assert_eq!(v.read_u64(save::CR3), linux::PAGE_TABLES_GPA);
        assert_eq!(v.rflags() & 1 << 9, 0, "interrupts off");
    }

    // what the kernel will find through RSI: the zero page with the RSDP of these tables
    let mut zp = [0u8; 4096];
    let rsi = rig.gprs.rsi;
    RigRam { rig: &mut rig }.read(rsi, &mut zp);
    assert_eq!(&zp[0x202..0x206], b"HdrS");
    assert_eq!(
        u64::from_le_bytes(zp[0x70..0x78].try_into().unwrap()),
        layout.rsdp
    );
    let mut sig = [0u8; 8];
    RigRam { rig: &mut rig }.read(layout.rsdp, &mut sig);
    assert_eq!(&sig, b"RSD PTR ");

    let mut out = [0u8; 64];
    let mut vcpu = PlatformVcpu::new(rig.cfg, Machine::new(0, 100_000_000), &mut out);
    let (mut mem, mut disk, mut net, mut screen, mut console, mut input) = (
        Idle::default(),
        Idle::default(),
        Idle::default(),
        Idle::default(),
        Idle::default(),
        Idle::default(),
    );
    let mut host = Host {
        mem: &mut mem,
        disk: &mut disk,
        net: &mut net,
        display: &mut screen,
        console: &mut console,
        input: &mut input,
    };
    let o = run_platform(&mut rig, &mut vcpu, &mut host);
    assert_eq!(o.verdict, Verdict::PowerOff);
    assert_eq!(vcpu.serial(), b"Linux\n");
    assert_eq!(
        rig.gprs.rsi,
        linux::BOOT_PARAMS_GPA,
        "RSI kept for the kernel"
    );
    rig.cpu.assert_clean();
}

#[test]
fn the_boot_state_is_what_the_protocol_asks_for() {
    let rig = Rig::platform(&[]);
    let mut out = [0u8; 1];
    let vcpu = PlatformVcpu::new(rig.cfg, Machine::new(0, 100_000_000), &mut out);
    let mut page = Box::new([0u8; 4096]);
    let mut v = Vmcb::new(&mut page);
    vcpu.prepare(&mut v);
    v.setup_linux_boot(0x100_0200, 0x9000, 0x500, 31);
    assert_eq!(v.check(), Ok(()), "the VMRUN checks accept the boot state");
    assert_eq!(v.rip(), 0x100_0200);
    assert_eq!(v.read_u64(save::RSP), 0, "no stack");
    let cs = v.segment(save::CS);
    assert_eq!((cs.selector, cs.base, cs.limit), (0x10, 0, 0xFFFF_FFFF));
    assert_ne!(cs.attrib & 1 << 9, 0, "64-bit code (L)");
    for off in [save::DS, save::ES, save::SS, save::FS, save::GS] {
        assert_eq!(v.segment(off).selector, 0x18);
    }
    assert_eq!(
        (v.segment(save::GDTR).base, v.segment(save::GDTR).limit),
        (0x500, 31)
    );
    let efer = v.read_u64(save::EFER);
    assert_eq!(efer & (1 << 8 | 1 << 10), 1 << 8 | 1 << 10, "LME and LMA");
    assert_eq!(efer & 1 << 11, 0, "NX is the kernel's to enable");
    assert_eq!(v.read_u64(save::CR4), 1 << 5, "PAE only");
    let cr0 = v.read_u64(save::CR0);
    assert_eq!(cr0 & (1 | 1 << 31), 1 | 1 << 31, "PE and PG");
    assert_eq!(v.read_u64(save::CR3), 0x9000);
    assert_eq!(v.rflags(), 2);
}
