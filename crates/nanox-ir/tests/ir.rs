//! Properties of NIR: the operation table, the verifier, the encoding, and
//! the optimizer against the plain interpreter.

use nanox_ir::opt::{alu_b, optimize, Compiled};
use nanox_ir::{
    alu, mask, sign_extend, verify, DecodeError, Insn, Op, Program, VerifyError, ALL_OPS, MAX_LEN,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn alu_ops() -> impl Iterator<Item = Op> {
    ALL_OPS.into_iter().filter(|o| o.is_alu())
}

#[test]
fn opcodes_are_dense_and_round_trip() {
    for (i, op) in ALL_OPS.iter().enumerate() {
        assert_eq!(*op as usize, i);
        assert_eq!(Op::from_u8(i as u8), Some(*op));
    }
    for b in ALL_OPS.len()..=255 {
        assert_eq!(Op::from_u8(b as u8), None);
    }
    let classes = ALL_OPS
        .iter()
        .map(|o| u8::from(o.is_binary()) + u8::from(o.is_unary()) + u8::from(o.is_jump()))
        .filter(|c| *c > 1)
        .count();
    assert_eq!(classes, 0, "an op is at most one of binary, unary, jump");
}

#[test]
fn every_8bit_operand_pair_agrees_between_a_and_b() {
    for op in alu_ops() {
        for x in 0..256u64 {
            for y in 0..256u64 {
                let (a, b) = (alu(op, x, y, 8), alu_b(op, x, y, 8));
                assert_eq!(a, b, "{op:?} {x} {y}");
                assert!(a < 256);
            }
        }
    }
}

#[test]
fn edge_and_random_64bit_operands_agree_between_a_and_b() {
    let edges = [
        0u64,
        1,
        2,
        63,
        64,
        65,
        0x7FFF_FFFF,
        0x8000_0000,
        u64::MAX >> 1,
        1 << 63,
        (1 << 63) | 1,
        u64::MAX - 1,
        u64::MAX,
    ];
    let mut rng = Rng(7);
    for op in alu_ops() {
        for &x in &edges {
            for &y in &edges {
                assert_eq!(
                    alu(op, x, y, 64),
                    alu_b(op, x, y, 64),
                    "{op:?} {x:#x} {y:#x}"
                );
            }
        }
        for _ in 0..20_000 {
            let (x, y) = (rng.next(), rng.next());
            assert_eq!(
                alu(op, x, y, 64),
                alu_b(op, x, y, 64),
                "{op:?} {x:#x} {y:#x}"
            );
        }
    }
}

#[test]
fn total_operations_have_the_documented_edge_results() {
    for f in [alu, alu_b] {
        assert_eq!(f(Op::Udiv, 5, 0, 8), 0xFF);
        assert_eq!(f(Op::Udiv, 5, 0, 64), u64::MAX);
        assert_eq!(f(Op::Urem, 5, 0, 8), 5);
        assert_eq!(f(Op::Urem, 0xFF, 0, 8), 0xFF);
        assert_eq!(f(Op::Shl, 1, 9, 8), 2, "shift is taken modulo the width");
        assert_eq!(f(Op::Shl, 1, 64, 64), 1);
        assert_eq!(f(Op::Shr, 0x80, 15, 8), 1);
        assert_eq!(f(Op::Sar, 0x80, 1, 8), 0xC0);
        assert_eq!(f(Op::Sar, 0x80, 7, 8), 0xFF);
        assert_eq!(f(Op::Sar, 0x7F, 7, 8), 0);
        assert_eq!(f(Op::Sar, 1 << 63, 63, 64), u64::MAX);
        assert_eq!(f(Op::Lts, 0x80, 0x7F, 8), 1, "-128 < 127");
        assert_eq!(f(Op::Ltu, 0x80, 0x7F, 8), 0);
        assert_eq!(f(Op::Les, 0x80, 0x80, 8), 1);
        assert_eq!(f(Op::Neg, 0x80, 0, 8), 0x80);
        assert_eq!(f(Op::Neg, 0, 0, 8), 0);
        assert_eq!(f(Op::Not, 0, 0, 8), 0xFF);
        assert_eq!(f(Op::Not, 0, 0, 64), u64::MAX);
        assert_eq!(f(Op::Add, 0xFF, 1, 8), 0);
        assert_eq!(f(Op::Sub, 0, 1, 8), 0xFF);
        assert_eq!(f(Op::Sub, 0, 1, 64), u64::MAX);
        assert_eq!(f(Op::Mul, 0x10, 0x10, 8), 0);
        assert_eq!(f(Op::Popcnt, 0xFF, 0, 8), 8);
        assert_eq!(f(Op::Popcnt, u64::MAX, 0, 64), 64);
        assert_eq!(f(Op::Minu, 3, 200, 8), 3);
        assert_eq!(f(Op::Maxu, 3, 200, 8), 200);
    }
    assert_eq!(sign_extend(0x80, 8), -128);
    assert_eq!(sign_extend(0x7F, 8), 127);
    assert_eq!(sign_extend(1 << 63, 64), i64::MIN);
    assert_eq!(mask(8), 0xFF);
    assert_eq!(mask(64), u64::MAX);
}

fn reg(rng: &mut Rng, nreg: u64) -> u8 {
    rng.below(nreg) as u8
}

/// A random verified program (by construction) of width `w`.
fn gen(rng: &mut Rng, w: u32) -> Vec<Insn> {
    let m = mask(w);
    let pool = [0, 1, 2, m, m - 1, m >> 1, (m >> 1) + 1, 3, 7];
    let n = 2 + rng.below(30) as usize;
    let nreg = 2 + rng.below(7);
    let ops: Vec<Op> = alu_ops().collect();
    let mut p = Vec::with_capacity(n);
    for pc in 0..n - 1 {
        let (d, a, b) = (reg(rng, nreg), reg(rng, nreg), reg(rng, nreg));
        p.push(match rng.below(20) {
            0..=10 => {
                let op = ops[rng.below(ops.len() as u64) as usize];
                if op.is_binary() {
                    Insn::binary(op, d, a, b)
                } else {
                    Insn::unary(op, d, a)
                }
            }
            11 | 12 => {
                let imm = if rng.below(2) == 0 {
                    pool[rng.below(pool.len() as u64) as usize]
                } else {
                    rng.next() & m
                };
                Insn::constant(d, imm)
            }
            13 | 14 => Insn::input(d, rng.below(5)),
            15 => Insn::sel(d, a, b, reg(rng, nreg)),
            _ => {
                let t = (pc + 1) as u64 + rng.below((n - 1 - pc) as u64);
                match rng.below(3) {
                    0 => Insn::jmp(t),
                    1 => Insn::jz(a, t),
                    _ => Insn::jnz(a, t),
                }
            }
        });
    }
    p.push(Insn::ret(reg(rng, nreg)));
    p
}

fn edge_inputs(rng: &mut Rng, w: u32) -> [u64; 3] {
    let m = mask(w);
    let pool = [
        0,
        1,
        2,
        3,
        m,
        m - 1,
        m >> 1,
        (m >> 1) + 1,
        0x80,
        0xFF,
        0x100,
    ];
    let mut v = [0; 3];
    for x in &mut v {
        *x = match rng.below(4) {
            0 | 1 => pool[rng.below(pool.len() as u64) as usize],
            2 => rng.next() & m,
            _ => rng.next(),
        };
    }
    v
}

struct Built {
    p: Program,
    opt: Program,
    b_plain: Compiled,
    b_opt: Compiled,
}

fn build(insns: &[Insn], w: u32) -> Built {
    let p = Program::new(insns, w).expect("generated programs verify");
    let opt = optimize(&p).expect("the optimizer output verifies");
    assert!(opt.insns().len() <= p.insns().len());
    Built {
        b_plain: Compiled::new(&p),
        b_opt: Compiled::new(&opt),
        p,
        opt,
    }
}

fn agree(b: &Built, inputs: &[u64], what: &str) {
    let a = b.p.eval(inputs);
    assert!(a.steps <= b.p.max_steps());
    let o = b.opt.eval(inputs);
    assert_eq!(o.value, a.value, "optimized program, {what} {inputs:?}");
    assert!(o.steps <= a.steps, "steps grew, {what}");
    let bp = b.b_plain.run(inputs);
    assert_eq!(
        (bp.value, bp.steps),
        (a.value, a.steps),
        "B, {what} {inputs:?}"
    );
    let bo = b.b_opt.run(inputs);
    assert_eq!(
        (bo.value, bo.steps),
        (o.value, o.steps),
        "B optimized, {what} {inputs:?}"
    );
}

#[test]
fn optimizer_and_both_evaluators_agree_on_all_8bit_inputs() {
    let mut rng = Rng(0xC0FFEE);
    for k in 0..40 {
        let insns = gen(&mut rng, 8);
        let b = build(&insns, 8);
        for x in 0..256u64 {
            for y in 0..256u64 {
                agree(&b, &[x, y], &format!("program {k}"));
            }
        }
    }
}

#[test]
fn optimizer_and_both_evaluators_agree_on_64bit_samples() {
    let mut rng = Rng(0xFEED_FACE);
    let mut shrunk = 0;
    for k in 0..400 {
        let insns = gen(&mut rng, 64);
        let b = build(&insns, 64);
        shrunk += usize::from(b.opt.insns().len() < b.p.insns().len());
        for _ in 0..40 {
            agree(&b, &edge_inputs(&mut rng, 64), &format!("program {k}"));
        }
    }
    assert!(
        shrunk > 100,
        "the optimizer should shrink many random programs: {shrunk}"
    );
}

#[test]
fn inputs_are_cut_to_the_width_and_missing_inputs_are_zero() {
    let p = Program::new(
        &[
            Insn::input(1, 0),
            Insn::input(2, 5),
            Insn::binary(Op::Add, 3, 1, 2),
            Insn::ret(3),
        ],
        8,
    )
    .unwrap();
    assert_eq!(p.eval(&[0x1_23]).value, 0x23);
    assert_eq!(p.eval(&[]).value, 0);
    let b = Compiled::new(&p);
    assert_eq!(b.run(&[0x1_23]).value, 0x23);
    assert_eq!(b.run(&[u64::MAX]).value, 0xFF);
}

fn v(prog: &[Insn], w: u32) -> Result<(), VerifyError> {
    verify(prog, w)
}

#[test]
fn verifier_rejects_each_kind_of_malformed_program() {
    let ret = Insn::ret(0);
    assert_eq!(v(&[ret], 16), Err(VerifyError::Width));
    assert_eq!(v(&[ret], 32), Err(VerifyError::Width));
    assert_eq!(v(&[], 8), Err(VerifyError::Empty));
    let long = vec![ret; MAX_LEN + 1];
    assert_eq!(v(&long, 8), Err(VerifyError::TooLong));
    assert_eq!(v(&long[..MAX_LEN], 8), Ok(()));
    assert_eq!(v(&[Insn::constant(0, 1)], 8), Err(VerifyError::NoFinalRet));
    assert_eq!(
        v(&[Insn::unary(Op::Mov, 16, 0), ret], 8),
        Err(VerifyError::Register(0))
    );
    assert_eq!(
        v(&[Insn::unary(Op::Mov, 0, 16), ret], 8),
        Err(VerifyError::Register(0))
    );
    assert_eq!(
        v(&[Insn::sel(0, 16, 0, 0), ret], 8),
        Err(VerifyError::Register(0))
    );
    assert_eq!(v(&[Insn::ret(16)], 8), Err(VerifyError::Register(0)));
    assert_eq!(v(&[Insn::input(0, 8), ret], 8), Err(VerifyError::Input(0)));
    assert_eq!(v(&[Insn::input(0, 7), ret], 8), Ok(()));
    assert_eq!(v(&[Insn::jmp(0), ret], 8), Err(VerifyError::JumpTarget(0)));
    assert_eq!(
        v(&[Insn::jz(0, 2), ret], 8),
        Err(VerifyError::JumpTarget(0))
    );
    assert_eq!(v(&[Insn::jnz(0, 1), ret], 8), Ok(()));
    assert_eq!(
        v(&[Insn::constant(0, 256), ret], 8),
        Err(VerifyError::Immediate(0))
    );
    assert_eq!(v(&[Insn::constant(0, 255), ret], 8), Ok(()));
    assert_eq!(v(&[Insn::constant(0, u64::MAX), ret], 64), Ok(()));
    let canon = [
        Insn::new(Op::Ret, 1, 0, 0, 0, 0),
        Insn::new(Op::Ret, 0, 0, 1, 0, 0),
        Insn::new(Op::Add, 0, 0, 0, 1, 0),
        Insn::new(Op::Mov, 0, 0, 3, 0, 0),
        Insn::new(Op::Add, 0, 0, 0, 0, 5),
        Insn::new(Op::Const, 0, 1, 0, 0, 0),
        Insn::new(Op::Jmp, 1, 0, 0, 0, 1),
        Insn::new(Op::Jz, 0, 0, 0, 1, 1),
    ];
    for i in canon {
        assert_eq!(v(&[i, ret], 8), Err(VerifyError::NotCanonical(0)), "{i:?}");
    }
}

#[test]
fn verifier_checks_in_the_documented_order() {
    // No final Ret wins over anything inside.
    assert_eq!(
        v(&[Insn::unary(Op::Mov, 99, 0)], 8),
        Err(VerifyError::NoFinalRet)
    );
    // Earlier instruction wins over a later one.
    let two = [Insn::input(0, 9), Insn::unary(Op::Mov, 99, 0), Insn::ret(0)];
    assert_eq!(v(&two, 8), Err(VerifyError::Input(0)));
    // Within one instruction, fields d a b c come before the immediate.
    assert_eq!(
        v(&[Insn::new(Op::Sel, 0, 0, 20, 0, 0), Insn::ret(0)], 8),
        Err(VerifyError::Register(0))
    );
    assert_eq!(
        v(&[Insn::new(Op::Jz, 0, 20, 0, 0, 0), Insn::ret(0)], 8),
        Err(VerifyError::Register(0))
    );
    assert_eq!(
        v(&[Insn::new(Op::Jz, 0, 0, 0, 3, 0), Insn::ret(0)], 8),
        Err(VerifyError::NotCanonical(0))
    );
}

#[test]
fn encoding_round_trips_and_rejects_damage() {
    let mut rng = Rng(99);
    for _ in 0..300 {
        let w = if rng.below(2) == 0 { 8 } else { 64 };
        let p = Program::new(&gen(&mut rng, w), w).unwrap();
        let mut buf = vec![0u8; p.insns().len() * 16];
        assert_eq!(p.encode(&mut buf), Some(buf.len()));
        let q = Program::decode(&buf, w).unwrap();
        assert_eq!(q.insns(), p.insns());
        let short = buf.len() - 1;
        assert_eq!(p.encode(&mut buf[..short]), None, "short output buffer");
    }
    let ok = Program::new(&[Insn::ret(0)], 8).unwrap();
    let mut b = [0u8; 16];
    ok.encode(&mut b).unwrap();
    assert!(Program::decode(&b, 8).is_ok());
    assert!(matches!(Program::decode(&[], 8), Err(DecodeError::Length)));
    assert!(matches!(
        Program::decode(&b[..15], 8),
        Err(DecodeError::Length)
    ));
    assert!(matches!(
        Program::decode(&vec![0u8; 16 * 257], 8),
        Err(DecodeError::Length)
    ));
    let mut bad = b;
    bad[0] = 30;
    assert!(matches!(Program::decode(&bad, 8), Err(DecodeError::Op(0))));
    for i in 5..8 {
        let mut bad = b;
        bad[i] = 1;
        assert!(matches!(
            Program::decode(&bad, 8),
            Err(DecodeError::Padding(0))
        ));
    }
    assert!(matches!(
        Program::decode(&b, 16),
        Err(DecodeError::Verify(VerifyError::Width))
    ));
}

#[test]
fn damaged_encodings_never_panic_and_accepted_ones_are_canonical() {
    let mut rng = Rng(0xABCD);
    let (mut accepted, mut rejected) = (0, 0);
    for _ in 0..20_000 {
        let w = if rng.below(2) == 0 { 8 } else { 64 };
        let p = Program::new(&gen(&mut rng, w), w).unwrap();
        let mut buf = vec![0u8; p.insns().len() * 16];
        p.encode(&mut buf).unwrap();
        for _ in 0..1 + rng.below(3) {
            let at = rng.below(buf.len() as u64) as usize;
            buf[at] = rng.next() as u8;
        }
        match Program::decode(&buf, w) {
            Ok(q) => {
                let mut out = vec![0u8; buf.len()];
                assert_eq!(q.encode(&mut out), Some(buf.len()));
                assert_eq!(out, buf);
                // Whatever was accepted must also run.
                let _ = q.eval(&[1, 2, 3]);
                accepted += 1;
            }
            Err(_) => rejected += 1,
        }
    }
    assert!(accepted > 100 && rejected > 100, "{accepted} {rejected}");
}

fn optimized(insns: &[Insn], w: u32) -> Vec<Insn> {
    optimize(&Program::new(insns, w).unwrap())
        .unwrap()
        .insns()
        .to_vec()
}

#[test]
fn optimizer_folds_constants_and_drops_dead_code() {
    use Op::*;
    // Registers start at zero, so a constant sum folds to one constant.
    let p = [
        Insn::constant(1, 5),
        Insn::constant(2, 7),
        Insn::binary(Add, 3, 1, 2),
        Insn::ret(3),
    ];
    assert_eq!(optimized(&p, 8), [Insn::constant(3, 12), Insn::ret(3)]);
    // A value that is never used is not computed.
    let p = [
        Insn::input(1, 0),
        Insn::input(2, 1),
        Insn::binary(Add, 3, 1, 2),
        Insn::ret(1),
    ];
    assert_eq!(optimized(&p, 8), [Insn::input(1, 0), Insn::ret(1)]);
    // A branch on a known value is decided and the dead arm disappears.
    let p = [
        Insn::constant(1, 0),
        Insn::jz(1, 3),
        Insn::constant(0, 99),
        Insn::ret(0),
    ];
    assert_eq!(optimized(&p, 8), [Insn::ret(0)]);
    let p = [
        Insn::constant(1, 1),
        Insn::jz(1, 3),
        Insn::constant(0, 99),
        Insn::ret(0),
    ];
    assert_eq!(optimized(&p, 8), [Insn::constant(0, 99), Insn::ret(0)]);
    // x ^ x is zero without knowing x.
    let p = [Insn::input(1, 0), Insn::binary(Xor, 2, 1, 1), Insn::ret(2)];
    assert_eq!(optimized(&p, 8), [Insn::constant(2, 0), Insn::ret(2)]);
    // A conditional jump whose two ways meet at once is not a branch.
    let p = [Insn::input(1, 0), Insn::jz(1, 2), Insn::ret(1)];
    assert_eq!(optimized(&p, 8), [Insn::input(1, 0), Insn::ret(1)]);
    // Selecting on a known condition picks a side.
    let p = [
        Insn::input(1, 0),
        Insn::input(2, 1),
        Insn::constant(3, 1),
        Insn::sel(4, 3, 1, 2),
        Insn::ret(4),
    ];
    assert_eq!(
        optimized(&p, 8),
        [Insn::input(1, 0), Insn::unary(Mov, 4, 1), Insn::ret(4)]
    );
    // Nothing to do: unchanged.
    let p = [
        Insn::input(1, 0),
        Insn::input(2, 1),
        Insn::binary(Add, 3, 1, 2),
        Insn::ret(3),
    ];
    assert_eq!(optimized(&p, 8), p);
}

#[test]
fn optimizer_does_not_fold_across_paths_that_disagree() {
    use Op::*;
    // r2 is 1 on one path and 2 on the other: not a constant at the join.
    let p = [
        Insn::input(1, 0),
        Insn::jz(1, 4),
        Insn::constant(2, 1),
        Insn::jmp(5),
        Insn::constant(2, 2),
        Insn::binary(Add, 3, 2, 2),
        Insn::ret(3),
    ];
    let o = optimized(&p, 8);
    assert!(o.iter().any(|i| i.op == Add), "the sum stays: {o:?}");
    // The same constant on both paths is a constant at the join.
    let mut q = p;
    q[4] = Insn::constant(2, 1);
    let o = optimized(&q, 8);
    assert!(!o.iter().any(|i| i.op == Add), "{o:?}");
    for x in 0..=255u64 {
        assert_eq!(Program::new(&q, 8).unwrap().eval(&[x]).value, 2);
        assert_eq!(Program::new(&o, 8).unwrap().eval(&[x]).value, 2);
    }
}

#[test]
fn every_run_takes_at_most_len_steps() {
    // The bound is structural: jumps only go forward. Also at the maximum length.
    let mut p = vec![Insn::input(1, 0)];
    for _ in 0..MAX_LEN - 3 {
        p.push(Insn::jnz(1, (p.len() + 1) as u64));
    }
    p.push(Insn::unary(Op::Not, 1, 1));
    p.push(Insn::ret(1));
    assert_eq!(p.len(), MAX_LEN);
    let prog = Program::new(&p, 8).unwrap();
    for x in [0u64, 1, 255] {
        assert!(prog.eval(&[x]).steps <= MAX_LEN);
    }
    assert_eq!(
        prog.eval(&[1]).steps,
        MAX_LEN,
        "jumps to the next instruction skip nothing"
    );
}

#[derive(Clone, Copy)]
enum Arg {
    In0,
    In1,
    K(u64),
}

/// `r5 = op a b` with r1, r2 the inputs, then `Ret r5`.
fn one_op(op: Op, a: Arg, b: Arg) -> Vec<Insn> {
    let mut p = vec![Insn::input(1, 0), Insn::input(2, 1)];
    let mut place = |arg: Arg, tmp: u8| match arg {
        Arg::In0 => 1,
        Arg::In1 => 2,
        Arg::K(v) => {
            p.push(Insn::constant(tmp, v));
            tmp
        }
    };
    let (ra, rb) = (place(a, 3), place(b, 4));
    p.push(if op.is_binary() {
        Insn::binary(op, 5, ra, rb)
    } else {
        Insn::unary(op, 5, ra)
    });
    p.push(Insn::ret(5));
    p
}

/// Every rewrite rule of the optimizer, on every operation, with the operand
/// patterns the rules look for (same register, a known 0, 1, all ones, a
/// shift by the width), checked on all 8-bit inputs and on 64-bit edges.
#[test]
fn rewrite_rules_are_sound_for_every_operation_and_operand_pattern() {
    let mut fired = 0;
    for w in [8u32, 64] {
        let m = mask(w);
        let consts = [0, 1, 2, 5, u64::from(w), u64::from(w) + 1, m >> 1, m - 1, m];
        let mut pairs = vec![
            (Arg::In0, Arg::In1),
            (Arg::In1, Arg::In0),
            (Arg::In0, Arg::In0),
        ];
        for k in consts {
            pairs.push((Arg::In0, Arg::K(k)));
            pairs.push((Arg::K(k), Arg::In0));
        }
        let xs: Vec<u64> = if w == 8 {
            (0..256).collect()
        } else {
            vec![
                0,
                1,
                2,
                3,
                63,
                64,
                65,
                m >> 1,
                (m >> 1) + 1,
                m - 1,
                m,
                0x1234_5678_9ABC_DEF0,
            ]
        };
        for op in alu_ops() {
            for &(a, b) in &pairs {
                let p = Program::new(&one_op(op, a, b), w).unwrap();
                let o = optimize(&p).unwrap();
                fired += usize::from(o.insns().len() < p.insns().len());
                let both_inputs = matches!((a, b), (Arg::In0, Arg::In1) | (Arg::In1, Arg::In0));
                let ys: &[u64] = if both_inputs { &xs } else { &[0] };
                let (bp, bo) = (Compiled::new(&p), Compiled::new(&o));
                for &x in &xs {
                    for &y in ys {
                        let want = p.eval(&[x, y]).value;
                        assert_eq!(o.eval(&[x, y]).value, want, "{op:?} w{w} {x} {y}");
                        assert_eq!(bo.run(&[x, y]).value, want, "B opt {op:?} w{w} {x} {y}");
                        assert_eq!(bp.run(&[x, y]).value, want, "B {op:?} w{w} {x} {y}");
                    }
                }
            }
        }
    }
    assert!(
        fired > 200,
        "the rules should fire on many of these: {fired}"
    );
}

#[test]
fn select_and_branch_rules_are_sound() {
    for w in [8u32, 64] {
        let m = mask(w);
        for cond in [Arg::K(0), Arg::K(1), Arg::K(m), Arg::In0, Arg::In1] {
            for (a, b) in [
                (Arg::In0, Arg::In1),
                (Arg::In1, Arg::In1),
                (Arg::In0, Arg::K(5)),
                (Arg::K(5), Arg::K(6)),
            ] {
                for jump in [false, true] {
                    // r3 = cond, r4 = a, r6 = b; r5 = sel(r3, r4, r6) or a branch on r3.
                    let mut p = vec![Insn::input(1, 0), Insn::input(2, 1)];
                    for (arg, r) in [(cond, 3u8), (a, 4), (b, 6)] {
                        p.push(match arg {
                            Arg::In0 => Insn::unary(Op::Mov, r, 1),
                            Arg::In1 => Insn::unary(Op::Mov, r, 2),
                            Arg::K(v) => Insn::constant(r, v),
                        });
                    }
                    if jump {
                        let t = p.len() as u64 + 3;
                        p.extend([
                            Insn::jz(3, t),
                            Insn::unary(Op::Mov, 5, 4),
                            Insn::jmp(t + 1),
                            Insn::unary(Op::Mov, 5, 6),
                        ]);
                    } else {
                        p.push(Insn::sel(5, 3, 4, 6));
                    }
                    p.push(Insn::ret(5));
                    let prog = Program::new(&p, w).unwrap();
                    let opt = optimize(&prog).unwrap();
                    let xs: Vec<u64> = if w == 8 {
                        (0..256).collect()
                    } else {
                        vec![0, 1, 5, m, m >> 1]
                    };
                    for &x in &xs {
                        for &y in &xs[..xs.len().min(8)] {
                            assert_eq!(
                                opt.eval(&[x, y]).value,
                                prog.eval(&[x, y]).value,
                                "{p:?} {x} {y}"
                            );
                        }
                    }
                }
            }
        }
    }
}
