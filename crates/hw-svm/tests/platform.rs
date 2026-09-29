//! The VMM's platform devices on the scripted processor: IA32_APIC_BASE,
//! APIC MMIO emulated from the faulting instruction (fetched through the
//! guest page tables or supplied by decode assists), timer interrupts on
//! virtual time (HLT wake-up, the STI shadow and the interrupt window),
//! PIT channel 2 and the PIC masks.

mod common;

use common::*;
use hw_svm::vmm::{Vcpu, Verdict};

const APIC: u64 = 0xFEE0_0000;
const PASS: Verdict = Verdict::DebugExit {
    value: 0x10,
    status: 33,
};

/// mov dword [rax+off], imm32
fn store(off: u32, imm: u32, assist: bool) -> Step {
    let mut b = vec![0xC7, 0x80];
    b.extend_from_slice(&off.to_le_bytes());
    b.extend_from_slice(&imm.to_le_bytes());
    mmio(APIC + u64::from(off), &b, assist)
}

/// mov r32, [rcx+off] with the ModRM reg field `reg` (2 = EDX, 3 = EBX).
fn load(reg: u8, off: u32, assist: bool) -> Step {
    let mut b = vec![0x8B, 0x81 | reg << 3];
    b.extend_from_slice(&off.to_le_bytes());
    mmio(APIC + u64::from(off), &b, assist)
}

fn out(port: u16, value: u32) -> Step {
    Step::Out {
        port,
        size: 1,
        value,
    }
}

fn debug_exit() -> Step {
    out(0xF4, 0x10)
}

/// The guest's IF must not mask the host's interrupts: prepare() sets
/// V_INTR_MASKING, and the interrupt window keeps it.
#[test]
fn physical_interrupts_stay_under_host_control() {
    let mut rig = Rig::new(&[]);
    let mut serial = [0u8; 4];
    let v = Vcpu::new(rig.cfg, &mut serial);
    let mut vmcb = rig.vmcb();
    v.prepare(&mut vmcb);
    let masking = hw_svm::vmcb::vintr::V_INTR_MASKING;
    assert_eq!(vmcb.read_u64(hw_svm::vmcb::ctl::VINTR) & masking, masking);
}

#[test]
fn apic_base_msr_is_emulated() {
    let mut rig = Rig::new(&[
        Step::MsrEmulated {
            msr: 0x1B,
            write: false,
            value: 0,
        },
        Step::MsrEmulated {
            msr: 0x1B,
            write: true,
            value: 0xFEE0_0900,
        },
        // x2APIC is not offered: #GP.
        Step::Wrmsr {
            msr: 0x1B,
            value: 0xFEE0_0D00,
        },
        debug_exit(),
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let o = run(&mut rig, &mut v);
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.cpu.rdmsr_results, vec![0xFEE0_0900]);
    assert_eq!(o.msr_faults, 1);
    rig.cpu.assert_clean();
}

#[test]
fn apic_mmio_is_emulated_from_guest_memory_and_from_decode_assists() {
    for assist in [false, true] {
        let mut rig = Rig::new(&[
            store(0xF0, 0x1FF, assist),
            load(2, 0x30, assist),
            load(3, 0xF0, assist),
            debug_exit(),
        ]);
        if !assist {
            rig.place_code();
        }
        rig.gprs.rdx = 0xFFFF_FFFF_0000_0000; // a 32-bit load zero-extends
        let mut serial = [0u8; 4];
        let mut v = Vcpu::new(rig.cfg, &mut serial);
        let o = run(&mut rig, &mut v);
        assert_eq!(o.verdict, PASS, "assist={assist}");
        assert_eq!(rig.gprs.rdx, 0x0005_0014, "APIC version");
        assert_eq!(rig.gprs.rbx, 0x1FF, "SVR read back");
        assert_eq!(o.mmio, 3);
        rig.cpu.assert_clean();
    }
}

/// A read-modify-write of an APIC register (what NANOX M1's compiled SVR
/// update is): the register is read, combined, written back, and the
/// guest's flags follow the result.
#[test]
fn apic_read_modify_write_is_emulated() {
    let mut rig = Rig::new(&[
        // or dword [0xffffffff930010f0], 0x100  (SVR: 0xFF -> 0x1FF)
        mmio(
            APIC + 0xF0,
            &[0x81, 0x0C, 0x25, 0xF0, 0x10, 0x00, 0x93, 0x00, 0x01, 0, 0],
            true,
        ),
        load(3, 0xF0, true),
        debug_exit(),
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(run(&mut rig, &mut v).verdict, PASS);
    assert_eq!(rig.gprs.rbx, 0x1FF);
    let zf = 1 << 6;
    assert_eq!(rig.vmcb().rflags() & zf, 0, "non-zero result");
    rig.cpu.assert_clean();
}

#[test]
fn mmio_the_vmm_cannot_decode_is_reported() {
    // No decode assists and the code is not in guest memory.
    let mut rig = Rig::new(&[store(0xF0, 0x1FF, false), debug_exit()]);
    let rip = rig.cpu.rip_of(0);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(
        run(&mut rig, &mut v).verdict,
        Verdict::MmioUnsupported {
            gpa: APIC + 0xF0,
            rip
        }
    );
    // An instruction outside the decoded forms: add [rax], eax.
    let mut rig = Rig::new(&[mmio(APIC + 0x80, &[0x01, 0x00], true)]);
    let rip = rig.cpu.rip_of(0);
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(
        run(&mut rig, &mut v).verdict,
        Verdict::MmioUnsupported {
            gpa: APIC + 0x80,
            rip
        }
    );
}

#[test]
fn timer_interrupt_wakes_hlt_and_is_injected_once() {
    let mut rig = Rig::new(&[
        store(0xF0, 0x1FF, true),
        store(0x320, 0x40, true), // one-shot, vector 0x40
        store(0x380, 1000, true), // 2 us at bus/2
        Step::Sti,
        Step::Hlt,
        store(0xB0, 0, true), // EOI
        debug_exit(),
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let o = run(&mut rig, &mut v);
    assert_eq!(o.verdict, PASS);
    assert_eq!(o.irqs, 1);
    // Taken on the instruction after HLT, once the deadline was reached.
    assert_eq!(rig.cpu.interrupts, vec![(0x40, rig.cpu.rip_of(5))]);
    assert!(o.virtual_ns >= 3_000 + 2_000, "{}", o.virtual_ns);
    assert_eq!(v.lapic().pending(), None);
    rig.cpu.assert_clean();
}

#[test]
fn pending_interrupt_waits_for_if_and_the_sti_shadow() {
    let mut rig = Rig::new(&[
        store(0xF0, 0x1FF, true),
        store(0x320, 0x40, true),
        store(0x380, 1, true), // expires almost at once
        out(0x80, 0),          // IF=0: the interrupt must wait
        out(0x80, 0),
        Step::Sti,
        Step::Load { gpa: 0x5000 }, // in the STI shadow, no exit
        Step::Load { gpa: 0x5000 }, // the window opens here
        store(0xB0, 0, true),
        debug_exit(),
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let o = run(&mut rig, &mut v);
    assert_eq!(o.verdict, PASS);
    assert_eq!(rig.cpu.interrupts, vec![(0x40, rig.cpu.rip_of(7))]);
    rig.cpu.assert_clean();
}

/// An exit on the instruction right after STI (here a host interrupt)
/// reports the shadow: the pending interrupt must not be injected before
/// that instruction runs.
#[test]
fn no_interrupt_is_injected_in_the_sti_shadow() {
    let mut rig = Rig::new(&[
        store(0xF0, 0x1FF, true),
        store(0x320, 0x40, true),
        store(0x380, 1, true),
        out(0x80, 0), // the timer is pending, IF=0
        Step::Sti,
        Step::Tick, // exits in the shadow
        Step::Load { gpa: 0x5000 },
        store(0xB0, 0, true),
        debug_exit(),
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(run(&mut rig, &mut v).verdict, PASS);
    assert_eq!(rig.cpu.interrupts, vec![(0x40, rig.cpu.rip_of(6))]);
    rig.cpu.assert_clean();
}

/// A guest spinning on PAUSE (no I/O, no HLT) still advances virtual time
/// and takes its timer interrupt.
#[test]
fn pause_spin_advances_time_and_takes_the_timer() {
    let mut script = vec![
        store(0xF0, 0x1FF, true),
        store(0x320, 0x40, true),
        store(0x380, 1500, true), // 3 us at bus/2
        Step::Sti,
    ];
    script.extend([Step::Pause; 6]);
    script.push(store(0xB0, 0, true));
    script.push(debug_exit());
    let mut rig = Rig::new(&script);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let o = run(&mut rig, &mut v);
    assert_eq!(o.verdict, PASS);
    assert_eq!(o.irqs, 1);
    // Set at 3 us, due at 6 us: after the third PAUSE exit.
    assert_eq!(rig.cpu.interrupts, vec![(0x40, rig.cpu.rip_of(7))]);
    rig.cpu.assert_clean();
}

/// A guest spinning without exits only leaves on host interrupts; each is
/// charged `intr_exit_ns`, so its timer still fires.
#[test]
fn host_interrupt_exits_carry_their_own_time_charge() {
    let mut rig = Rig::new(&[
        store(0xF0, 0x1FF, true),
        store(0x320, 0x40, true),
        store(0x380, 500_000, true), // 1 ms at bus/2
        Step::Sti,
        Step::Tick, // host interrupts while the guest spins
        Step::Tick,
        store(0xB0, 0, true),
        debug_exit(),
    ]);
    rig.cfg.intr_exit_ns = 600_000;
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let o = run(&mut rig, &mut v);
    assert_eq!(o.verdict, PASS);
    assert_eq!(o.irqs, 1, "1.2 ms of host ticks cover the 1 ms timer");
    assert_eq!(o.virtual_ns, 5 * 1_000 + 2 * 600_000);
    rig.cpu.assert_clean();
}

#[test]
fn hlt_without_a_wake_up_source_is_final() {
    for script in [
        vec![Step::Sti, Step::Hlt], // no timer
        vec![
            store(0xF0, 0x1FF, true),
            store(0x320, 0x40, true),
            store(0x380, 10, true),
            Step::Hlt,
        ], // IF=0
    ] {
        let mut rig = Rig::new(&script);
        let mut serial = [0u8; 4];
        let mut v = Vcpu::new(rig.cfg, &mut serial);
        assert_eq!(run(&mut rig, &mut v).verdict, Verdict::Halted);
        rig.cpu.assert_clean();
    }
}

#[test]
fn idle_guest_with_a_periodic_timer_hits_the_virtual_time_budget() {
    let mut rig = Rig::new(&[
        store(0xF0, 0x1FF, true),
        store(0x320, 0x2_0040, true), // periodic
        store(0x380, 500_000, true),  // 1 ms at bus/2
        Step::Sti,
        Step::SpinForever,
    ]);
    rig.cfg.max_virtual_ns = 5_000_000;
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let o = run(&mut rig, &mut v);
    assert_eq!(o.verdict, Verdict::VirtualTimeout);
    // The script's handler never sends EOI: vector 0x40 stays in service
    // and blocks its own class, so later periods merge in IRR.
    assert_eq!(o.irqs, 1);
    assert_eq!(rig.cpu.interrupts.len(), 1);
    // Expirations at ~1, 2, 3, 4 ms: the first delivered, the second
    // pending in IRR, the third and fourth merged into it.
    assert_eq!(v.lapic().coalesced, 2);
    assert_eq!(
        v.lapic().pending(),
        None,
        "blocked by the in-service vector"
    );
    rig.cpu.assert_clean();
}

#[test]
fn pit_channel2_and_pic_masks_through_ports() {
    let mut rig = Rig::new(&[
        Step::In {
            port: 0x21,
            size: 1,
            expect: 0xFF,
        },
        out(0x61, 0),
        out(0x43, 0xB0),
        out(0x42, 3),
        out(0x42, 0),
        out(0x61, 1),
        Step::In {
            port: 0x61,
            size: 1,
            expect: 0x01, // counting, OUT2 low
        },
        out(0x80, 0),
        out(0x80, 0),
        out(0x80, 0),
        Step::In {
            port: 0x61,
            size: 1,
            expect: 0x21, // 5 us > 4 clocks: OUT2 high
        },
        debug_exit(),
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(run(&mut rig, &mut v).verdict, PASS);
    rig.cpu.assert_clean();
}
