//! Guest memory as the VMM sees it when it must emulate an instruction:
//! guest-virtual → guest-physical through the guest's own page tables
//! (4-level long-mode paging with 1 GiB and 2 MiB pages), and fetching the
//! instruction bytes at RIP. Physical reads go through
//! [`SvmCpu::read_guest_phys`], which the kernel implements over the nested
//! tables.

use crate::vmcb::{bits, ctl, save, Vmcb};
use crate::SvmCpu;

const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const PS: u64 = 1 << 7;
/// Architectural maximum instruction length.
pub const MAX_INSN: usize = 15;

fn read_u64<C: SvmCpu + ?Sized>(cpu: &mut C, gpa: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    cpu.read_guest_phys(gpa, &mut b)
        .then(|| u64::from_le_bytes(b))
}

/// Guest-physical address of `va`, or None if it is not mapped or the
/// guest is in a paging mode other than none or 4-level long mode.
pub fn translate<C: SvmCpu + ?Sized>(cpu: &mut C, vmcb: &Vmcb<'_>, va: u64) -> Option<u64> {
    if vmcb.read_u64(save::CR0) & bits::CR0_PG == 0 {
        return Some(va);
    }
    let long = vmcb.read_u64(save::EFER) & bits::EFER_LMA != 0;
    let la57 = vmcb.read_u64(save::CR4) & (1 << 12) != 0;
    if !long || la57 {
        return None;
    }
    let mut table = vmcb.read_u64(save::CR3) & ADDR;
    for level in (0..4u32).rev() {
        let shift = 12 + 9 * level;
        let e = read_u64(cpu, table + 8 * ((va >> shift) & 511))?;
        if e & 1 == 0 {
            return None;
        }
        if (1..=2).contains(&level) && e & PS != 0 {
            let mask = (1u64 << shift) - 1;
            return Some((e & ADDR & !mask) | (va & mask));
        }
        table = e & ADDR;
    }
    Some(table | (va & 0xFFF))
}

/// The instruction bytes at the guest's RIP: the processor's copy when it
/// saved one (decode assists, INSN_LEN != 0), else read through the guest
/// page tables. Returns how many bytes are valid (fewer than
/// [`MAX_INSN`] where the next page is not mapped).
pub fn fetch<C: SvmCpu + ?Sized>(cpu: &mut C, vmcb: &Vmcb<'_>, out: &mut [u8; MAX_INSN]) -> usize {
    let saved = usize::from(vmcb.read_u8(ctl::INSN_LEN) & 0xF);
    if saved > 0 {
        for (i, b) in out.iter_mut().enumerate().take(saved) {
            *b = vmcb.read_u8(ctl::INSN_BYTES + i);
        }
        return saved;
    }
    let rip = vmcb.rip();
    for (i, byte) in out.iter_mut().enumerate() {
        let va = rip.wrapping_add(i as u64);
        let Some(pa) = translate(cpu, vmcb, va) else {
            return i;
        };
        let mut b = [0u8];
        if !cpu.read_guest_phys(pa, &mut b) {
            return i;
        }
        *byte = b[0];
    }
    MAX_INSN
}
