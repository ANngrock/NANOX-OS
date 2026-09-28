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
    // APIC_BASE is outside the MSR allowlist: #GP, then triple fault.
    ".global nanox_guest_msr_denied",
    ".global nanox_guest_msr_denied_end",
    "nanox_guest_msr_denied:",
    "mov ecx, 0x1b",
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
