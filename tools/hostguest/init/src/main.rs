//! Init for the measurement boot: mounts the pseudo file systems, prints what
//! the guest kernel reports about its machine on the console, greets the host
//! on the agent channel (virtio-console, `/dev/hvc0`) when there is one,
//! brings up `eth0` and pings the host (10.0.2.2) when there is a network card,
//! and powers off.
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
const SYS_POLL: usize = 7;
const SYS_IOCTL: usize = 16;
const SYS_SOCKET: usize = 41;
const SYS_SENDTO: usize = 44;
const SYS_RECVFROM: usize = 45;
const SYS_DUP2: usize = 33;
const SYS_MKDIR: usize = 83;
const SYS_MOUNT: usize = 165;
const SYS_REBOOT: usize = 169;
const SYS_EXIT_GROUP: usize = 231;
const O_RDWR: usize = 2;
const O_NOCTTY: usize = 0o400;
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
/// `c_oflag`: output processing (`\n` to `\r\n`).
const OPOST: u32 = 0o1;
/// `c_lflag`: echo of the input.
const ECHO: u32 = 0o10;
const POLLIN: u16 = 1;
/// How long the host may take to answer on the agent channel.
const AGENT_WAIT_MS: usize = 1000;
const AF_INET: u16 = 2;
const SOCK_DGRAM: usize = 2;
const SOCK_RAW: usize = 3;
const IPPROTO_ICMP: usize = 1;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const SIOCSIFADDR: usize = 0x8916;
const SIOCSIFNETMASK: usize = 0x891C;
const IFF_UP: u8 = 1;
/// The guest's address and the host's on the VMM's network (as QEMU's user network has them).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const NETMASK: [u8; 4] = [255, 255, 255, 0];
const HOST_IP: [u8; 4] = [10, 0, 2, 2];
const PING_DATA: &[u8] = b"NANOX ping";
/// How long the host may take to answer a ping.
const PING_WAIT_MS: usize = 1000;

unsafe fn syscall(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    // SAFETY: the caller's arguments; the sixth is 0.
    unsafe { syscall6(n, a, b, c, d, e, 0) }
}

unsafe fn syscall6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let ret: isize;
    // SAFETY: the Linux system call ABI; the arguments are the caller's.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
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

/// The agent channel (virtio-console port 0, `/dev/hvc0`): one line to the
/// host, and its answer line (if it comes within [`AGENT_WAIT_MS`]) on the console.
fn agent() {
    write(1, b"--- agent\n");
    let mut b = [0u8; 128];
    let p = path("/dev/hvc0", &mut b);
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { syscall(SYS_OPEN, p, O_RDWR | O_NOCTTY, 0, 0, 0) };
    if fd < 0 {
        write(1, b"(not available)\n");
        return;
    }
    let fd = fd as usize;
    // The kernel's struct termios: four flag words, the line discipline and 19
    // control characters. Raw output and no echo: the host gets exactly the
    // bytes written, and its answer is not sent back to it.
    let mut t = [0u32; 9];
    // SAFETY: a buffer of the kernel's termios size for TCGETS and TCSETS.
    unsafe {
        if syscall(SYS_IOCTL, fd, TCGETS, t.as_mut_ptr() as usize, 0, 0) == 0 {
            t[1] &= !OPOST;
            t[3] &= !ECHO;
            syscall(SYS_IOCTL, fd, TCSETS, t.as_ptr() as usize, 0, 0);
        }
    }
    write(fd, b"NANOX_AGENT_HELLO\n");
    // struct pollfd: fd, events, revents.
    let mut pfd = [fd as u32, u32::from(POLLIN)];
    // SAFETY: one pollfd; the canonical tty returns one line per read.
    let ready = unsafe { syscall(SYS_POLL, pfd.as_mut_ptr() as usize, 1, AGENT_WAIT_MS, 0, 0) };
    let mut line = [0u8; 128];
    let n = if ready > 0 {
        // SAFETY: a valid buffer.
        unsafe { syscall(SYS_READ, fd, line.as_mut_ptr() as usize, line.len(), 0, 0) }
    } else {
        0
    };
    if n > 0 {
        write(1, &line[..n as usize]);
    } else {
        write(1, b"(no answer)\n");
    }
    // SAFETY: closing the descriptor we opened.
    unsafe { syscall(SYS_CLOSE, fd, 0, 0, 0, 0) };
}

/// `n` in decimal.
fn write_dec(fd: usize, mut n: usize) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    write(fd, &d[i..]);
}

/// A struct sockaddr_in: AF_INET, port 0, `addr`.
fn sockaddr(addr: [u8; 4]) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[..2].copy_from_slice(&AF_INET.to_le_bytes());
    s[4..8].copy_from_slice(&addr);
    s
}

/// A struct ifreq for `eth0`, its union holding a struct sockaddr_in of `addr`.
fn ifreq(addr: [u8; 4]) -> [u8; 40] {
    let mut r = [0u8; 40];
    r[..4].copy_from_slice(b"eth0");
    r[16..32].copy_from_slice(&sockaddr(addr));
    r
}

/// The Internet checksum (RFC 1071).
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in data.chunks(2) {
        sum += u32::from(u16::from_be_bytes([
            pair[0],
            pair.get(1).copied().unwrap_or(0),
        ]));
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// `eth0` gets [`GUEST_IP`]/24 and is brought up; one ICMP echo request goes
/// to [`HOST_IP`] and its reply (if it comes within [`PING_WAIT_MS`]) is
/// reported.
fn net() {
    write(1, b"--- net\n");
    // SAFETY: the socket, ioctl and close system calls with buffers of the
    // kernel's struct ifreq size; the descriptor is ours.
    let up = unsafe {
        let s = syscall(SYS_SOCKET, AF_INET as usize, SOCK_DGRAM, 0, 0, 0);
        if s < 0 {
            false
        } else {
            let s = s as usize;
            let (addr, mask, mut flags) = (ifreq(GUEST_IP), ifreq(NETMASK), ifreq([0; 4]));
            let ok = syscall(SYS_IOCTL, s, SIOCSIFADDR, addr.as_ptr() as usize, 0, 0) == 0
                && syscall(SYS_IOCTL, s, SIOCSIFNETMASK, mask.as_ptr() as usize, 0, 0) == 0
                && syscall(
                    SYS_IOCTL,
                    s,
                    SIOCGIFFLAGS,
                    flags.as_mut_ptr() as usize,
                    0,
                    0,
                ) == 0
                && {
                    flags[16] |= IFF_UP;
                    syscall(SYS_IOCTL, s, SIOCSIFFLAGS, flags.as_ptr() as usize, 0, 0) == 0
                };
            syscall(SYS_CLOSE, s, 0, 0, 0, 0);
            ok
        }
    };
    if !up {
        write(1, b"(no eth0)\n");
        return;
    }
    write(1, b"eth0 10.0.2.15/24 up\n");
    // SAFETY: a raw ICMP socket (init runs as root).
    let r = unsafe { syscall(SYS_SOCKET, AF_INET as usize, SOCK_RAW, IPPROTO_ICMP, 0, 0) };
    if r < 0 {
        write(1, b"(no raw socket)\n");
        return;
    }
    let r = r as usize;
    // Echo request: type 8, code 0, checksum, identifier "NX", sequence 1, data.
    let mut req = [0u8; 8 + PING_DATA.len()];
    req[..8].copy_from_slice(&[8, 0, 0, 0, b'N', b'X', 0, 1]);
    req[8..].copy_from_slice(PING_DATA);
    let sum = checksum(&req);
    req[2..4].copy_from_slice(&sum.to_be_bytes());
    let to = sockaddr(HOST_IP);
    let mut pfd = [r as u32, u32::from(POLLIN)];
    let mut buf = [0u8; 128];
    // SAFETY: valid buffers for sendto, poll and recvfrom.
    let n = unsafe {
        let sent = syscall6(
            SYS_SENDTO,
            r,
            req.as_ptr() as usize,
            req.len(),
            0,
            to.as_ptr() as usize,
            to.len(),
        );
        if sent == req.len() as isize
            && syscall(SYS_POLL, pfd.as_mut_ptr() as usize, 1, PING_WAIT_MS, 0, 0) > 0
        {
            syscall(SYS_RECVFROM, r, buf.as_mut_ptr() as usize, buf.len(), 0, 0)
        } else {
            0
        }
    };
    // SAFETY: closing the descriptor we opened.
    unsafe { syscall(SYS_CLOSE, r, 0, 0, 0, 0) };
    // A raw socket gets the IPv4 header, then the ICMP message.
    let n = n.max(0) as usize;
    let ihl = usize::from(buf[0] & 0x0F) * 4;
    let reply = buf.get(ihl..n).unwrap_or(&[]);
    if reply.len() == req.len() && reply[0] == 0 && reply[4..] == req[4..] && checksum(reply) == 0 {
        write(1, b"ping 10.0.2.2: echo reply, ttl ");
        write_dec(1, usize::from(buf[8]));
        write(1, b", ");
        write_dec(1, n);
        write(1, b" bytes\n");
    } else {
        write(1, b"(no reply)\n");
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
    agent();
    net();
    write(1, b"NANOX_GUEST_REPORT_END\n");
    // SAFETY: the reboot system call with the documented magic numbers; power off.
    unsafe { syscall(SYS_REBOOT, 0xfee1dead, 672274793, 0x4321fedc, 0, 0) };
    // SAFETY: leaving the process.
    unsafe { syscall(SYS_EXIT_GROUP, 0, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
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
    loop {
        core::hint::spin_loop();
    }
}
