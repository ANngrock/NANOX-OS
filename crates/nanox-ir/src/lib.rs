//! NIR: a bounded, total intermediate representation for policies —
//! admission rules, filters, resource checks — that the kernel can run
//! without trusting the author of the rule (docs/research/IR-SEMANTICS.md).
//!
//! * 16 registers of W bits (W = 8 or 64), all zero at the start, 8 input
//!   words, one result word;
//! * straight-line code with **forward-only jumps**, so every run ends in
//!   at most `len` steps and never loops;
//! * every operation is total: division by zero, over-wide shifts and
//!   overflow have defined results, nothing traps;
//! * a verifier accepts only well-formed programs (register and input
//!   indexes, jump targets, immediates that fit W, a final `Ret`, canonical
//!   unused fields), and a 16-byte-per-instruction encoding round-trips
//!   exactly.
//!
//! Two implementations of the semantics live here, written to differ:
//! [`Program::eval`] (a direct interpreter over [`alu`]) and
//! [`opt::Compiled`] (the program optimized and run by a second evaluator
//! whose operations are formulated another way, [`opt::alu_b`]). Neither is
//! trusted alone: tests compare them on every 8-bit operand pair and on
//! random programs, and a Python reference (tools/nir) checks both against
//! the written semantics. `no_std`, no allocation, safe Rust.

#![no_std]
#![forbid(unsafe_code)]

pub mod opt;

pub const REGS: usize = 16;
pub const INPUTS: usize = 8;
pub const MAX_LEN: usize = 256;
pub const INSN_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    /// `Ret a`: the result is register `a`.
    Ret = 0,
    /// `Const d imm`.
    Const = 1,
    /// `In d imm`: input word `imm` (0 if the caller passed fewer), cut to W bits.
    In = 2,
    Mov = 3,
    Add = 4,
    Sub = 5,
    Mul = 6,
    /// Unsigned; `x / 0` is all ones.
    Udiv = 7,
    /// Unsigned; `x % 0` is `x`.
    Urem = 8,
    And = 9,
    Or = 10,
    Xor = 11,
    /// Shift amounts are taken modulo the width.
    Shl = 12,
    Shr = 13,
    Sar = 14,
    Eq = 15,
    Ne = 16,
    Ltu = 17,
    Leu = 18,
    Lts = 19,
    Les = 20,
    Not = 21,
    Neg = 22,
    /// `Sel d c a b`: `d = if c != 0 { a } else { b }`.
    Sel = 23,
    Minu = 24,
    Maxu = 25,
    Popcnt = 26,
    /// `Jmp imm`: pc = imm (forward only).
    Jmp = 27,
    /// `Jz a imm`: pc = imm if register `a` is zero.
    Jz = 28,
    Jnz = 29,
}

pub const ALL_OPS: [Op; 30] = [
    Op::Ret,
    Op::Const,
    Op::In,
    Op::Mov,
    Op::Add,
    Op::Sub,
    Op::Mul,
    Op::Udiv,
    Op::Urem,
    Op::And,
    Op::Or,
    Op::Xor,
    Op::Shl,
    Op::Shr,
    Op::Sar,
    Op::Eq,
    Op::Ne,
    Op::Ltu,
    Op::Leu,
    Op::Lts,
    Op::Les,
    Op::Not,
    Op::Neg,
    Op::Sel,
    Op::Minu,
    Op::Maxu,
    Op::Popcnt,
    Op::Jmp,
    Op::Jz,
    Op::Jnz,
];

/// Which fields of an instruction an operation uses.
#[derive(Clone, Copy)]
struct Shape {
    d: bool,
    a: bool,
    b: bool,
    c: bool,
}

impl Op {
    pub fn from_u8(b: u8) -> Option<Self> {
        ALL_OPS.get(usize::from(b)).copied()
    }

    /// Binary ALU operations `d = a op b`.
    pub fn is_binary(self) -> bool {
        matches!(self as u8, 4..=20 | 24 | 25)
    }

    /// Unary `d = op a`.
    pub fn is_unary(self) -> bool {
        matches!(self, Op::Not | Op::Neg | Op::Popcnt | Op::Mov)
    }

    pub fn is_jump(self) -> bool {
        matches!(self, Op::Jmp | Op::Jz | Op::Jnz)
    }

    /// Operations computed by [`alu`]: pure functions of up to two values.
    pub fn is_alu(self) -> bool {
        self.is_binary() || self.is_unary()
    }

    fn shape(self) -> Shape {
        let s = |d, a, b, c| Shape { d, a, b, c };
        match self {
            Op::Ret | Op::Jz | Op::Jnz => s(false, true, false, false),
            Op::Const | Op::In => s(true, false, false, false),
            Op::Jmp => s(false, false, false, false),
            Op::Sel => s(true, true, true, true),
            op if op.is_binary() => s(true, true, true, false),
            _ => s(true, true, false, false),
        }
    }
}

/// One instruction. Unused fields are zero in canonical form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub op: Op,
    pub d: u8,
    pub a: u8,
    pub b: u8,
    pub c: u8,
    pub imm: u64,
}

impl Insn {
    pub const fn new(op: Op, d: u8, a: u8, b: u8, c: u8, imm: u64) -> Self {
        Self {
            op,
            d,
            a,
            b,
            c,
            imm,
        }
    }
    pub const fn ret(a: u8) -> Self {
        Self::new(Op::Ret, 0, a, 0, 0, 0)
    }
    pub const fn constant(d: u8, imm: u64) -> Self {
        Self::new(Op::Const, d, 0, 0, 0, imm)
    }
    pub const fn input(d: u8, idx: u64) -> Self {
        Self::new(Op::In, d, 0, 0, 0, idx)
    }
    pub const fn unary(op: Op, d: u8, a: u8) -> Self {
        Self::new(op, d, a, 0, 0, 0)
    }
    pub const fn binary(op: Op, d: u8, a: u8, b: u8) -> Self {
        Self::new(op, d, a, b, 0, 0)
    }
    pub const fn sel(d: u8, c: u8, a: u8, b: u8) -> Self {
        Self::new(Op::Sel, d, a, b, c, 0)
    }
    pub const fn jmp(target: u64) -> Self {
        Self::new(Op::Jmp, 0, 0, 0, 0, target)
    }
    pub const fn jz(a: u8, target: u64) -> Self {
        Self::new(Op::Jz, 0, a, 0, 0, target)
    }
    pub const fn jnz(a: u8, target: u64) -> Self {
        Self::new(Op::Jnz, 0, a, 0, 0, target)
    }

    /// Bit set of the registers this instruction reads.
    pub fn reads(&self) -> u16 {
        let sh = self.op.shape();
        let bit = |used: bool, r: u8| if used { 1u16 << (r & 15) } else { 0 };
        bit(sh.a, self.a) | bit(sh.b, self.b) | bit(sh.c, self.c)
    }

    /// The register written, if any.
    pub fn writes(&self) -> Option<u8> {
        self.op.shape().d.then_some(self.d)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Width must be 8 or 64.
    Width,
    Empty,
    TooLong,
    /// The final instruction is not `Ret`.
    NoFinalRet,
    /// Register index >= 16 in `pc`.
    Register(usize),
    /// Input index >= 8 in `pc`.
    Input(usize),
    /// Jump target not strictly forward or beyond the last instruction.
    JumpTarget(usize),
    /// Constant does not fit the width.
    Immediate(usize),
    /// A field the operation does not use is not zero.
    NotCanonical(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Length,
    Op(usize),
    Padding(usize),
    Verify(VerifyError),
}

pub fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Checks a program: well-formedness and canonical form. The order of the
/// checks is part of the contract (IR-SEMANTICS.md, section 5): whole-program
/// checks first, then instruction by instruction, fields `d a b c`, then
/// the immediate.
pub fn verify(prog: &[Insn], width: u32) -> Result<(), VerifyError> {
    if width != 8 && width != 64 {
        return Err(VerifyError::Width);
    }
    if prog.is_empty() {
        return Err(VerifyError::Empty);
    }
    if prog.len() > MAX_LEN {
        return Err(VerifyError::TooLong);
    }
    if prog[prog.len() - 1].op != Op::Ret {
        return Err(VerifyError::NoFinalRet);
    }
    let m = mask(width);
    for (pc, i) in prog.iter().enumerate() {
        let sh = i.op.shape();
        for (used, f) in [(sh.d, i.d), (sh.a, i.a), (sh.b, i.b), (sh.c, i.c)] {
            if used {
                if usize::from(f) >= REGS {
                    return Err(VerifyError::Register(pc));
                }
            } else if f != 0 {
                return Err(VerifyError::NotCanonical(pc));
            }
        }
        match i.op {
            Op::Const => {
                if i.imm & !m != 0 {
                    return Err(VerifyError::Immediate(pc));
                }
            }
            Op::In => {
                if i.imm >= INPUTS as u64 {
                    return Err(VerifyError::Input(pc));
                }
            }
            Op::Jmp | Op::Jz | Op::Jnz => {
                if i.imm <= pc as u64 || i.imm >= prog.len() as u64 {
                    return Err(VerifyError::JumpTarget(pc));
                }
            }
            _ => {
                if i.imm != 0 {
                    return Err(VerifyError::NotCanonical(pc));
                }
            }
        }
    }
    Ok(())
}

pub fn encode_insn(i: &Insn) -> [u8; INSN_BYTES] {
    let mut b = [0u8; INSN_BYTES];
    b[0] = i.op as u8;
    b[1] = i.d;
    b[2] = i.a;
    b[3] = i.b;
    b[4] = i.c;
    b[8..].copy_from_slice(&i.imm.to_le_bytes());
    b
}

/// A verified program of a given width.
#[derive(Clone, Copy)]
pub struct Program {
    insns: [Insn; MAX_LEN],
    len: usize,
    width: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub value: u64,
    pub steps: usize,
}

impl Program {
    /// Verifies and stores `prog`.
    pub fn new(prog: &[Insn], width: u32) -> Result<Self, VerifyError> {
        verify(prog, width)?;
        let mut insns = [Insn::ret(0); MAX_LEN];
        insns[..prog.len()].copy_from_slice(prog);
        Ok(Self {
            insns,
            len: prog.len(),
            width,
        })
    }

    pub fn insns(&self) -> &[Insn] {
        &self.insns[..self.len]
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    /// The most steps any run can take.
    pub fn max_steps(&self) -> usize {
        self.len
    }

    /// Canonical bytes: 16 per instruction. Returns the length written.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let n = self.len * INSN_BYTES;
        let o = out.get_mut(..n)?;
        for (ins, chunk) in self.insns().iter().zip(o.chunks_exact_mut(INSN_BYTES)) {
            chunk.copy_from_slice(&encode_insn(ins));
        }
        Some(n)
    }

    pub fn decode(bytes: &[u8], width: u32) -> Result<Self, DecodeError> {
        if bytes.is_empty()
            || !bytes.len().is_multiple_of(INSN_BYTES)
            || bytes.len() / INSN_BYTES > MAX_LEN
        {
            return Err(DecodeError::Length);
        }
        let mut tmp = [Insn::ret(0); MAX_LEN];
        let n = bytes.len() / INSN_BYTES;
        for (pc, ch) in bytes.chunks_exact(INSN_BYTES).enumerate() {
            let op = Op::from_u8(ch[0]).ok_or(DecodeError::Op(pc))?;
            if ch[5..8] != [0, 0, 0] {
                return Err(DecodeError::Padding(pc));
            }
            let mut imm = [0u8; 8];
            imm.copy_from_slice(&ch[8..]);
            tmp[pc] = Insn::new(op, ch[1], ch[2], ch[3], ch[4], u64::from_le_bytes(imm));
        }
        Self::new(&tmp[..n], width).map_err(DecodeError::Verify)
    }

    /// The reference interpreter: implementation A of the semantics.
    pub fn eval(&self, inputs: &[u64]) -> Outcome {
        let w = self.width;
        let m = mask(w);
        let mut r = [0u64; REGS];
        let mut pc = 0;
        let mut steps = 0;
        loop {
            let i = self.insns[pc];
            steps += 1;
            let (a, b) = (r[usize::from(i.a)], r[usize::from(i.b)]);
            match i.op {
                Op::Ret => return Outcome { value: a, steps },
                Op::Const => r[usize::from(i.d)] = i.imm,
                Op::In => {
                    r[usize::from(i.d)] = inputs.get(i.imm as usize).copied().unwrap_or(0) & m;
                }
                Op::Sel => {
                    r[usize::from(i.d)] = if r[usize::from(i.c)] != 0 { a } else { b };
                }
                Op::Jmp => {
                    pc = i.imm as usize;
                    continue;
                }
                Op::Jz => {
                    if a == 0 {
                        pc = i.imm as usize;
                        continue;
                    }
                }
                Op::Jnz => {
                    if a != 0 {
                        pc = i.imm as usize;
                        continue;
                    }
                }
                op => r[usize::from(i.d)] = alu(op, a, b, w),
            }
            pc += 1;
        }
    }
}

pub fn sign_extend(x: u64, w: u32) -> i64 {
    if w >= 64 {
        x as i64
    } else {
        ((x << (64 - w)) as i64) >> (64 - w)
    }
}

/// The operation table, implementation A: each operation stated directly
/// with Rust operators. `x` and `y` are already cut to `w` bits.
pub fn alu(op: Op, x: u64, y: u64, w: u32) -> u64 {
    let m = mask(w);
    let sh = (y % u64::from(w)) as u32;
    match op {
        Op::Mov => x,
        Op::Add => x.wrapping_add(y) & m,
        Op::Sub => x.wrapping_sub(y) & m,
        Op::Mul => x.wrapping_mul(y) & m,
        Op::Udiv => x.checked_div(y).unwrap_or(m),
        Op::Urem => x.checked_rem(y).unwrap_or(x),
        Op::And => x & y,
        Op::Or => x | y,
        Op::Xor => x ^ y,
        Op::Shl => (x << sh) & m,
        Op::Shr => x >> sh,
        Op::Sar => ((sign_extend(x, w) >> sh) as u64) & m,
        Op::Eq => u64::from(x == y),
        Op::Ne => u64::from(x != y),
        Op::Ltu => u64::from(x < y),
        Op::Leu => u64::from(x <= y),
        Op::Lts => u64::from(sign_extend(x, w) < sign_extend(y, w)),
        Op::Les => u64::from(sign_extend(x, w) <= sign_extend(y, w)),
        Op::Not => !x & m,
        Op::Neg => x.wrapping_neg() & m,
        Op::Minu => x.min(y),
        Op::Maxu => x.max(y),
        Op::Popcnt => u64::from(x.count_ones()),
        // Not ALU operations.
        Op::Ret | Op::Const | Op::In | Op::Sel | Op::Jmp | Op::Jz | Op::Jnz => 0,
    }
}
