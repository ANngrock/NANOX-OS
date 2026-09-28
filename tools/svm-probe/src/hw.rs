//! Privileged instructions. The probe runs as a UEFI application at CPL 0
//! under OVMF, whose page tables identity-map memory: a pointer value is
//! the physical address.

use core::arch::{asm, global_asm};
use core::fmt;

pub fn outb(port: u16, value: u8) {
    // SAFETY: CPL 0; port I/O has no memory operands. Only the fixed COM1
    // and isa-debug-exit ports of the probe profile are used.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) }
}

pub fn outw(port: u16, value: u16) {
    // SAFETY: as in `outb` (the fw_cfg selector port).
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack)) }
}

pub fn inb(port: u16) -> u8 {
    let value;
    // SAFETY: as in `outb`.
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) }
    value
}

pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: CPL 0; callers read only architectural MSRs that exist on a
    // processor reporting SVM (EFER, VM_CR, VM_HSAVE_PA).
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    u64::from(hi) << 32 | u64::from(lo)
}

pub fn wrmsr(msr: u32, value: u64) {
    // SAFETY: CPL 0; callers write EFER.SVME and VM_HSAVE_PA, which change
    // no mapping or state the probe's Rust code relies on.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack),
        );
    }
}

pub fn cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    let (a, c, d): (u32, u32, u32);
    let b: u64;
    // SAFETY: CPUID has no side effects; RBX is reserved by the compiler,
    // so it is saved in a scratch register and swapped back.
    unsafe {
        asm!(
            "mov {t}, rbx",
            "cpuid",
            "xchg {t}, rbx",
            t = out(reg) b,
            inout("eax") leaf => a,
            inout("ecx") subleaf => c,
            out("edx") d,
            options(nomem, nostack, preserves_flags),
        );
    }
    [a, b as u32, c, d]
}

pub fn interrupts_off() {
    // SAFETY: the probe never returns to firmware and needs no interrupts.
    unsafe { asm!("cli", options(nomem, nostack)) }
}

/// Ends the QEMU run through isa-debug-exit: status `(value << 1) | 1`.
pub fn exit(value: u32) -> ! {
    // SAFETY: the probe profile always has isa-debug-exit at 0xF4; without
    // it the OUT is ignored and the loop below halts the processor.
    unsafe { asm!("out dx, eax", in("dx") 0xF4u16, in("eax") value, options(nomem, nostack)) }
    loop {
        // SAFETY: final state; interrupts masked.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

// VMSAVE host state, VMLOAD+VMRUN+VMSAVE the guest, restore the host.
// sysv64: RDI = 14 guest GPRs (rbx, rcx, rdx, rsi, rdi, rbp, r8..r15),
// RSI = VMCB physical address, RDX = host save area physical address.
// VMRUN restores RSP and RAX on #VMEXIT; the VMCB address is reloaded from
// the stack anyway. SVM instructions are emitted as bytes.
global_asm!(
    ".global nanox_svm_probe_vmrun",
    "nanox_svm_probe_vmrun:",
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "push rdi",
    "push rdx",
    "push rsi",
    ".byte 0x0f, 0x01, 0xdd", // CLGI
    "mov rax, rdx",
    ".byte 0x0f, 0x01, 0xdb", // VMSAVE (host)
    "mov rax, rsi",
    "mov rbx, [rdi + 0]",
    "mov rcx, [rdi + 8]",
    "mov rdx, [rdi + 16]",
    "mov rsi, [rdi + 24]",
    "mov rbp, [rdi + 40]",
    "mov r8, [rdi + 48]",
    "mov r9, [rdi + 56]",
    "mov r10, [rdi + 64]",
    "mov r11, [rdi + 72]",
    "mov r12, [rdi + 80]",
    "mov r13, [rdi + 88]",
    "mov r14, [rdi + 96]",
    "mov r15, [rdi + 104]",
    "mov rdi, [rdi + 32]",
    ".byte 0x0f, 0x01, 0xda", // VMLOAD (guest)
    ".byte 0x0f, 0x01, 0xd8", // VMRUN
    "mov rax, [rsp]",
    ".byte 0x0f, 0x01, 0xdb", // VMSAVE (guest)
    "push rdi",
    "mov rdi, [rsp + 24]",
    "mov [rdi + 0], rbx",
    "mov [rdi + 8], rcx",
    "mov [rdi + 16], rdx",
    "mov [rdi + 24], rsi",
    "mov [rdi + 40], rbp",
    "mov [rdi + 48], r8",
    "mov [rdi + 56], r9",
    "mov [rdi + 64], r10",
    "mov [rdi + 72], r11",
    "mov [rdi + 80], r12",
    "mov [rdi + 88], r13",
    "mov [rdi + 96], r14",
    "mov [rdi + 104], r15",
    "pop rax",
    "mov [rdi + 32], rax",
    "pop rax",
    "pop rax",
    ".byte 0x0f, 0x01, 0xda", // VMLOAD (host)
    ".byte 0x0f, 0x01, 0xdc", // STGI
    "pop rdi",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "ret",
);

unsafe extern "sysv64" {
    fn nanox_svm_probe_vmrun(gprs: *mut u64, vmcb: u64, host_save: u64);
}

/// # Safety
/// `vmcb` and `host_save` are 4 KiB pages owned by the caller, the VMCB
/// passed the consistency checks or is deliberately invalid (the processor
/// then exits with VMEXIT_INVALID without entering the guest), EFER.SVME
/// and VM_HSAVE_PA are set, and the nested page tables map only probe-owned
/// frames, so the guest cannot reach host memory.
pub unsafe fn vmrun(gprs: &mut [u64; 14], vmcb: *mut [u8; 4096], host_save: u64) {
    // SAFETY: per the contract above; the routine restores every register
    // the sysv64 ABI requires and writes only `gprs`, the VMCB and the two
    // save areas.
    unsafe { nanox_svm_probe_vmrun(gprs.as_mut_ptr(), vmcb as u64, host_save) }
}

/// COM1, polled.
pub struct Serial;

impl Serial {
    pub fn init() {
        for (port, value) in [
            (0x3F9, 0),
            (0x3FB, 0x80),
            (0x3F8, 1),
            (0x3F9, 0),
            (0x3FB, 3),
            (0x3FA, 0xC7),
            (0x3FC, 3),
        ] {
            outb(port, value);
        }
    }

    pub fn byte(b: u8) {
        for _ in 0..100_000 {
            if inb(0x3FD) & 0x20 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(0x3F8, b);
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        s.bytes().for_each(Serial::byte);
        Ok(())
    }
}
