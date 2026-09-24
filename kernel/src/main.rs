#![no_std]
#![no_main]

use boot_protocol::{BootInfo, FLAG_TEST_PROFILE, HANDOFF_BASE, HANDOFF_MAPPED_SIZE};
use core::{
    arch::{asm, global_asm},
    fmt::{self, Write},
    panic::PanicInfo,
    sync::atomic::{AtomicBool, Ordering},
};

static TEST_PROFILE: AtomicBool = AtomicBool::new(false);
static mut DATA_SENTINEL: u64 = 0x4e41_4e4f_5844_4154;
static mut BSS_SENTINEL: [u8; 64] = [0; 64];

// The loader enters with RDI=BootInfo, interrupts disabled, a private 64 KiB
// mapped stack and a mapped early GDT/IDT. Align BEFORE call: SysV callee sees
// RSP % 16 == 8 after the return address is pushed. No Rust ABI crosses handoff.
global_asm!(
    ".section .text.entry,\"ax\"",
    ".global _start",
    "_start:",
    "cli",
    "cld",
    "and rsp, -16",
    "xor rbp, rbp",
    "call kernel_main",
    "2: hlt",
    "jmp 2b",
);

struct Serial;

fn outb(port: u16, value: u8) {
    // SAFETY: ring 0; only the boot CPU uses the fixed legacy UART ports in M0.
    // Port I/O does not borrow memory; interrupts are disabled throughout.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

fn inb(port: u16) -> u8 {
    let value;
    // SAFETY: fixed UART status port, sole boot CPU, no memory access or aliases.
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
    }
    value
}

impl Serial {
    fn init() {
        outb(0x3f9, 0);
        outb(0x3fb, 0x80);
        outb(0x3f8, 1);
        outb(0x3f9, 0);
        outb(0x3fb, 3);
        outb(0x3fa, 0xc7);
        outb(0x3fc, 3);
    }
}

impl Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            // Bounded polling: a broken UART must not trap diagnostics forever.
            for _ in 0..100_000 {
                if inb(0x3fd) & 0x20 != 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            outb(0x3f8, byte);
        }
        Ok(())
    }
}

fn idle() -> ! {
    loop {
        // SAFETY: kernel owns this CPU, no enabled interrupt handlers/locks in M0.
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}

fn test_exit(value: u32) -> ! {
    // SAFETY: called only after validating the test-profile bit; 0xf4 is the
    // explicitly configured QEMU test device, never a physical poweroff API.
    unsafe {
        asm!("out dx, eax", in("dx") 0xf4u16, in("eax") value, options(nomem, nostack));
    }
    idle()
}

fn mapped_buffer(address: u64, len: u64) -> Option<&'static [u8]> {
    let end = address.checked_add(len)?;
    if address < HANDOFF_BASE
        || end > HANDOFF_BASE.checked_add(HANDOFF_MAPPED_SIZE)?
        || len > isize::MAX as u64
    {
        return None;
    }
    // SAFETY: loader maps and reserves the handoff arena containing every
    // header-validated buffer. All bytes were initialized before handoff and
    // remain owned by the kernel; no allocator, other CPU or IRQ can mutate it.
    // u8 has alignment 1; checked range and isize bound precede slice creation.
    Some(unsafe { core::slice::from_raw_parts(address as *const u8, len as usize) })
}

#[no_mangle]
extern "C" fn kernel_main(pointer: u64) -> ! {
    Serial::init();
    let _ = writeln!(Serial, "NANOX:KERNEL:ENTER");
    if pointer != HANDOFF_BASE || pointer % core::mem::align_of::<BootInfo>() as u64 != 0 {
        let _ = writeln!(Serial, "NANOX:KERNEL:BOOTINFO_ERROR:pointer");
        idle();
    }
    // SAFETY: entry contract supplies the 8-byte-aligned, initialized 160-byte
    // header at HANDOFF_BASE, mapped by the loader and exclusively owned after
    // ExitBootServices. Copy it, keeping no Rust reference across the ABI.
    let info = unsafe { core::ptr::read(pointer as *const BootInfo) };
    if let Err(error) = info.validate_header_at(pointer) {
        let _ = writeln!(Serial, "NANOX:KERNEL:BOOTINFO_ERROR:{error:?}");
        idle();
    }
    let test_profile = info.flags & FLAG_TEST_PROFILE != 0;
    TEST_PROFILE.store(test_profile, Ordering::Relaxed);
    let buffers = (|| {
        let map = mapped_buffer(info.memory_map_virt, info.memory_map_len)?;
        let ranges = mapped_buffer(
            info.reserved_ranges_virt,
            u64::from(info.reserved_ranges_count)
                .checked_mul(u64::from(info.reserved_ranges_stride))?,
        )?;
        let segments = mapped_buffer(
            info.load_segments_virt,
            u64::from(info.load_segments_count)
                .checked_mul(u64::from(info.load_segments_stride))?,
        )?;
        Some((map, ranges, segments))
    })();
    let Some((map, ranges, segments)) = buffers else {
        let _ = writeln!(Serial, "NANOX:KERNEL:BOOTINFO_ERROR:buffers");
        if test_profile {
            test_exit(0x11);
        }
        idle();
    };
    if let Err(error) = info.validate_buffers(map, ranges, segments) {
        let _ = writeln!(Serial, "NANOX:KERNEL:BOOTINFO_ERROR:{error:?}");
        if test_profile {
            test_exit(0x11);
        }
        idle();
    }
    let active_cr3: u64;
    // SAFETY: privileged read on our single boot CPU; no memory or alias effects.
    unsafe {
        asm!("mov {}, cr3", out(reg) active_cr3, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: statics are aligned, initialized, mapped kernel allocations that
    // remain alive and immutable. Volatile reads ensure testing loaded bytes.
    let initialized_data = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DATA_SENTINEL)) };
    // SAFETY: the 64-byte static has alignment 1 and lifetime of the kernel;
    // no thread or IRQ writes it. Read the actual mapped memory, not a constant.
    let initialized_bss = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(BSS_SENTINEL)) };
    if active_cr3 != info.pml4_phys
        || initialized_data != 0x4e41_4e4f_5844_4154
        || initialized_bss != [0; 64]
    {
        let _ = writeln!(Serial, "NANOX:KERNEL:BOOTINFO_ERROR:machine_state");
        if test_profile {
            test_exit(0x11);
        }
        idle();
    }
    let _ = writeln!(Serial, "NANOX:KERNEL:BOOTINFO_VALIDATED");
    let _ = writeln!(
        Serial,
        "map_bytes={} descriptor_stride={} reservations={} segments={} pml4={:#x} epoch={}",
        info.memory_map_len,
        info.memory_descriptor_size,
        info.reserved_ranges_count,
        info.load_segments_count,
        info.pml4_phys,
        info.boot_epoch
    );
    if test_profile {
        match info.boot_epoch {
            u64::MAX => {
                let _ = writeln!(Serial, "NANOX:TEST:FAIL:injected");
                test_exit(0x11);
            }
            epoch if epoch == u64::MAX - 1 => {
                let _ = writeln!(Serial, "NANOX:TEST:HANG:injected");
                idle();
            }
            epoch if epoch == u64::MAX - 2 => panic!("injected kernel panic"),
            _ => {}
        }
        let _ = writeln!(Serial, "NANOX:TEST:PASS");
        test_exit(0x10);
    }
    let _ = writeln!(Serial, "NANOX:KERNEL:IDLE");
    idle()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(Serial, "NANOX:KERNEL:PANIC:{info}");
    // Only a previously validated header enables the QEMU-only exit device.
    if TEST_PROFILE.load(Ordering::Relaxed) {
        test_exit(0x11);
    }
    idle()
}
