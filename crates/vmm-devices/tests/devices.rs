use vmm_devices::decode::{decode, merge, source_value, DecodeError, Operation, Reg, Source};
use vmm_devices::lapic::{self, reg, Lapic, LVT_MASKED, LVT_PERIODIC, SVR_ENABLE};
use vmm_devices::pit::{self, Pit};

fn r(index: u8) -> Reg {
    Reg {
        index,
        high8: false,
    }
}

// ---- decode -----------------------------------------------------------------

#[test]
fn decodes_the_mmio_forms() {
    use Operation::*;
    let load = |reg, size| Load {
        reg,
        size,
        dest: size,
    };
    let store = |reg, size| Store {
        src: Source::Reg(reg),
        size,
    };
    let cases: &[(&[u8], Operation, u8)] = &[
        // mov eax, [rcx+0x390]
        (&[0x8B, 0x81, 0x90, 0x03, 0, 0], load(r(0), 4), 6),
        // mov [rax+0xb0], ecx
        (&[0x89, 0x88, 0xB0, 0, 0, 0], store(r(1), 4), 6),
        // mov r8d, [rdi]
        (&[0x44, 0x8B, 0x07], load(r(8), 4), 3),
        // mov rax, [rip+0x10]
        (&[0x48, 0x8B, 0x05, 0x10, 0, 0, 0], load(r(0), 8), 7),
        // mov eax, [rsp+8]  (SIB, disp8)
        (&[0x8B, 0x44, 0x24, 0x08], load(r(0), 4), 4),
        // mov eax, [rax*4+0x1000]  (SIB, no base, disp32)
        (&[0x8B, 0x04, 0x85, 0x00, 0x10, 0, 0], load(r(0), 4), 7),
        // mov [rax], cx
        (&[0x66, 0x89, 0x08], store(r(1), 2), 3),
        // mov r15d, [r13+0]  (REX.RB, rm=5 with mod=1)
        (&[0x45, 0x8B, 0x7D, 0x00], load(r(15), 4), 4),
        // mov al, [rbx]
        (&[0x8A, 0x03], load(r(0), 1), 2),
        // mov ah, [rbx]
        (
            &[0x8A, 0x23],
            load(
                Reg {
                    index: 0,
                    high8: true,
                },
                1,
            ),
            2,
        ),
        // mov spl, [rbx]  (REX: no high-byte registers)
        (&[0x40, 0x8A, 0x23], load(r(4), 1), 3),
        // mov byte [rdx], dl
        (&[0x88, 0x12], store(r(2), 1), 2),
        // movzx eax, byte [rdx]
        (
            &[0x0F, 0xB6, 0x02],
            Load {
                reg: r(0),
                size: 1,
                dest: 4,
            },
            3,
        ),
        // movzx ecx, word [rsi+4]
        (
            &[0x0F, 0xB7, 0x4E, 0x04],
            Load {
                reg: r(1),
                size: 2,
                dest: 4,
            },
            4,
        ),
        // mov dword [rax], 0x12345678
        (
            &[0xC7, 0x00, 0x78, 0x56, 0x34, 0x12],
            Store {
                src: Source::Imm(0x1234_5678),
                size: 4,
            },
            6,
        ),
        // mov qword [rax], -1  (imm32 sign-extended)
        (
            &[0x48, 0xC7, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
            Store {
                src: Source::Imm(u64::MAX),
                size: 8,
            },
            7,
        ),
        // mov word [rax+2], 0xBEEF
        (
            &[0x66, 0xC7, 0x40, 0x02, 0xEF, 0xBE],
            Store {
                src: Source::Imm(0xBEEF),
                size: 2,
            },
            6,
        ),
        // mov byte [rax], 7
        (
            &[0xC6, 0x00, 0x07],
            Store {
                src: Source::Imm(7),
                size: 1,
            },
            3,
        ),
        // ds: segment override, then mov eax, [rcx]
        (&[0x3E, 0x8B, 0x01], load(r(0), 4), 3),
    ];
    for (bytes, op, len) in cases {
        let mut padded = bytes.to_vec();
        padded.extend_from_slice(&[0x90; 8]); // what follows must not matter
        assert_eq!(
            decode(&padded),
            Ok(vmm_devices::decode::Insn { op: *op, len: *len }),
            "{bytes:02x?}"
        );
        // Exactly the instruction's bytes suffice; one fewer does not.
        assert!(decode(bytes).is_ok(), "{bytes:02x?}");
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Truncated),
            "{bytes:02x?}"
        );
    }
}

#[test]
fn decodes_logical_read_modify_write() {
    use vmm_devices::decode::{Alu, Insn};
    let rmw = |alu, src, size| Operation::Rmw { alu, src, size };
    let cases: &[(&[u8], Operation, u8)] = &[
        // or dword [0xffffffff930010f0], 0x1ff — emitted in NANOX M1 for
        // `write_apic(SVR, (read_apic(SVR) & !0x1ff) | 0x1ff)`.
        (
            &[0x81, 0x0C, 0x25, 0xF0, 0x10, 0x00, 0x93, 0xFF, 0x01, 0, 0],
            rmw(Alu::Or, Source::Imm(0x1FF), 4),
            11,
        ),
        // or dword [rax+0x10], 1
        (
            &[0x83, 0x48, 0x10, 0x01],
            rmw(Alu::Or, Source::Imm(1), 4),
            4,
        ),
        // and dword [rax+0x10], -2  (imm8 sign-extended)
        (
            &[0x83, 0x60, 0x10, 0xFE],
            rmw(Alu::And, Source::Imm(0xFFFF_FFFF_FFFF_FFFE), 4),
            4,
        ),
        // or [rax], ecx
        (&[0x09, 0x08], rmw(Alu::Or, Source::Reg(r(1)), 4), 2),
        // and [rax], dl
        (&[0x20, 0x10], rmw(Alu::And, Source::Reg(r(2)), 1), 2),
        // xor word [rax], 0x1234
        (
            &[0x66, 0x81, 0x30, 0x34, 0x12],
            rmw(Alu::Xor, Source::Imm(0x1234), 2),
            5,
        ),
        // or byte [rax], 1
        (&[0x80, 0x08, 0x01], rmw(Alu::Or, Source::Imm(1), 1), 3),
    ];
    for (bytes, op, len) in cases {
        assert_eq!(
            decode(bytes),
            Ok(Insn { op: *op, len: *len }),
            "{bytes:02x?}"
        );
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Truncated),
            "{bytes:02x?}"
        );
    }
    // ADD (/0) and SUB (/5) are refused.
    assert_eq!(
        decode(&[0x81, 0x00, 1, 0, 0, 0]),
        Err(DecodeError::Unsupported)
    );
    assert_eq!(decode(&[0x83, 0x28, 0x01]), Err(DecodeError::Unsupported));
}

#[test]
fn logical_operations_set_flags_like_the_processor() {
    use vmm_devices::decode::Alu;
    const CF: u64 = 1;
    const PF: u64 = 1 << 2;
    const ZF: u64 = 1 << 6;
    const SF: u64 = 1 << 7;
    const OF: u64 = 1 << 11;
    let all = CF | PF | ZF | SF | OF | 1 << 9 | 2; // IF and bit 1 survive
    assert_eq!(Alu::Or.apply(0xFF, 0x100, 4, all), (0x1FF, PF | 1 << 9 | 2));
    assert_eq!(Alu::And.apply(1, 2, 4, 2), (0, ZF | PF | 2));
    assert_eq!(Alu::Xor.apply(0x80, 0, 1, 2), (0x80, SF | 2));
    assert_eq!(
        Alu::Or.apply(0x1_0000_0001, 0, 4, 2),
        (1, 2),
        "masked to 32 bits"
    );
    assert_eq!(
        Alu::Or.apply(1 << 63, 0, 8, 2),
        (1 << 63, SF | PF | 2),
        "64-bit sign"
    );
}

#[test]
fn decodes_compare_and_test() {
    use vmm_devices::decode::{FlagOp, Insn};
    let f = |op, src, size, mem_first| Operation::Flags {
        op,
        src,
        size,
        mem_first,
    };
    let cases: &[(&[u8], Operation, u8)] = &[
        // cmp dword [rbp+0x390], 0 — NANOX M1 polling the APIC current count.
        (
            &[0x83, 0xBD, 0x90, 0x03, 0, 0, 0x00],
            f(FlagOp::Cmp, Source::Imm(0), 4, true),
            7,
        ),
        // cmp [rax], ecx / cmp ecx, [rax]
        (&[0x39, 0x08], f(FlagOp::Cmp, Source::Reg(r(1)), 4, true), 2),
        (
            &[0x3B, 0x08],
            f(FlagOp::Cmp, Source::Reg(r(1)), 4, false),
            2,
        ),
        // cmp dword [rax], 0x12345678
        (
            &[0x81, 0x38, 0x78, 0x56, 0x34, 0x12],
            f(FlagOp::Cmp, Source::Imm(0x1234_5678), 4, true),
            6,
        ),
        // test [rax], ecx ; test byte [rax], 1 ; test dword [rax], 0x10000
        (
            &[0x85, 0x08],
            f(FlagOp::Test, Source::Reg(r(1)), 4, true),
            2,
        ),
        (
            &[0xF6, 0x00, 0x01],
            f(FlagOp::Test, Source::Imm(1), 1, true),
            3,
        ),
        (
            &[0xF7, 0x00, 0, 0, 1, 0],
            f(FlagOp::Test, Source::Imm(0x1_0000), 4, true),
            6,
        ),
    ];
    for (bytes, op, len) in cases {
        assert_eq!(
            decode(bytes),
            Ok(Insn { op: *op, len: *len }),
            "{bytes:02x?}"
        );
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Truncated),
            "{bytes:02x?}"
        );
    }
    // NOT/NEG (F7 /2, /3) are not tests.
    assert_eq!(decode(&[0xF7, 0x10]), Err(DecodeError::Unsupported));
}

#[test]
fn compare_flags_follow_subtraction() {
    use vmm_devices::decode::FlagOp;
    const CF: u64 = 1;
    const PF: u64 = 1 << 2;
    const AF: u64 = 1 << 4;
    const ZF: u64 = 1 << 6;
    const SF: u64 = 1 << 7;
    const OF: u64 = 1 << 11;
    let base = 2 | 1 << 9;
    // 7 - 7 = 0
    assert_eq!(FlagOp::Cmp.flags(7, 7, 4, base), base | ZF | PF);
    // 5 - 7 = -2: borrow, negative, AF from bit 3
    assert_eq!(FlagOp::Cmp.flags(5, 7, 4, base), base | CF | SF | AF);
    // 0x8000_0000 - 1: signed overflow in 32 bits
    assert_eq!(
        FlagOp::Cmp.flags(0x8000_0000, 1, 4, base),
        base | OF | PF | AF
    );
    // Upper bits beyond the operand size are ignored.
    assert_eq!(FlagOp::Cmp.flags(0x1_0000_0003, 3, 4, base), base | ZF | PF);
    // TEST clears CF/OF even if set before.
    assert_eq!(FlagOp::Test.flags(0x10, 0x10, 4, base | CF | OF), base);
    assert_eq!(FlagOp::Test.flags(0x10, 0x01, 4, base), base | ZF | PF);
}

#[test]
fn refuses_what_it_does_not_emulate() {
    assert_eq!(decode(&[0x8B, 0xC0]), Err(DecodeError::RegisterOperand));
    assert_eq!(decode(&[0x0F, 0x10, 0x00]), Err(DecodeError::Unsupported)); // movups
    assert_eq!(decode(&[0xF0, 0x89, 0x08]), Err(DecodeError::Unsupported)); // lock
    assert_eq!(decode(&[0xF3, 0xA5]), Err(DecodeError::Unsupported)); // rep movs
    assert_eq!(
        decode(&[0xC7, 0x08, 0, 0, 0, 0]),
        Err(DecodeError::Unsupported)
    ); // C7 /1
    assert_eq!(decode(&[0x01, 0x08]), Err(DecodeError::Unsupported)); // add r/m, reg
    assert_eq!(decode(&[0x66; 16]), Err(DecodeError::TooLong));
    assert_eq!(decode(&[]), Err(DecodeError::Truncated));
}

#[test]
fn register_merge_rules() {
    let old = 0x1122_3344_5566_7788;
    assert_eq!(
        merge(old, r(0), 4, 0xAABB_CCDD),
        0xAABB_CCDD,
        "32-bit zero-extends"
    );
    assert_eq!(merge(old, r(0), 2, 0xAABB), 0x1122_3344_5566_AABB);
    assert_eq!(merge(old, r(0), 1, 0xAA), 0x1122_3344_5566_77AA);
    let ah = Reg {
        index: 0,
        high8: true,
    };
    assert_eq!(merge(old, ah, 1, 0xAA), 0x1122_3344_5566_AA88);
    assert_eq!(merge(old, r(0), 8, 5), 5);
    assert_eq!(source_value(old, ah), 0x77);
    assert_eq!(source_value(old, r(0)), old);
}

// ---- LAPIC ------------------------------------------------------------------

const BUS: u64 = 1_000_000_000;

#[test]
fn reset_state_and_base_msr() {
    let mut a = Lapic::new(BUS);
    assert_eq!(a.read(reg::VERSION, 0), lapic::VERSION);
    assert_eq!(a.read(reg::SVR, 0), 0xFF);
    assert_eq!(a.read(reg::LVT_TIMER, 0), LVT_MASKED);
    assert_eq!(a.read(reg::DFR, 0), 0xFFFF_FFFF);
    assert_eq!(a.base(), lapic::DEFAULT_BASE);
    let msr = a.read_msr();
    assert_eq!(msr, 0xFEE0_0900);
    assert!(a.write_msr(msr & !lapic::MSR_ENABLE).is_ok());
    assert!(a.write_msr(msr).is_ok());
    assert!(a.write_msr(msr | lapic::MSR_X2APIC).is_err(), "no x2APIC");
    assert!(a.write_msr(0xFEC0_0900).is_err(), "no relocation");
    assert!(a.write_msr(msr & !lapic::MSR_BSP).is_err());
}

fn enabled() -> Lapic {
    let mut a = Lapic::new(BUS);
    a.write(reg::SVR, 0x1FF, 0);
    a.write(reg::TIMER_DIVIDE, 3, 0); // /16: 62.5 MHz
    a
}

#[test]
fn one_shot_counts_down_and_fires_once() {
    let mut a = enabled();
    a.write(reg::LVT_TIMER, 0x40, 0);
    a.write(reg::TIMER_INITIAL, 1000, 0); // 16 us
    assert_eq!(a.read(reg::TIMER_CURRENT, 0), 1000);
    assert_eq!(a.read(reg::TIMER_CURRENT, 8_000), 500);
    assert_eq!(a.next_deadline(), Some(16_000));
    assert_eq!(a.pending(), None);
    assert_eq!(a.read(reg::TIMER_CURRENT, 16_000), 0);
    assert_eq!(a.pending(), Some(0x40));
    assert_eq!(a.read(reg::IRR + 0x20, 16_000), 1, "vector 0x40 in IRR");
    a.accept(0x40);
    assert_eq!(a.pending(), None);
    assert_eq!(a.read(reg::ISR + 0x20, 16_000), 1);
    a.write(reg::EOI, 0, 16_000);
    assert_eq!(a.read(reg::ISR + 0x20, 16_000), 0);
    a.update(1_000_000);
    assert_eq!(a.pending(), None, "one-shot fires once");
    assert_eq!(a.next_deadline(), None);
}

#[test]
fn periodic_reloads_and_coalesces_missed_expirations() {
    let mut a = enabled();
    a.write(reg::LVT_TIMER, 0x40 | LVT_PERIODIC, 0);
    a.write(reg::TIMER_INITIAL, 625_000, 0); // 10 ms
    assert_eq!(a.next_deadline(), Some(10_000_000));
    a.update(10_000_000);
    assert_eq!(a.pending(), Some(0x40));
    a.accept(0x40);
    a.write(reg::EOI, 0, 10_000_000);
    assert_eq!(a.next_deadline(), Some(20_000_000));
    assert_eq!(a.read(reg::TIMER_CURRENT, 15_000_000), 312_500);
    // Three more periods pass without the guest taking the interrupt.
    a.update(40_000_000);
    assert_eq!(a.pending(), Some(0x40));
    assert_eq!(a.coalesced, 2);
    assert_eq!(a.next_deadline(), Some(50_000_000));
}

#[test]
fn masked_timer_and_software_disable_deliver_nothing() {
    let mut a = enabled();
    a.write(reg::LVT_TIMER, 0x40 | LVT_MASKED, 0);
    a.write(reg::TIMER_INITIAL, 100, 0);
    a.update(1_000_000);
    assert_eq!(a.pending(), None);
    assert_eq!(a.read(reg::IRR + 0x20, 1_000_000), 0, "masked: no IRR");
    assert_eq!(a.next_deadline(), None);
    // SVR bit 8 clear masks the LVT and cannot be overridden.
    let mut b = Lapic::new(BUS);
    b.write(reg::LVT_TIMER, 0x40, 0);
    assert_eq!(b.read(reg::LVT_TIMER, 0), 0x40 | LVT_MASKED);
    b.write(reg::SVR, SVR_ENABLE | 0xFF, 0);
    b.write(reg::LVT_TIMER, 0x40, 0);
    b.write(reg::SVR, 0xFF, 0);
    assert_eq!(b.read(reg::LVT_TIMER, 0) & LVT_MASKED, LVT_MASKED);
}

#[test]
fn priority_tpr_isr_and_eoi_order() {
    let mut a = enabled();
    a.write(reg::LVT_TIMER, 0x40, 0);
    a.write(reg::TIMER_INITIAL, 1, 0);
    a.update(1_000);
    a.write(reg::TPR, 0x40, 1_000);
    assert_eq!(a.pending(), None, "TPR class 4 blocks vector 0x40");
    assert_eq!(a.read(reg::PPR, 1_000), 0x40);
    a.write(reg::TPR, 0x30, 1_000);
    assert_eq!(a.pending(), Some(0x40));
    a.accept(0x40);
    // A second timer interrupt of the same class waits for the EOI.
    a.write(reg::TIMER_INITIAL, 1, 1_000);
    a.update(2_000);
    assert_eq!(a.pending(), None, "same class as the in-service vector");
    a.write(reg::EOI, 0, 2_000);
    assert_eq!(a.pending(), Some(0x40));
}

#[test]
fn divide_change_keeps_the_count_continuous() {
    let mut a = enabled();
    a.write(reg::LVT_TIMER, 0x40 | LVT_MASKED, 0);
    a.write(reg::TIMER_INITIAL, 1_000_000, 0);
    let before = a.read(reg::TIMER_CURRENT, 1_000_000); // 62_500 counts done
    assert_eq!(before, 937_500);
    a.write(reg::TIMER_DIVIDE, 0xB, 1_000_000); // /1: 1 GHz
    let after = a.read(reg::TIMER_CURRENT, 1_000_000);
    assert!(before.abs_diff(after) <= 1, "{before} {after}");
    assert_eq!(
        a.read(reg::TIMER_CURRENT, 1_001_000)
            .abs_diff(after - 1_000),
        0
    );
    for (d, div) in [
        (0, 2),
        (1, 4),
        (2, 8),
        (3, 16),
        (8, 32),
        (9, 64),
        (0xA, 128),
        (0xB, 1),
    ] {
        let mut b = Lapic::new(BUS);
        b.write(reg::TIMER_DIVIDE, d, 0);
        b.write(reg::TIMER_INITIAL, u32::MAX, 0);
        let counted = u32::MAX - b.read(reg::TIMER_CURRENT, 1_000_000);
        assert_eq!(u64::from(counted), 1_000_000 / div, "divide {d:#x}");
    }
}

// ---- PIT --------------------------------------------------------------------

#[test]
fn pit_mode0_out2_and_gate() {
    let mut p = Pit::new();
    assert_eq!(p.read(pit::PORT_SPEAKER, 0), Some(0));
    p.write(pit::PORT_SPEAKER, 0, 0);
    p.write(pit::PORT_CONTROL, pit::CONTROL_MODE0, 0);
    p.write(pit::PORT_CHANNEL2, 100, 0);
    p.write(pit::PORT_CHANNEL2, 0, 0); // count 100, gate still closed
    assert!(!p.out2(1_000_000), "gate closed: no counting");
    p.write(pit::PORT_SPEAKER, 1, 1_000_000);
    // 101 clocks of 1.193182 MHz = 84.6476 us, rounded up to whole ns.
    let done = p.out2_deadline().unwrap();
    assert_eq!(done, 1_000_000 + 84_648);
    assert!(!p.out2(done - 1));
    assert!(p.out2(done));
    assert_eq!(p.read(pit::PORT_SPEAKER, done), Some(0x21));
    // Closing the gate pauses the count.
    let mut q = Pit::new();
    q.write(pit::PORT_CONTROL, pit::CONTROL_MODE0, 0);
    q.write(pit::PORT_CHANNEL2, 100, 0);
    q.write(pit::PORT_CHANNEL2, 0, 0);
    q.write(pit::PORT_SPEAKER, 1, 0);
    q.write(pit::PORT_SPEAKER, 0, 50_000);
    assert!(!q.out2(10_000_000));
    q.write(pit::PORT_SPEAKER, 1, 10_000_000);
    assert!(!q.out2(10_000_000 + 30_000));
    assert!(q.out2(10_000_000 + 40_000));
    assert_eq!(q.unsupported, 0);
}

/// What this model used to refuse (other modes on channel 2, count reads,
/// channel 0) is the 8254 now; the full coverage is in tests/pit.rs.
#[test]
fn pit_channel2_square_wave_count_reads_and_channel0_ports() {
    let mut p = Pit::new();
    p.write(pit::PORT_CONTROL, 0xB6, 0); // channel 2, mode 3 (square wave)
    p.write(pit::PORT_CHANNEL2, 4, 0);
    p.write(pit::PORT_CHANNEL2, 0, 0);
    assert!(
        p.out2(1_000_000_000),
        "gate closed: mode 3 stopped, OUT high"
    );
    p.write(pit::PORT_SPEAKER, 1, 0);
    // Count 4: clocks at 839, 1677, 2515, 3353, 4191 ns; high for clocks 1-2, low for 3-4.
    assert!(p.out2(2_514));
    assert!(!p.out2(2_515));
    assert!(!p.out2(4_190));
    assert!(p.out2(4_191));
    assert_eq!(p.read(pit::PORT_SPEAKER, 2_515), Some(0x01));
    // the counter goes down by two in each half
    assert_eq!(p.read(pit::PORT_CHANNEL2, 1_677), Some(2));
    assert_eq!(p.read(pit::PORT_CHANNEL2, 1_677), Some(0));
    assert_eq!(p.read(pit::PORT_CHANNEL2, 2_515), Some(4));
    assert_eq!(p.unsupported, 0);
    assert!(p.write(0x40, 0, 0), "channel 0 is modeled");
    assert!(p.read(0x40, 0).is_some());
    assert_eq!(
        p.read(pit::PORT_CONTROL, 0),
        Some(0),
        "the control port reads nothing"
    );
    assert_eq!(p.unsupported, 1);
    assert!(!p.write(0x44, 0, 0));
    assert_eq!(p.read(0x60, 0), None);
}

// ---- the NANOX M1 calibration, access by access ------------------------------

/// A guest's view: every device access is one VM exit, and the VMM
/// advances virtual time by a fixed quantum per exit.
struct Machine {
    now: u64,
    quantum: u64,
    lapic: Lapic,
    pit: Pit,
    ticks: u64,
    irq_enabled: bool,
}

impl Machine {
    fn exit(&mut self) {
        self.now += self.quantum;
        self.lapic.update(self.now);
        if self.irq_enabled {
            if let Some(v) = self.lapic.pending() {
                // The kernel's handler: count, EOI.
                self.lapic.accept(v);
                self.ticks += 1;
                self.lapic.write(reg::EOI, 0, self.now);
            }
        }
    }
    fn inb(&mut self, port: u16) -> u8 {
        self.exit();
        self.pit.read(port, self.now).unwrap()
    }
    fn outb(&mut self, port: u16, v: u8) {
        self.exit();
        assert!(self.pit.write(port, v, self.now));
    }
    fn read_apic(&mut self, off: u32) -> u32 {
        self.exit();
        self.lapic.read(off, self.now)
    }
    fn write_apic(&mut self, off: u32, v: u32) {
        self.exit();
        self.lapic.write(off, v, self.now);
    }
    fn wait_out2(&mut self, high: bool) -> bool {
        (0..10_000_000).any(|_| (self.inb(0x61) & 0x20 != 0) == high)
    }
    /// kernel/src/timer.rs measure_pit2 (codex/m1-m8-continuation).
    fn measure_pit2(&mut self, count: u16) -> (u32, u32, u64, u64) {
        let saved = self.inb(0x61);
        self.outb(0x61, saved & !1);
        self.outb(0x43, 0xB0);
        self.outb(0x42, count as u8);
        self.outb(0x42, (count >> 8) as u8);
        self.outb(0x61, (saved & !3) | 1);
        assert!(self.wait_out2(false), "PIT did not start");
        let initial = self.read_apic(reg::TIMER_CURRENT);
        let t0 = self.ticks;
        assert!(self.wait_out2(true), "PIT did not finish");
        let t1 = self.ticks;
        let fin = self.read_apic(reg::TIMER_CURRENT);
        self.outb(0x61, saved);
        (initial, fin, t0, t1)
    }
}

#[test]
fn m1_kernel_calibration_and_periodic_verification_pass() {
    const PIT_CALIBRATION_COUNT: u16 = 11_932;
    const PIT_VERIFY_COUNT: u16 = 59_659;
    let mut m = Machine {
        now: 0,
        quantum: 1_000, // 1 us per exit
        lapic: Lapic::new(BUS),
        pit: Pit::new(),
        ticks: 0,
        irq_enabled: false,
    };
    // finish_setup
    m.write_apic(reg::LVT_TIMER, 0x40 | LVT_MASKED);
    m.write_apic(reg::TPR, 0);
    let svr = m.read_apic(reg::SVR);
    m.write_apic(reg::SVR, (svr & !0x1FF) | 0xFF | 1 << 8);
    m.write_apic(reg::TIMER_DIVIDE, 3);
    m.write_apic(reg::TIMER_INITIAL, 0);
    m.write_apic(reg::TIMER_INITIAL, u32::MAX);
    let (initial, fin, _, _) = m.measure_pit2(PIT_CALIBRATION_COUNT);
    m.write_apic(reg::TIMER_INITIAL, 0);
    let delta = u64::from(initial - fin);
    let hz = delta * pit::HZ / u64::from(PIT_CALIBRATION_COUNT);
    assert!((10_000_000..=1_000_000_000).contains(&hz), "hz {hz}");
    assert!(
        hz.abs_diff(BUS / 16) < BUS / 16 / 200,
        "hz {hz} vs 62.5 MHz"
    );
    let reload = ((delta * pit::HZ + u64::from(PIT_CALIBRATION_COUNT) * 50)
        / (u64::from(PIT_CALIBRATION_COUNT) * 100)) as u32;

    // measure_periodic_irq, not suppressed.
    m.write_apic(reg::LVT_TIMER, 0x40 | LVT_PERIODIC | LVT_MASKED);
    m.write_apic(reg::TIMER_INITIAL, 0);
    m.write_apic(reg::TIMER_INITIAL, reload);
    m.write_apic(reg::LVT_TIMER, 0x40 | LVT_PERIODIC);
    // mask_pic writes 0x21/0xA1: two exits the VMM ignores (no PIC model).
    m.exit();
    m.exit();
    m.irq_enabled = true;
    let mut total = 0;
    for _ in 0..10 {
        let (_, _, t0, t1) = m.measure_pit2(PIT_VERIFY_COUNT);
        let per_window = t1 - t0;
        assert!((4..=6).contains(&per_window), "{per_window} ticks in 50 ms");
        total += per_window;
    }
    m.irq_enabled = false;
    assert!((45..=55).contains(&total), "{total} ticks in 500 ms");
    assert_eq!(m.lapic.coalesced, 0, "1 us quantum: nothing merged");
}
