//! Init for the measurement boot: mounts the pseudo file systems, prints what
//! the guest kernel reports about its machine on the console, and powers off.
//! Raw Linux system calls, no libc: the same kind of freestanding static
//! program a NANOX native process is.

#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_OPEN: usize = 2;
const SYS_CLOSE: usize = 3;
const SYS_DUP2: usize = 33;
const SYS_MKDIR: usize = 83;
const SYS_MOUNT: usize = 165;
const SYS_REBOOT: usize = 169;
const SYS_EXIT_GROUP: usize = 231;

unsafe fn syscall(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    let ret: isize;
    // SAFETY: the Linux system call ABI; the arguments are the caller's.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e,
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

fn write(fd: usize, bytes: &[u8]) {
    let mut left = bytes;
    while !left.is_empty() {
        // SAFETY: a valid buffer.
        let n = unsafe { syscall(SYS_WRITE, fd, left.as_ptr() as usize, left.len(), 0, 0) };
        if n <= 0 {
            return;
        }
        left = &left[n as usize..];
    }
}

/// A NUL-terminated copy of a path on the stack.
fn path(s: &str, buf: &mut [u8; 128]) -> usize {
    let n = s.len().min(127);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    buf[n] = 0;
    buf.as_ptr() as usize
}

fn mkdir(p: &str) {
    let mut b = [0u8; 128];
    let a = path(p, &mut b);
    // SAFETY: a NUL-terminated path.
    unsafe { syscall(SYS_MKDIR, a, 0o755, 0, 0, 0) };
}

fn mount(src: &str, target: &str, fstype: &str) {
    let (mut a, mut b, mut c) = ([0u8; 128], [0u8; 128], [0u8; 128]);
    let (s, t, f) = (
        path(src, &mut a),
        path(target, &mut b),
        path(fstype, &mut c),
    );
    // SAFETY: NUL-terminated strings.
    unsafe { syscall(SYS_MOUNT, s, t, f, 0, 0) };
}

/// Copies a file to the console, line prefix first.
fn cat(label: &str, file: &str) {
    write(1, b"--- ");
    write(1, label.as_bytes());
    write(1, b"\n");
    let mut b = [0u8; 128];
    let p = path(file, &mut b);
    // SAFETY: a NUL-terminated path; read-only open.
    let fd = unsafe { syscall(SYS_OPEN, p, 0, 0, 0, 0) };
    if fd < 0 {
        write(1, b"(not available)\n");
        return;
    }
    let mut chunk = [0u8; 512];
    loop {
        // SAFETY: a valid buffer.
        let n = unsafe {
            syscall(
                SYS_READ,
                fd as usize,
                chunk.as_mut_ptr() as usize,
                chunk.len(),
                0,
                0,
            )
        };
        if n <= 0 {
            break;
        }
        write(1, &chunk[..n as usize]);
    }
    // SAFETY: closing the descriptor we opened.
    unsafe { syscall(SYS_CLOSE, fd as usize, 0, 0, 0, 0) };
}

/// Prints the entries of a directory by reading their uevent files.
fn pci_devices() {
    write(1, b"--- pci\n");
    for slot in 0..32u8 {
        for func in 0..8u8 {
            let mut name = *b"/sys/bus/pci/devices/0000:00:00.0/vendor\0";
            name[29] = b'0' + slot / 10;
            name[30] = b'0' + slot % 10;
            name[32] = b'0' + func;
            // SAFETY: a NUL-terminated path.
            let fd = unsafe { syscall(SYS_OPEN, name.as_ptr() as usize, 0, 0, 0, 0) };
            if fd < 0 {
                continue;
            }
            let mut v = [0u8; 16];
            // SAFETY: a valid buffer.
            let n = unsafe { syscall(SYS_READ, fd as usize, v.as_mut_ptr() as usize, 16, 0, 0) };
            // SAFETY: closing our descriptor.
            unsafe { syscall(SYS_CLOSE, fd as usize, 0, 0, 0, 0) };
            write(1, &name[21..33]);
            write(1, b" vendor ");
            write(1, &v[..(n.max(0) as usize).min(16)]);
            // The device id sits next to the vendor in the same directory.
            let mut did = name;
            did[34..40].copy_from_slice(b"device");
            did[40] = 0;
            let fd2 = unsafe { syscall(SYS_OPEN, did.as_ptr() as usize, 0, 0, 0, 0) };
            if fd2 >= 0 {
                let mut d = [0u8; 16];
                let m =
                    unsafe { syscall(SYS_READ, fd2 as usize, d.as_mut_ptr() as usize, 16, 0, 0) };
                unsafe { syscall(SYS_CLOSE, fd2 as usize, 0, 0, 0, 0) };
                write(1, b" device ");
                write(1, &d[..(m.max(0) as usize).min(16)]);
            }
        }
    }
}

#[no_mangle]
extern "C" fn init_main() -> ! {
    mkdir("/dev");
    mkdir("/proc");
    mkdir("/sys");
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    let mut b = [0u8; 128];
    let p = path("/dev/console", &mut b);
    // SAFETY: a NUL-terminated path; the descriptors are ours.
    unsafe {
        let fd = syscall(SYS_OPEN, p, 2, 0, 0, 0);
        if fd >= 0 {
            syscall(SYS_DUP2, fd as usize, 1, 0, 0, 0);
            syscall(SYS_DUP2, fd as usize, 2, 0, 0, 0);
        }
    }
    write(1, b"NANOX_GUEST_REPORT_BEGIN\n");
    cat("ioports", "/proc/ioports");
    cat("iomem", "/proc/iomem");
    cat("interrupts", "/proc/interrupts");
    cat(
        "clocksource",
        "/sys/devices/system/clocksource/clocksource0/current_clocksource",
    );
    cat("devices", "/proc/devices");
    pci_devices();
    write(1, b"NANOX_GUEST_REPORT_END\n");
    // SAFETY: the reboot system call with the documented magic numbers; power off.
    unsafe { syscall(SYS_REBOOT, 0xfee1dead, 672274793, 0x4321fedc, 0, 0) };
    // SAFETY: leaving the process.
    unsafe { syscall(SYS_EXIT_GROUP, 0, 0, 0, 0, 0) };
    loop {}
}

global_asm!(
    ".section .text.entry,\"ax\",@progbits",
    ".global _start",
    "_start:",
    "xor ebp, ebp",
    "and rsp, -16",
    "call init_main",
    "ud2",
);

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    loop {}
}
