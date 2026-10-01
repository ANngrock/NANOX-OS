//! The optimizer and the second implementation (B) of the NIR semantics.
//!
//! [`optimize`] turns a verified program into a smaller verified program
//! that computes the same result on every input and never takes more steps:
//! forward constant propagation (registers start at zero, so a lot is
//! known), a few algebraic identities, branch pruning, unreachable-code
//! removal and dead-store elimination by a single backward liveness pass
//! (jumps only go forward, so one pass is exact). [`Compiled`] runs a
//! program with relative jumps on evaluator B, whose operations
//! ([`alu_b`]) are formulated differently from [`crate::alu`]: 128-bit
//! sums, restoring long division, sign-bit flips instead of signed
//! comparison, SWAR population count.

use crate::{alu, mask, Insn, Op, Outcome, Program, VerifyError, MAX_LEN, REGS};

/// Restoring long division on `w` bits. Dividing by zero falls out of the
/// algorithm: every step subtracts nothing and sets its quotient bit, so
/// the quotient is all ones and the remainder is the dividend.
fn divmod(x: u64, y: u64, w: u32) -> (u64, u64) {
    let (mut q, mut r) = (0u64, 0u128);
    let mut i = w;
    while i > 0 {
        i -= 1;
        r = (r << 1) | u128::from((x >> i) & 1);
        if r >= u128::from(y) {
            r -= u128::from(y);
            q |= 1 << i;
        }
    }
    (q, r as u64)
}

fn popcount(x: u64) -> u64 {
    let mut v = x;
    v -= (v >> 1) & 0x5555_5555_5555_5555;
    v = (v & 0x3333_3333_3333_3333) + ((v >> 2) & 0x3333_3333_3333_3333);
    v = (v + (v >> 4)) & 0x0f0f_0f0f_0f0f_0f0f;
    v.wrapping_mul(0x0101_0101_0101_0101) >> 56
}

/// The operation table, implementation B. Same contract as [`crate::alu`].
pub fn alu_b(op: Op, x: u64, y: u64, w: u32) -> u64 {
    let m = mask(w);
    let sign = 1u64 << (w - 1);
    let sh = (y & u64::from(w - 1)) as u32;
    match op {
        Op::Mov => x,
        Op::Add => ((u128::from(x) + u128::from(y)) as u64) & m,
        Op::Sub => ((u128::from(x) + u128::from(!y & m) + 1) as u64) & m,
        Op::Mul => ((u128::from(x) * u128::from(y)) as u64) & m,
        Op::Udiv => divmod(x, y, w).0,
        Op::Urem => divmod(x, y, w).1,
        Op::And => !(!x | !y) & m,
        Op::Or => !(!x & !y) & m,
        Op::Xor => (x | y) & !(x & y),
        Op::Shl => ((u128::from(x) << sh) as u64) & m,
        Op::Shr => x >> sh,
        Op::Sar => ((x ^ sign) >> sh).wrapping_sub(sign >> sh) & m,
        Op::Eq => u64::from((x ^ y) == 0),
        Op::Ne => u64::from((x ^ y) != 0),
        Op::Ltu => u64::from(x < y),
        Op::Leu => u64::from(y.checked_sub(x).is_some()),
        Op::Lts => u64::from((x ^ sign) < (y ^ sign)),
        Op::Les => u64::from((y ^ sign).checked_sub(x ^ sign).is_some()),
        Op::Not => x ^ m,
        Op::Neg => (x ^ m).wrapping_add(1) & m,
        Op::Minu => {
            if x < y {
                x
            } else {
                y
            }
        }
        Op::Maxu => {
            if y < x {
                x
            } else {
                y
            }
        }
        Op::Popcnt => popcount(x),
        Op::Ret | Op::Const | Op::In | Op::Sel | Op::Jmp | Op::Jz | Op::Jnz => 0,
    }
}

/// What the forward pass knows at a program point.
#[derive(Clone, Copy)]
struct St {
    reached: bool,
    /// Bit r set: register r holds `v[r]` on every path here.
    known: u16,
    v: [u64; REGS],
}

impl St {
    const NONE: St = St {
        reached: false,
        known: 0,
        v: [0; REGS],
    };

    fn merge(&mut self, o: &St) {
        if !o.reached {
            return;
        }
        if !self.reached {
            *self = *o;
            return;
        }
        let mut k = self.known & o.known;
        for r in 0..REGS {
            if k >> r & 1 == 1 && self.v[r] != o.v[r] {
                k &= !(1 << r);
            }
        }
        self.known = k;
    }

    fn get(&self, r: u8) -> Option<u64> {
        (self.known >> r & 1 == 1).then(|| self.v[usize::from(r)])
    }

    fn set(&mut self, r: u8, v: Option<u64>) {
        match v {
            Some(x) => {
                self.known |= 1 << r;
                self.v[usize::from(r)] = x;
            }
            None => self.known &= !(1 << r),
        }
    }
}

fn mov(d: u8, a: u8) -> Option<Insn> {
    (d != a).then(|| Insn::unary(Op::Mov, d, a))
}

fn konst(d: u8, v: u64) -> Option<Insn> {
    Some(Insn::constant(d, v))
}

/// A cheaper instruction with the same effect, or `None` if this one has no
/// effect at all here. `st` describes the registers before it runs.
fn rewrite(i: Insn, st: &St, w: u32) -> Option<Insn> {
    let m = mask(w);
    let (x, y) = (st.get(i.a), st.get(i.b));
    match i.op {
        Op::Sel => match st.get(i.c) {
            Some(c) => mov(i.d, if c != 0 { i.a } else { i.b }),
            None if i.a == i.b => mov(i.d, i.a),
            None => Some(i),
        },
        Op::Jz | Op::Jnz => match x {
            Some(c) if (c == 0) == (i.op == Op::Jz) => Some(Insn::jmp(i.imm)),
            Some(_) => None,
            None => Some(i),
        },
        Op::Mov if i.d == i.a => None,
        op if op.is_unary() => match x {
            Some(x) => konst(i.d, alu(op, x, 0, w)),
            None => Some(i),
        },
        op if op.is_binary() => {
            if let (Some(x), Some(y)) = (x, y) {
                return konst(i.d, alu(op, x, y, w));
            }
            let (d, a, b, same) = (i.d, i.a, i.b, i.a == i.b);
            let is = |v: Option<u64>, c: u64| v == Some(c);
            // A rewrite to `Mov d d` is a no-op; `mov` returns None then.
            match op {
                Op::Add if is(y, 0) => mov(d, a),
                Op::Add if is(x, 0) => mov(d, b),
                Op::Sub if is(y, 0) => mov(d, a),
                Op::Sub if same => konst(d, 0),
                Op::Mul if is(x, 0) || is(y, 0) => konst(d, 0),
                Op::Mul if is(y, 1) => mov(d, a),
                Op::Mul if is(x, 1) => mov(d, b),
                Op::And if is(x, 0) || is(y, 0) => konst(d, 0),
                Op::And if is(y, m) || same => mov(d, a),
                Op::And if is(x, m) => mov(d, b),
                Op::Or if is(x, m) || is(y, m) => konst(d, m),
                Op::Or if is(y, 0) || same => mov(d, a),
                Op::Or if is(x, 0) => mov(d, b),
                Op::Xor if is(y, 0) => mov(d, a),
                Op::Xor if is(x, 0) => mov(d, b),
                Op::Xor if same => konst(d, 0),
                Op::Shl | Op::Shr | Op::Sar if is(x, 0) => konst(d, 0),
                Op::Shl | Op::Shr | Op::Sar if y.is_some_and(|s| s % u64::from(w) == 0) => {
                    mov(d, a)
                }
                Op::Udiv if is(y, 1) => mov(d, a),
                Op::Urem if is(y, 1) => konst(d, 0),
                Op::Eq | Op::Leu | Op::Les if same => konst(d, 1),
                Op::Ne | Op::Ltu | Op::Lts if same => konst(d, 0),
                Op::Minu if is(x, 0) || is(y, 0) => konst(d, 0),
                Op::Maxu if is(x, m) || is(y, m) => konst(d, m),
                Op::Minu | Op::Maxu if same => mov(d, a),
                _ => Some(i),
            }
        }
        _ => Some(i),
    }
}

/// [`rewrite`] for a caller-supplied description of the registers (`known[r]`
/// is the value register `r` holds, if it is known). For the proofs crate,
/// which checks the rewrite rules against every state in a bounded domain.
#[doc(hidden)]
pub fn simplify(i: Insn, known: &[Option<u64>; REGS], w: u32) -> Option<Insn> {
    let mut st = St {
        reached: true,
        known: 0,
        v: [0; REGS],
    };
    for (r, k) in known.iter().enumerate() {
        st.set(r as u8, *k);
    }
    rewrite(i, &st, w)
}

fn transfer(st: &mut St, i: &Insn) {
    match i.op {
        Op::Const => st.set(i.d, Some(i.imm)),
        Op::Mov => st.set(i.d, st.get(i.a)),
        Op::Ret | Op::Jmp | Op::Jz | Op::Jnz => {}
        _ => st.set(i.d, None),
    }
}

/// Returns a verified program with the same result on every input and no
/// more steps on any input.
pub fn optimize(p: &Program) -> Result<Program, VerifyError> {
    let w = p.width();
    let src = p.insns();
    let n = src.len();
    let mut st = [St::NONE; MAX_LEN];
    st[0] = St {
        reached: true,
        known: u16::MAX,
        v: [0; REGS],
    };
    let mut out = [Insn::ret(0); MAX_LEN];
    let mut keep = [false; MAX_LEN];

    // Forward: constants, identities, branch pruning, reachability.
    for pc in 0..n {
        if !st[pc].reached {
            continue;
        }
        let mut cur = st[pc];
        match rewrite(src[pc], &cur, w) {
            None => st[pc + 1].merge(&cur),
            Some(i) => {
                out[pc] = i;
                keep[pc] = true;
                match i.op {
                    Op::Ret => {}
                    Op::Jmp => st[i.imm as usize].merge(&cur),
                    Op::Jz | Op::Jnz => {
                        st[i.imm as usize].merge(&cur);
                        st[pc + 1].merge(&cur);
                    }
                    _ => {
                        transfer(&mut cur, &i);
                        st[pc + 1].merge(&cur);
                    }
                }
            }
        }
    }

    // Backward: dead stores and jumps to the next instruction. `next[q]` is
    // the first kept instruction at or after q.
    let mut live = [0u16; MAX_LEN + 1];
    let mut next = [0usize; MAX_LEN + 1];
    next[n] = n;
    for pc in (0..n).rev() {
        let mut here = None;
        if keep[pc] {
            let i = out[pc];
            let t = i.imm as usize;
            here = match i.op {
                Op::Ret => Some(i.reads()),
                Op::Jmp if next[t] == next[pc + 1] => None,
                Op::Jmp => Some(live[t]),
                Op::Jz | Op::Jnz if next[t] == next[pc + 1] => None,
                Op::Jz | Op::Jnz => Some(live[t] | live[pc + 1] | i.reads()),
                _ => {
                    let d = i.d;
                    (live[pc + 1] >> d & 1 == 1).then(|| (live[pc + 1] & !(1 << d)) | i.reads())
                }
            };
        }
        match here {
            Some(l) => {
                live[pc] = l;
                next[pc] = pc;
            }
            None => {
                keep[pc] = false;
                live[pc] = live[pc + 1];
                next[pc] = next[pc + 1];
            }
        }
    }

    // Compact and retarget.
    let mut idx = [0usize; MAX_LEN + 1];
    let mut k = 0;
    for pc in 0..n {
        idx[pc] = k;
        k += usize::from(keep[pc]);
    }
    idx[n] = k;
    let mut prog = [Insn::ret(0); MAX_LEN];
    let mut j = 0;
    for pc in 0..n {
        if keep[pc] {
            let mut i = out[pc];
            if i.op.is_jump() {
                i.imm = idx[i.imm as usize] as u64;
            }
            prog[j] = i;
            j += 1;
        }
    }
    Program::new(&prog[..j], w)
}

/// A program in the form evaluator B runs: jumps are relative (`imm` is the
/// number of instructions to skip forward, at least 1).
#[derive(Clone, Copy)]
pub struct Compiled {
    code: [Insn; MAX_LEN],
    len: usize,
    width: u32,
}

impl Compiled {
    /// Translates without optimizing.
    pub fn new(p: &Program) -> Self {
        let mut code = [Insn::ret(0); MAX_LEN];
        for (pc, i) in p.insns().iter().enumerate() {
            code[pc] = *i;
            if i.op.is_jump() {
                code[pc].imm = i.imm - pc as u64;
            }
        }
        Self {
            code,
            len: p.insns().len(),
            width: p.width(),
        }
    }

    /// Optimizes, then translates.
    pub fn optimized(p: &Program) -> Result<Self, VerifyError> {
        Ok(Self::new(&optimize(p)?))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Evaluator B.
    pub fn run(&self, inputs: &[u64]) -> Outcome {
        let w = self.width;
        let m = mask(w);
        let mut r = [0u64; REGS];
        let mut pc = 0usize;
        let mut steps = 0;
        loop {
            let i = &self.code[pc];
            steps += 1;
            let (d, x, y) = (usize::from(i.d), r[usize::from(i.a)], r[usize::from(i.b)]);
            match i.op {
                Op::Ret => return Outcome { value: x, steps },
                Op::Const => r[d] = i.imm,
                Op::In => {
                    let idx = i.imm as usize;
                    r[d] = if idx < inputs.len() {
                        inputs[idx] & m
                    } else {
                        0
                    };
                }
                Op::Sel => r[d] = if r[usize::from(i.c)] != 0 { x } else { y },
                Op::Jmp => {
                    pc += i.imm as usize;
                    continue;
                }
                Op::Jz if x == 0 => {
                    pc += i.imm as usize;
                    continue;
                }
                Op::Jnz if x != 0 => {
                    pc += i.imm as usize;
                    continue;
                }
                Op::Jz | Op::Jnz => {}
                op => r[d] = alu_b(op, x, y, w),
            }
            pc += 1;
        }
    }
}
