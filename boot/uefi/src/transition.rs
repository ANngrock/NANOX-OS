//! A position-independent copied trampoline. No UEFI mapping is assumed after
//! CR3 changes; code, private stack, context, GDT and IDT have identity mappings.
use crate::{
    fatal,
    memory::{PageTables, Pages},
    uefi,
};
use boot_protocol::{HANDOFF_BASE, STACK_TOP};
use core::{
    arch::{asm, global_asm},
    ptr,
};

global_asm!(include_str!("transition.S"));

unsafe extern "C" {
    static nanox_transition_start: u8;
    static nanox_transition_end: u8;
    static nanox_transition_fault: u8;
    static nanox_transition_test: u8;
}

#[derive(Clone, Copy)]
pub struct Transition {
    pub code: Pages,
    pub data: Pages,
    pub stack: Pages,
}

impl Transition {
    pub fn prepare(
        bs: &uefi::BootServices,
        tables: &mut PageTables,
        entry: u64,
        test: bool,
    ) -> Self {
        let code = Pages::allocate(bs, 1, true);
        let data = Pages::allocate(bs, 2, false);
        let stack = Pages::allocate(bs, 2, false);
        let start = ptr::addr_of!(nanox_transition_start) as usize;
        let end = ptr::addr_of!(nanox_transition_end) as usize;
        let fault_offset = ptr::addr_of!(nanox_transition_fault) as usize - start;
        let test_offset = ptr::addr_of!(nanox_transition_test) as usize - start;
        if end <= start
            || end - start > 4096
            || fault_offset >= end - start
            || test_offset >= end - start
        {
            fatal("transition-size");
        }
        // Context: 0 CR3, 8 RSP, 16 RDI, 24 RIP, 32 GDTR(10),
        // 48 IDTR(10), 64 transition-stack-top. GDT at 128, IDT at 4096.
        // SAFETY: fresh allocations are exclusive and zero-filled, their
        // bounded offsets lie wholly inside the respective pages. Source
        // symbols bound one contiguous executable section with RIP-relative
        // internal references only; there are no external relocations in it.
        unsafe {
            ptr::copy_nonoverlapping(start as *const u8, code.base as *mut u8, end - start);
            *((code.base as *mut u8).add(test_offset)) = u8::from(test);
            let context = data.base as *mut u64;
            context.add(0).write(tables.pages.base);
            context.add(1).write(STACK_TOP);
            context.add(2).write(HANDOFF_BASE);
            context.add(3).write(entry);
            (data.base as *mut u16).add(16).write(23);
            ptr::write_unaligned((data.base + 34) as *mut u64, data.base + 128);
            (data.base as *mut u16).add(24).write(4095);
            ptr::write_unaligned((data.base + 50) as *mut u64, data.base + 4096);
            context.add(8).write(stack.base + stack.count as u64 * 4096);
            ((data.base + 128) as *mut u64)
                .add(1)
                .write(0x00af_9a00_0000_ffff);
            ((data.base + 128) as *mut u64)
                .add(2)
                .write(0x00cf_9200_0000_ffff);
            let handler = code.base + fault_offset as u64;
            for vector in 0..256u64 {
                let gate = (data.base + 4096 + vector * 16) as *mut u8;
                (gate as *mut u16).write(handler as u16);
                (gate.add(2) as *mut u16).write(8);
                gate.add(4).write(0); // IST=0; M1 adds independent exception stacks.
                gate.add(5).write(0x8e); // Present CPL0 interrupt gate.
                (gate.add(6) as *mut u16).write((handler >> 16) as u16);
                (gate.add(8) as *mut u32).write((handler >> 32) as u32);
            }
        }
        tables.map(code.base, code, false, true);
        tables.map(data.base, data, true, false);
        tables.map(stack.base, stack, true, false);
        Self { code, data, stack }
    }

    pub fn enter(self) -> ! {
        // SAFETY: prepared code and context are live and identity mapped in
        // both firmware and new tables. All allocations remain reserved. This
        // one-way jump obeys our RCX context ABI, switches stacks itself, and
        // cannot return. No Rust locals or firmware resources are used again.
        unsafe {
            asm!("jmp rax", in("rax") self.code.base, in("rcx") self.data.base, options(noreturn));
        }
    }
}

pub fn check_cpu() {
    // SAFETY: CPUID is available on the x86-64 target and touches no memory;
    // only the bootstrap CPU runs here, before handing off page tables.
    let supported = unsafe { core::arch::x86_64::__cpuid(0x8000_0000) };
    if supported.eax < 0x8000_0001 {
        fatal("cpu-no-extended-features");
    }
    // SAFETY: the preceding maximum-leaf check establishes this CPUID leaf.
    let features = unsafe { core::arch::x86_64::__cpuid(0x8000_0001) };
    if features.edx & (1 << 20) == 0 {
        fatal("cpu-no-nx");
    }
    let cr4: u64;
    // SAFETY: loader executes at CPL0. Reading CR4 changes no state.
    unsafe {
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    }
    if cr4 & (1 << 12) != 0 {
        fatal("five-level-paging-unsupported");
    }
}
