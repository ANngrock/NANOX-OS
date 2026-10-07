//! Privileged instructions. The probe runs as a UEFI application at CPL 0
//! under OVMF, whose page tables identity-map memory: a pointer value is
//! the physical address.

use core::arch::{asm, global_asm};
use core::fmt;

pub fn outb(port: u16, value: u8) {
    // SAFETY: CPL 0; port I/O has no memory operands. Only the fixed COM1,
    // isa-debug-exit and PCI configuration ports and (in an interactive run)
    // the PS/2 controller and PIT channel 2 ports of the probe profile are used.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) }
}

pub fn outw(port: u16, value: u16) {
    // SAFETY: as in `outb` (the fw_cfg selector port).
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack)) }
}

/// OUT of a 32-bit value that starts a device's DMA (the fw_cfg DMA address
/// register): without `nomem`, so the compiler keeps the memory the device
/// reads and writes in place around it.
pub fn outl(port: u16, value: u32) {
    // SAFETY: as in `outb`; the only DMA started is fw_cfg's, into a buffer
    // the caller owns.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack)) }
}

/// IN of a 32-bit value (PCI configuration data, port 0xCFC).
pub fn inl(port: u16) -> u32 {
    let value;
    // SAFETY: as in `outb`.
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack)) }
    value
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

/// `pages` 4 KiB pages of EfiLoaderData anywhere, from the firmware's boot
/// services (UEFI 2.10 §7.2 AllocatePages; EFI_SYSTEM_TABLE.BootServices at
/// 0x60, EFI_BOOT_SERVICES.AllocatePages at 0x28): the `linux` case needs
/// hundreds of MiB, which a static pool would add to the image every profile
/// loads. OVMF identity-maps all memory, so the address is also a pointer.
/// Interrupts stay off: AllocatePages raises the TPL to TPL_NOTIFY only, and
/// returning from it re-enables interrupts only below TPL_HIGH_LEVEL.
pub fn allocate_pages(system: *mut u8, pages: usize) -> Option<u64> {
    type AllocatePages = unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> usize;
    const ALLOCATE_ANY_PAGES: u32 = 0;
    const ALLOCATE_MAX_ADDRESS: u32 = 1;
    const EFI_LOADER_DATA: u32 = 2;
    // SAFETY: `system` is the EFI_SYSTEM_TABLE the firmware passed to
    // efi_main; boot services are still active (the probe never calls
    // ExitBootServices), so the table and the function are valid.
    let allocate = |kind: u32, mut addr: u64| unsafe {
        let boot = *(system.add(0x60) as *const *const u8);
        let f: AllocatePages = core::mem::transmute(*(boot.add(0x28) as *const usize));
        (f(kind, EFI_LOADER_DATA, pages, &mut addr) == 0).then_some(addr)
    };
    // AllocateAnyPages stays below 4 GiB in OVMF; a guest's GiBs of RAM may
    // only fit above, so the second try allows any address.
    allocate(ALLOCATE_ANY_PAGES, 0).or_else(|| allocate(ALLOCATE_MAX_ADDRESS, u64::MAX))
}

/// The firmware's display (UEFI 2.10 §12.9, Graphics Output Protocol): the
/// mode in use and its linear framebuffer.
#[derive(Clone, Copy, Debug)]
pub struct Gop {
    pub base: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// EFI_GRAPHICS_PIXEL_FORMAT.
    pub format: u32,
}

/// EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID 9042a9de-23dc-4a38-96fb-7aded080516a, as laid out in memory.
const GOP_GUID: [u8; 16] = [
    0xde, 0xa9, 0x42, 0x90, 0xdc, 0x23, 0x38, 0x4a, 0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a,
];
/// PixelBlueGreenRedReserved8BitPerColor.
const GOP_BGRX8: u32 = 1;

/// Finds the display (EFI_BOOT_SERVICES.LocateProtocol at 0x140), switches
/// it to the largest blue-green-red mode that fits `max_w` × `max_h` (if
/// there is one) and returns the mode in use; None without a display. The
/// mode information QueryMode allocates stays with the firmware (a few dozen
/// bytes per mode, once per run).
pub fn gop(system: *mut u8, max_w: u32, max_h: u32) -> Option<Gop> {
    type LocateProtocol = unsafe extern "efiapi" fn(*const u8, *const u8, *mut *mut u8) -> usize;
    type QueryMode = unsafe extern "efiapi" fn(*mut u8, u32, *mut usize, *mut *const u8) -> usize;
    type SetMode = unsafe extern "efiapi" fn(*mut u8, u32) -> usize;
    let mut gop: *mut u8 = core::ptr::null_mut();
    // SAFETY: `system` is the EFI_SYSTEM_TABLE and boot services are active
    // (see `allocate_pages`); LocateProtocol writes the interface pointer.
    let status = unsafe {
        let boot = *(system.add(0x60) as *const *const u8);
        let locate: LocateProtocol = core::mem::transmute(*(boot.add(0x140) as *const usize));
        locate(GOP_GUID.as_ptr(), core::ptr::null(), &mut gop)
    };
    if status != 0 || gop.is_null() {
        return None;
    }
    // SAFETY: `gop` is the EFI_GRAPHICS_OUTPUT_PROTOCOL the firmware handed
    // out: QueryMode at 0, SetMode at 8, Mode at 0x18; EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE
    // has MaxMode at 0, Info at 8, FrameBufferBase at 24 and FrameBufferSize at 32;
    // EFI_GRAPHICS_OUTPUT_MODE_INFORMATION has the resolution at 4 and 8, the
    // pixel format at 12 and PixelsPerScanLine at 32.
    unsafe {
        let query: QueryMode = core::mem::transmute(*(gop as *const usize));
        let set: SetMode = core::mem::transmute(*(gop.add(8) as *const usize));
        let mode = *(gop.add(0x18) as *const *const u8);
        let max_mode = *(mode as *const u32);
        let mut best: Option<(u32, u64)> = None;
        for m in 0..max_mode {
            let (mut size, mut info) = (0usize, core::ptr::null::<u8>());
            if query(gop, m, &mut size, &mut info) != 0 || info.is_null() {
                continue;
            }
            let w = *(info.add(4) as *const u32);
            let h = *(info.add(8) as *const u32);
            let area = u64::from(w) * u64::from(h);
            let fits = *(info.add(12) as *const u32) == GOP_BGRX8 && w <= max_w && h <= max_h;
            if fits && best.is_none_or(|(_, a)| area > a) {
                best = Some((m, area));
            }
        }
        if let Some((m, _)) = best {
            if set(gop, m) != 0 {
                return None;
            }
        }
        let info = *(mode.add(8) as *const *const u8);
        let word = |at: usize| *(info.add(at) as *const u32);
        Some(Gop {
            base: *(mode.add(24) as *const u64),
            size: *(mode.add(32) as *const usize) as u64,
            width: word(4),
            height: word(8),
            format: word(12),
            stride: word(32),
        })
    }
}

/// Ends the QEMU run through isa-debug-exit: status `(value << 1) | 1`.
pub fn exit(value: u32) -> ! {
    // SAFETY: the probe profile always has isa-debug-exit at 0xF4; without
    // it the OUT is ignored and the loop below halts the processor.
    unsafe { asm!("out dx, eax", in("dx") 0xF4u16, in("eax") value, options(nomem, nostack)) }
    halt()
}

/// The probe machine's time-stamp counter (the probe's own, never a guest's).
pub fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC reads a counter; CR4.TSD is clear at CPL 0 anyway.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) }
    u64::from(hi) << 32 | u64::from(lo)
}

/// Stops the processor for good, leaving the display as it is.
pub fn halt() -> ! {
    loop {
        // SAFETY: final state; interrupts masked.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

// VMSAVE host state, VMLOAD+VMRUN+VMSAVE the guest, restore the host.
// sysv64: RDI = 14 guest GPRs (rbx, rcx, rdx, rsi, rdi, rbp, r8..r15),
// RSI = VMCB physical address, RDX = host save area physical address,
// RCX != 0: run with host IF=1 (a host interrupt then exits the guest,
// and is taken by the host IDT between STGI and CLI).
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
    "test rcx, rcx",
    "jz .Lprobe_no_irq",
    "sti", // GIF=0: nothing is taken before VMRUN
    ".Lprobe_no_irq:",
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
    ".byte 0x0f, 0x01, 0xdc", // STGI: a pending host interrupt is taken here
    "cli",
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
    fn nanox_svm_probe_vmrun(gprs: *mut u64, vmcb: u64, host_save: u64, host_irq: u64);
}

/// # Safety
/// `vmcb` and `host_save` are 4 KiB pages owned by the caller, the VMCB
/// passed the consistency checks or is deliberately invalid (the processor
/// then exits with VMEXIT_INVALID without entering the guest), EFER.SVME
/// and VM_HSAVE_PA are set, and the nested page tables map only probe-owned
/// frames, so the guest cannot reach host memory. With `host_irq`, the
/// probe's host IDT ([`HostTick::start`]) must be loaded.
pub unsafe fn vmrun(gprs: &mut [u64; 14], vmcb: *mut [u8; 4096], host_save: u64, host_irq: bool) {
    // SAFETY: per the contract above; the routine restores every register
    // the sysv64 ABI requires and writes only `gprs`, the VMCB and the two
    // save areas.
    unsafe {
        nanox_svm_probe_vmrun(
            gprs.as_mut_ptr(),
            vmcb as u64,
            host_save,
            u64::from(host_irq),
        )
    }
}

// Host interrupt handler for every vector: EOI to the local APIC (address
// patched in by HostTick::start) and return. Only interrupts are expected;
// the probe raises no exceptions.
global_asm!(
    ".global nanox_probe_host_irq",
    "nanox_probe_host_irq:",
    "push rax",
    "mov rax, [rip + nanox_probe_host_eoi]",
    "mov dword ptr [rax], 0",
    "pop rax",
    "iretq",
    ".data",
    ".balign 8",
    ".global nanox_probe_host_eoi",
    "nanox_probe_host_eoi:",
    ".quad 0",
    ".text",
);

unsafe extern "C" {
    static nanox_probe_host_irq: u8;
    static mut nanox_probe_host_eoi: u64;
}

const HOST_TICK_VECTOR: u32 = 0xF0;

/// A periodic host timer so a guest spinning without exits still leaves
/// the guest (INTR exit) about every `period` of host time: the probe's
/// own IDT (all vectors: EOI and return), the host local APIC timer, the
/// 8259s masked.
pub struct HostTick {
    idt: [u64; 512],
    apic: u64,
}

fn apic_write(base: u64, off: u64, v: u32) {
    // SAFETY: the host local APIC page (IA32_APIC_BASE), identity-mapped
    // uncached by OVMF; 32-bit aligned register access.
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

impl Default for HostTick {
    fn default() -> Self {
        Self::new()
    }
}

impl HostTick {
    pub const fn new() -> Self {
        Self {
            idt: [0; 512],
            apic: 0,
        }
    }

    /// Loads the probe IDT and starts the host APIC timer with `count`
    /// ticks at bus/16.
    pub fn start(&mut self, count: u32) {
        self.apic = rdmsr(0x1B) & 0x000F_FFFF_FFFF_F000;
        // SAFETY: a plain store to the probe's own handler data, before any
        // interrupt can use it (IF=0).
        unsafe { (&raw mut nanox_probe_host_eoi).write_volatile(self.apic + 0xB0) };
        let handler = (&raw const nanox_probe_host_irq) as u64;
        let cs: u16;
        // SAFETY: reads the current code selector.
        unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack)) };
        for v in 0..256 {
            // 64-bit interrupt gate, present, DPL 0.
            self.idt[2 * v] = (handler & 0xFFFF)
                | u64::from(cs) << 16
                | 0x8E00u64 << 32
                | (handler >> 16 & 0xFFFF) << 48;
            self.idt[2 * v + 1] = handler >> 32;
        }
        let mut idtr = [0u8; 10];
        idtr[..2].copy_from_slice(&(4095u16).to_le_bytes());
        idtr[2..].copy_from_slice(&(self.idt.as_ptr() as u64).to_le_bytes());
        // SAFETY: the IDT lives in `self`, which the caller keeps alive and
        // in place while host interrupts may occur; interrupts are off.
        unsafe { asm!("lidt [{}]", in(reg) idtr.as_ptr(), options(nostack)) };
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);
        apic_write(self.apic, 0xF0, 0x1FF); // SVR: enabled, spurious 0xFF
        apic_write(self.apic, 0x3E0, 3); // divide by 16
        apic_write(self.apic, 0x320, HOST_TICK_VECTOR | 1 << 17); // periodic
        apic_write(self.apic, 0x380, count);
    }

    /// Stops the host timer; the probe IDT stays loaded (harmless with
    /// IF=0).
    pub fn stop(&mut self) {
        if self.apic != 0 {
            apic_write(self.apic, 0x320, HOST_TICK_VECTOR | 1 << 16);
            apic_write(self.apic, 0x380, 0);
        }
    }
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
