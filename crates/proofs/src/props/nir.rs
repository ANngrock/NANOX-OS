//! Properties of nanox-ir (docs/research/IR-SEMANTICS.md).

use nanox_ir::opt::{alu_b, optimize, simplify, Compiled};
use nanox_ir::{alu, Insn, Op, Program, ALL_OPS, INSN_BYTES, REGS};

use crate::{Proof, Property};

const COMPONENT: &[&str] = &["crates/nanox-ir/src/lib.rs", "crates/nanox-ir/src/opt.rs"];
const CHECKER: &str = "crates/proofs/src/props/nir.rs";

pub fn properties() -> Vec<Property> {
    vec![
        Property {
            id: "nir.alu.three-way-w8",
            version: 1,
            statement: "For every operation and every pair of 8-bit operands, the two \
                        implementations of the operation table and an integer-arithmetic \
                        oracle give the same result, inside 8 bits.",
            bound: "all 23 ALU operations x all 65536 operand pairs, width 8",
            component: COMPONENT,
            checker: CHECKER,
            run: alu_three_way,
        },
        Property {
            id: "nir.rewrite.local-soundness-w8",
            version: 1,
            statement: "A rewrite of one instruction under partial knowledge of the \
                        registers has the same effect as the instruction on every concrete \
                        register state consistent with that knowledge.",
            bound: "width 8; registers r0,r1 (each unknown or a known value, all 256 values \
                    either way); every ALU operation, Sel, Jz, Jnz over r0,r1",
            component: COMPONENT,
            checker: CHECKER,
            run: rewrite_local,
        },
        Property {
            id: "nir.optimize.small-programs-w8",
            version: 3,
            statement: "optimize preserves the result of every program of the family on \
                        every input, never increases the step count, produces a verified \
                        program, and the second evaluator agrees on both programs.",
            bound: "width 8; [X,Y,Ret], [In,In,X,Ret] (distinct inputs), \
                    [In,J,Y,Z,Ret] (J a branch to pc 3 or 4; Y,Z from 72 instructions) and \
                    [In,In,J,Y,Ret] (J a branch to pc 4) over r0,r1 with every ALU op, Sel, \
                    Const in {0,1,2,7F,80,FF}, In, forward jumps; all input values of every \
                    input the program reads",
            component: COMPONENT,
            checker: CHECKER,
            run: optimize_small,
        },
        Property {
            id: "nir.decode.canonical-and-total",
            version: 1,
            statement: "decode never panics, accepts only canonical encodings (they encode \
                        back to the same bytes) and an accepted program runs.",
            bound: "one-instruction programs: op 0..40, each register field in \
                    {0,1,15,16,255}, padding in {0,1}, imm in 8 boundary values, both widths",
            component: COMPONENT,
            checker: CHECKER,
            run: decode_total,
        },
    ]
}

fn alu_ops() -> impl Iterator<Item = Op> {
    ALL_OPS.into_iter().filter(|o| o.is_alu())
}

/// The third formulation: the table in the specification, on wide signed
/// integers, without any wrapping idiom.
fn oracle(op: Op, x: u8, y: u8) -> u8 {
    let (xi, yi) = (i64::from(x), i64::from(y));
    let (xs, ys) = (i64::from(x as i8), i64::from(y as i8));
    let sh = yi % 8;
    let r = match op {
        Op::Mov => xi,
        Op::Add => (xi + yi).rem_euclid(256),
        Op::Sub => (xi - yi).rem_euclid(256),
        Op::Mul => (xi * yi).rem_euclid(256),
        Op::Udiv => {
            if yi == 0 {
                255
            } else {
                xi / yi
            }
        }
        Op::Urem => {
            if yi == 0 {
                xi
            } else {
                xi % yi
            }
        }
        Op::And => xi & yi,
        Op::Or => xi | yi,
        Op::Xor => xi ^ yi,
        Op::Shl => (xi << sh).rem_euclid(256),
        Op::Shr => xi >> sh,
        Op::Sar => (xs >> sh).rem_euclid(256),
        Op::Eq => i64::from(xi == yi),
        Op::Ne => i64::from(xi != yi),
        Op::Ltu => i64::from(xi < yi),
        Op::Leu => i64::from(xi <= yi),
        Op::Lts => i64::from(xs < ys),
        Op::Les => i64::from(xs <= ys),
        Op::Not => 255 - xi,
        Op::Neg => (-xi).rem_euclid(256),
        Op::Minu => xi.min(yi),
        Op::Maxu => xi.max(yi),
        Op::Popcnt => i64::from(x.count_ones()),
        _ => unreachable!("not an ALU operation"),
    };
    r as u8
}

fn alu_three_way() -> Proof {
    let mut cases = 0;
    for op in alu_ops() {
        for x in 0..=255u8 {
            for y in 0..=255u8 {
                cases += 1;
                let (a, b, c) = (
                    alu(op, u64::from(x), u64::from(y), 8),
                    alu_b(op, u64::from(x), u64::from(y), 8),
                    u64::from(oracle(op, x, y)),
                );
                if a != c || b != c {
                    return Proof::failed(cases, format!("{op:?} {x} {y}: A={a} B={b} oracle={c}"));
                }
            }
        }
    }
    Proof::held(cases)
}

/// One instruction on concrete registers: the new registers and whether a
/// conditional jump is taken. A third, separate single-step interpreter.
fn step(i: &Insn, r: &[u64; REGS]) -> ([u64; REGS], bool) {
    let mut n = *r;
    let (x, y) = (r[usize::from(i.a)], r[usize::from(i.b)]);
    let mut taken = false;
    match i.op {
        Op::Const => n[usize::from(i.d)] = i.imm,
        Op::In => n[usize::from(i.d)] = 0,
        Op::Sel => n[usize::from(i.d)] = if r[usize::from(i.c)] != 0 { x } else { y },
        Op::Jmp => taken = true,
        Op::Jz => taken = x == 0,
        Op::Jnz => taken = x != 0,
        Op::Ret => {}
        op => n[usize::from(i.d)] = alu(op, x, y, 8),
    }
    (n, taken)
}

/// Every instruction of the local-soundness domain: each ALU operation, Sel
/// and the conditional jumps, over r0 and r1.
fn local_forms() -> Vec<Insn> {
    let mut v = Vec::new();
    for op in alu_ops() {
        for d in 0..2u8 {
            for a in 0..2u8 {
                if op.is_binary() {
                    for b in 0..2u8 {
                        v.push(Insn::binary(op, d, a, b));
                    }
                } else {
                    v.push(Insn::unary(op, d, a));
                }
            }
        }
    }
    for bits in 0..16u8 {
        v.push(Insn::sel(
            bits & 1,
            bits >> 1 & 1,
            bits >> 2 & 1,
            bits >> 3 & 1,
        ));
    }
    for a in 0..2u8 {
        v.push(Insn::jz(a, 9));
        v.push(Insn::jnz(a, 9));
    }
    v
}

fn rewrite_local() -> Proof {
    let mut cases = 0;
    for ins in local_forms() {
        for s0 in 0..512u64 {
            for s1 in 0..512u64 {
                cases += 1;
                let (v0, v1) = (s0 % 256, s1 % 256);
                let mut known = [None; REGS];
                known[0] = (s0 >= 256).then_some(v0);
                known[1] = (s1 >= 256).then_some(v1);
                let mut regs = [0u64; REGS];
                regs[0] = v0;
                regs[1] = v1;
                let want = step(&ins, &regs);
                let got = match simplify(ins, &known, 8) {
                    None => (regs, false),
                    Some(j) if j.op.is_jump() != ins.op.is_jump() => {
                        return Proof::failed(cases, format!("{ins:?} became {j:?}"));
                    }
                    Some(j) => step(&j, &regs),
                };
                if got != want {
                    return Proof::failed(
                        cases,
                        format!("{ins:?} with knowledge {known:?} and registers {v0},{v1}"),
                    );
                }
            }
        }
    }
    Proof::held(cases)
}

fn body_alphabet(pos: usize, len: usize) -> Vec<Insn> {
    let mut v: Vec<Insn> = local_forms()
        .into_iter()
        .filter(|i| !i.op.is_jump())
        .collect();
    for d in 0..2u8 {
        for k in [0, 1, 2, 0x7F, 0x80, 0xFF] {
            v.push(Insn::constant(d, k));
        }
        for idx in 0..2 {
            v.push(Insn::input(d, idx));
        }
    }
    for t in pos + 1..len {
        v.push(Insn::jmp(t as u64));
        for a in 0..2u8 {
            v.push(Insn::jz(a, t as u64));
            v.push(Insn::jnz(a, t as u64));
        }
    }
    v
}

/// Enough variety to make values differ between the arms of a branch.
fn merge_alphabet() -> Vec<Insn> {
    let mut v = Vec::new();
    for d in 0..2u8 {
        for k in [0, 1, 0xFF] {
            v.push(Insn::constant(d, k));
        }
        v.push(Insn::unary(Op::Mov, d, 1 - d));
    }
    for op in [
        Op::Add,
        Op::Sub,
        Op::Xor,
        Op::And,
        Op::Or,
        Op::Mul,
        Op::Ltu,
        Op::Eq,
    ] {
        for d in 0..2u8 {
            for a in 0..2u8 {
                for b in 0..2u8 {
                    v.push(Insn::binary(op, d, a, b));
                }
            }
        }
    }
    v
}

/// Runs one program and its optimization on all values of the inputs it
/// reads; returns the number of runs.
fn check_program(insns: &[Insn]) -> Result<u64, String> {
    let prog = Program::new(insns, 8).map_err(|e| format!("generated {insns:?}: {e:?}"))?;
    let opt = optimize(&prog).map_err(|e| format!("optimize {insns:?}: {e:?}"))?;
    let (b, bo) = (Compiled::new(&prog), Compiled::new(&opt));
    let reads = |k: u64| insns.iter().any(|i| i.op == Op::In && i.imm == k);
    let (r0, r1) = (reads(0), reads(1));
    let (n0, n1) = (if r0 { 256 } else { 1 }, if r1 { 256 } else { 1 });
    for x in 0..n0 {
        for y in 0..n1 {
            let inp = [x, y];
            let a = prog.eval(&inp);
            let o = opt.eval(&inp);
            let (rb, rbo) = (b.run(&inp), bo.run(&inp));
            if o.value != a.value
                || o.steps > a.steps
                || (rb.value, rb.steps) != (a.value, a.steps)
                || (rbo.value, rbo.steps) != (o.value, o.steps)
            {
                return Err(format!(
                    "{insns:?} on {inp:?}: A={a:?} optimized={o:?} B={rb:?} B-optimized={rbo:?}"
                ));
            }
        }
    }
    Ok(n0 * n1)
}

fn optimize_small() -> Proof {
    let mut cases = 0;
    let mut run = |p: &[Insn]| -> Result<(), String> {
        cases += check_program(p)?;
        Ok(())
    };
    let res = (|| {
        for x in body_alphabet(0, 3) {
            for y in body_alphabet(1, 3) {
                for r in 0..2u8 {
                    run(&[x, y, Insn::ret(r)])?;
                }
            }
        }
        for (i, j, k, l) in [(0, 0, 0, 1), (0, 1, 0, 1), (1, 0, 0, 1), (1, 1, 0, 1)]
            .into_iter()
            .chain([(0, 0, 1, 0), (0, 1, 1, 0), (1, 0, 1, 0), (1, 1, 1, 0)])
        {
            for x in body_alphabet(2, 4) {
                for r in 0..2u8 {
                    run(&[Insn::input(i, k), Insn::input(j, l), x, Insn::ret(r)])?;
                }
            }
        }
        let alphabet = merge_alphabet();
        for d in 0..2u8 {
            for t in [3u64, 4] {
                let mut branches = vec![Insn::jmp(t)];
                for a in 0..2u8 {
                    branches.push(Insn::jz(a, t));
                    branches.push(Insn::jnz(a, t));
                }
                for j in branches {
                    for y in &alphabet {
                        for z in &alphabet {
                            for r in 0..2u8 {
                                run(&[Insn::input(d, 0), j, *y, *z, Insn::ret(r)])?;
                            }
                        }
                    }
                }
            }
        }
        // A value defined before a branch and used only where the branch goes.
        for (k0, k1) in [(0u64, 1u64), (1, 0)] {
            let mut branches = vec![Insn::jmp(4)];
            for a in 0..2u8 {
                branches.push(Insn::jz(a, 4));
                branches.push(Insn::jnz(a, 4));
            }
            for j in branches {
                for y in &alphabet {
                    for r in 0..2u8 {
                        run(&[Insn::input(0, k0), Insn::input(1, k1), j, *y, Insn::ret(r)])?;
                    }
                }
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => Proof::held(cases),
        Err(why) => Proof::failed(cases, why),
    }
}

fn decode_total() -> Proof {
    const REG: [u8; 5] = [0, 1, 15, 16, 255];
    const IMM: [u64; 8] = [0, 1, 7, 8, 255, 256, 1 << 63, u64::MAX];
    let mut cases = 0;
    for w in [8u32, 64] {
        for op in 0..40u8 {
            for d in REG {
                for a in REG {
                    for b in REG {
                        for c in REG {
                            for pad in [0u8, 1] {
                                for imm in IMM {
                                    let mut bytes = [0u8; 2 * INSN_BYTES];
                                    bytes[..5].copy_from_slice(&[op, d, a, b, c]);
                                    bytes[6] = pad;
                                    bytes[8..16].copy_from_slice(&imm.to_le_bytes());
                                    cases += 1;
                                    if let Ok(p) = Program::decode(&bytes, w) {
                                        let mut out = [0u8; 2 * INSN_BYTES];
                                        let ok = p.encode(&mut out) == Some(bytes.len())
                                            && out == bytes
                                            && p.eval(&[1, 2, 3]).steps <= p.max_steps()
                                            && optimize(&p).is_ok();
                                        if !ok {
                                            return Proof::failed(cases, format!("{bytes:02x?}"));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Proof::held(cases)
}
