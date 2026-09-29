//! Guest programs, assembled here and copied into guest RAM at the entry
//! address. They are position-independent (relative jumps, RIP-relative
//! data); absolute operands are guest-physical addresses of the probe's
//! guest layout (see main.rs). The host never executes them.

use core::arch::global_asm;

macro_rules! guest {
    ($name:ident, $start:literal, $end:literal) => {
        pub fn $name() -> &'static [u8] {
            unsafe extern "C" {
                #[link_name = $start]
                static START: u8;
                #[link_name = $end]
                static END: u8;
            }
            let (s, e) = (&raw const START, &raw const END);
            // SAFETY: both labels delimit one program inside .text, which is
            // mapped read-only-executable for the probe's whole lifetime.
            unsafe { core::slice::from_raw_parts(s, e as usize - s as usize) }
        }
    };
}

guest!(basic, "nanox_guest_basic", "nanox_guest_basic_end");
guest!(fail, "nanox_guest_fail", "nanox_guest_fail_end");
guest!(
    npf_absent,
    "nanox_guest_npf_absent",
    "nanox_guest_npf_absent_end"
);
guest!(npf_readonly, "nanox_guest_npf_ro", "nanox_guest_npf_ro_end");
guest!(triple_fault, "nanox_guest_ud2", "nanox_guest_ud2_end");
guest!(halt, "nanox_guest_hlt", "nanox_guest_hlt_end");
guest!(
    msr_denied,
    "nanox_guest_msr_denied",
    "nanox_guest_msr_denied_end"
);
guest!(vmmcall, "nanox_guest_vmmcall", "nanox_guest_vmmcall_end");
guest!(timer, "nanox_guest_timer", "nanox_guest_timer_end");

global_asm!(
    // Local APIC and PIT through the VMM's devices: a GDT (0xA900) and an
    // IDT (0xA000) with a handler for vector 0x40, the APIC at 0xFEE00000
    // (MMIO: version check, SVR, divide 16, periodic 1 ms), five timer
    // interrupts woken from HLT, then the APIC timer measured against
    // PIT channel 2 over 1193 clocks (~1 ms: 62,500 counts at 62.5 MHz).
    ".global nanox_guest_timer",
    ".global nanox_guest_timer_end",
    "nanox_guest_timer:",
    "mov rax, 0x00AF9A000000FFFF",
    "mov qword ptr [0xA908], rax",
    "mov rax, 0x00CF92000000FFFF",
    "mov qword ptr [0xA910], rax",
    "mov qword ptr [0xA900], 0",
    "mov word ptr [0xA880], 0x17",
    "mov qword ptr [0xA882], 0xA900",
    "lgdt [0xA880]",
    "lea rax, [rip + .Ltimer_handler]",
    "mov word ptr [0xA400], ax",
    "mov word ptr [0xA402], 0x08",
    "mov word ptr [0xA404], 0x8E00",
    "shr rax, 16",
    "mov word ptr [0xA406], ax",
    "shr rax, 16",
    "mov dword ptr [0xA408], eax",
    "mov dword ptr [0xA40C], 0",
    "mov word ptr [0xA890], 0xFFF",
    "mov qword ptr [0xA892], 0xA000",
    "lidt [0xA890]",
    "mov dword ptr [0x5000], 0",
    "mov rbx, 0xFEE00000",
    "mov eax, dword ptr [rbx + 0x30]",
    "cmp eax, 0x50014",
    "jne .Ltimer_fail",
    "mov dword ptr [rbx + 0xF0], 0x1FF",
    "mov dword ptr [rbx + 0x3E0], 3",
    "mov dword ptr [rbx + 0x320], 0x20040",
    "mov dword ptr [rbx + 0x380], 62500",
    "sti",
    ".Ltimer_wait:",
    "hlt",
    "cmp dword ptr [0x5000], 5",
    "jb .Ltimer_wait",
    "cli",
    "mov dword ptr [rbx + 0x320], 0x10040",
    "mov dword ptr [rbx + 0x380], 0xFFFFFFFF",
    "in al, 0x61",
    "and al, 0xFC",
    "out 0x61, al",
    "mov al, 0xB0",
    "out 0x43, al",
    "mov al, 0xA9",
    "out 0x42, al",
    "mov al, 0x04",
    "out 0x42, al",
    "in al, 0x61",
    "or al, 1",
    "out 0x61, al",
    "mov ecx, dword ptr [rbx + 0x390]",
    ".Ltimer_poll:",
    "in al, 0x61",
    "test al, 0x20",
    "jz .Ltimer_poll",
    "mov edx, dword ptr [rbx + 0x390]",
    "sub ecx, edx",
    "cmp ecx, 55000",
    "jb .Ltimer_fail",
    "cmp ecx, 70000",
    "ja .Ltimer_fail",
    "mov dx, 0xf4",
    "mov eax, 0x10",
    "out dx, eax",
    "hlt",
    ".Ltimer_fail:",
    "mov dx, 0xf4",
    "mov eax, 0x11",
    "out dx, eax",
    "hlt",
    ".Ltimer_handler:",
    "push rax",
    "inc dword ptr [0x5000]",
    "mov rax, 0xFEE000B0",
    "mov dword ptr [rax], 0",
    "pop rax",
    "iretq",
    "nanox_guest_timer_end:",
);
guest!(remap, "nanox_guest_remap", "nanox_guest_remap_end");

global_asm!(
    // Serial greeting, LSR read (IN EAX must zero-extend RAX), CPUID policy,
    // a store through guest paging and nested paging, a pass-through MSR,
    // then debug-exit 0x10 (status 33). Any mismatch: 0x11 (status 35).
    ".global nanox_guest_basic",
    ".global nanox_guest_basic_end",
    "nanox_guest_basic:",
    "lea rsi, [rip + .Lbasic_msg]",
    "mov dx, 0x3f8",
    ".Lbasic_loop:",
    "lodsb",
    "test al, al",
    "jz .Lbasic_lsr",
    "out dx, al",
    "jmp .Lbasic_loop",
    ".Lbasic_lsr:",
    "mov rax, -1",
    "mov dx, 0x3fd",
    "in eax, dx",
    "cmp rax, 0x60",
    "jne .Lbasic_fail",
    "mov eax, 0x40000000",
    "xor ecx, ecx",
    "cpuid",
    "cmp ebx, 0x6f6e614e",
    "jne .Lbasic_fail",
    "cmp ecx, 0x4d4d5678",
    "jne .Lbasic_fail",
    "mov eax, 1",
    "xor ecx, ecx",
    "cpuid",
    "bt ecx, 31",
    "jnc .Lbasic_fail",
    "mov eax, 0x80000001",
    "xor ecx, ecx",
    "cpuid",
    "bt ecx, 2",
    "jc .Lbasic_fail",
    "mov rax, 0x5a5aa5a512345678",
    "mov qword ptr [0x5000], rax",
    "mov ecx, 0x277",
    "rdmsr",
    "wrmsr",
    "mov dx, 0xf4",
    "mov eax, 0x10",
    "out dx, eax",
    "hlt",
    ".Lbasic_fail:",
    "mov dx, 0xf4",
    "mov eax, 0x11",
    "out dx, eax",
    "hlt",
    ".Lbasic_msg:",
    ".asciz \"NANOX:GUEST:HELLO\\n\"",
    "nanox_guest_basic_end:",
    // FAIL verdict.
    ".global nanox_guest_fail",
    ".global nanox_guest_fail_end",
    "nanox_guest_fail:",
    "mov dx, 0xf4",
    "mov eax, 0x11",
    "out dx, eax",
    "hlt",
    "nanox_guest_fail_end:",
    // Read beyond guest RAM (mapped by the guest's page tables, not by the
    // nested tables).
    ".global nanox_guest_npf_absent",
    ".global nanox_guest_npf_absent_end",
    "nanox_guest_npf_absent:",
    "mov al, byte ptr [0x100000]",
    "hlt",
    "nanox_guest_npf_absent_end:",
    // Write to the page the nested tables map read-only.
    ".global nanox_guest_npf_ro",
    ".global nanox_guest_npf_ro_end",
    "nanox_guest_npf_ro:",
    "mov al, byte ptr [0x6000]",
    "mov byte ptr [0x6000], 1",
    "hlt",
    "nanox_guest_npf_ro_end:",
    // #UD with no usable IDT: triple fault, SHUTDOWN.
    ".global nanox_guest_ud2",
    ".global nanox_guest_ud2_end",
    "nanox_guest_ud2:",
    "ud2",
    "nanox_guest_ud2_end:",
    ".global nanox_guest_hlt",
    ".global nanox_guest_hlt_end",
    "nanox_guest_hlt:",
    "hlt",
    "nanox_guest_hlt_end:",
    // IA32_TSC is outside the MSR allowlist and not emulated: #GP, then
    // triple fault.
    ".global nanox_guest_msr_denied",
    ".global nanox_guest_msr_denied_end",
    "nanox_guest_msr_denied:",
    "mov ecx, 0x10",
    "rdmsr",
    "mov dx, 0xf4",
    "mov eax, 0x10",
    "out dx, eax",
    "hlt",
    "nanox_guest_msr_denied_end:",
    // VMMCALL: #UD, then triple fault.
    ".global nanox_guest_vmmcall",
    ".global nanox_guest_vmmcall_end",
    "nanox_guest_vmmcall:",
    ".byte 0x0f, 0x01, 0xd9",
    "mov dx, 0xf4",
    "mov eax, 0x10",
    "out dx, eax",
    "hlt",
    "nanox_guest_vmmcall_end:",
    // Print the byte at 0x9000, stop; the host remaps the page; print it
    // again, stop.
    ".global nanox_guest_remap",
    ".global nanox_guest_remap_end",
    "nanox_guest_remap:",
    "mov dx, 0x3f8",
    "mov al, byte ptr [0x9000]",
    "out dx, al",
    "mov dx, 0xf4",
    "mov eax, 0x10",
    "out dx, eax",
    "mov dx, 0x3f8",
    "mov al, byte ptr [0x9000]",
    "out dx, al",
    "mov dx, 0xf4",
    "mov eax, 0x10",
    "out dx, eax",
    "hlt",
    "nanox_guest_remap_end:",
);
