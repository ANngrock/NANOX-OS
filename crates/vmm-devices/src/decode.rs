//! Decoding of the instructions a guest uses for MMIO: MOV between memory
//! and a register or immediate, and MOVZX from memory — what compilers emit
//! for volatile loads and stores of device registers. Anything else is
//! refused, so the VMM reports the exit instead of guessing.
//!
//! Only the operation, the register and the length are decoded; the
//! guest-physical address comes from the nested page fault (EXITINFO2).

/// A general register: `index` 0..=15 is RAX..R15. With `high8`, one of
/// AH/CH/DH/BH: bits 15:8 of register `index` (0..=3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg {
    pub index: u8,
    pub high8: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Reg(Reg),
    /// Already sign-extended to the operand size (C7 with REX.W).
    Imm(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Read `size` bytes into `reg`, zero-extended to `dest` bytes (`dest ==
    /// size` for MOV). Writes of 4 bytes clear bits 63:32, writes of 1 or 2
    /// bytes merge (the processor's rules for the destination register).
    Load { reg: Reg, size: u8, dest: u8 },
    /// Write the low `size` bytes of `src`.
    Store { src: Source, size: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub op: Operation,
    /// Instruction length, to advance RIP.
    pub len: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes end inside the instruction.
    Truncated,
    /// Longer than the architectural 15 bytes.
    TooLong,
    /// Not one of the supported forms (or a LOCK/REP prefix).
    Unsupported,
    /// The ModRM operand is a register: no memory access to emulate.
    RegisterOperand,
}

const MAX_LEN: usize = 15;

fn byte(bytes: &[u8], i: usize) -> Result<u8, DecodeError> {
    if i >= MAX_LEN {
        return Err(DecodeError::TooLong);
    }
    bytes.get(i).copied().ok_or(DecodeError::Truncated)
}

fn le(bytes: &[u8], i: usize, n: usize) -> Result<u64, DecodeError> {
    let mut v = 0u64;
    for k in 0..n {
        v |= u64::from(byte(bytes, i + k)?) << (8 * k);
    }
    Ok(v)
}

/// Decodes one instruction at the start of `bytes` (64-bit mode).
pub fn decode(bytes: &[u8]) -> Result<Insn, DecodeError> {
    let mut i = 0;
    let mut opsize16 = false;
    loop {
        match byte(bytes, i)? {
            0x66 => opsize16 = true,
            // Segment overrides are ignored in 64-bit mode (FS/GS bases are
            // irrelevant: the fault already carries the address); 0x67 only
            // changes how the address is formed, not the ModRM length.
            0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0x67 => {}
            0xF0 | 0xF2 | 0xF3 => return Err(DecodeError::Unsupported),
            _ => break,
        }
        i += 1;
    }
    let mut rex = 0u8;
    if (0x40..=0x4F).contains(&byte(bytes, i)?) {
        rex = byte(bytes, i)?;
        i += 1;
    }
    let w = rex & 8 != 0;
    let wide: u8 = if w {
        8
    } else if opsize16 {
        2
    } else {
        4
    };
    let mut opcode = u16::from(byte(bytes, i)?);
    i += 1;
    if opcode == 0x0F {
        opcode = 0x0F00 | u16::from(byte(bytes, i)?);
        i += 1;
    }
    let modrm = byte(bytes, i)?;
    i += 1;
    let (mode, reg_field, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
    if mode == 3 {
        return Err(DecodeError::RegisterOperand);
    }
    if rm == 4 {
        let sib = byte(bytes, i)?;
        i += 1;
        if mode == 0 && sib & 7 == 5 {
            i += 4;
        }
    } else if mode == 0 && rm == 5 {
        i += 4; // RIP-relative disp32
    }
    match mode {
        1 => i += 1,
        2 => i += 4,
        _ => {}
    }
    // The displacement must be present even though its value is unused.
    if i > 0 {
        byte(bytes, i - 1)?;
    }
    let reg = |size: u8| {
        let index = reg_field | (rex >> 2 & 1) << 3;
        // Without REX, byte registers 4..=7 are AH, CH, DH, BH.
        if size == 1 && rex == 0 && (4..8).contains(&index) {
            Reg {
                index: index - 4,
                high8: true,
            }
        } else {
            Reg {
                index,
                high8: false,
            }
        }
    };
    let op = match opcode {
        0x88 => Operation::Store {
            src: Source::Reg(reg(1)),
            size: 1,
        },
        0x89 => Operation::Store {
            src: Source::Reg(reg(wide)),
            size: wide,
        },
        0x8A => Operation::Load {
            reg: reg(1),
            size: 1,
            dest: 1,
        },
        0x8B => Operation::Load {
            reg: reg(wide),
            size: wide,
            dest: wide,
        },
        0xC6 | 0xC7 if reg_field != 0 => return Err(DecodeError::Unsupported),
        0xC6 => {
            let imm = le(bytes, i, 1)?;
            i += 1;
            Operation::Store {
                src: Source::Imm(imm),
                size: 1,
            }
        }
        0xC7 => {
            let n = if wide == 2 { 2 } else { 4 };
            let mut imm = le(bytes, i, n)?;
            i += n;
            if wide == 8 {
                imm = imm as u32 as i32 as i64 as u64;
            }
            Operation::Store {
                src: Source::Imm(imm),
                size: wide,
            }
        }
        0x0FB6 | 0x0FB7 => Operation::Load {
            reg: reg(wide),
            size: if opcode == 0x0FB6 { 1 } else { 2 },
            dest: wide,
        },
        _ => return Err(DecodeError::Unsupported),
    };
    if i > MAX_LEN {
        return Err(DecodeError::TooLong);
    }
    Ok(Insn { op, len: i as u8 })
}

/// Applies a load's result to a 64-bit register value (x86-64 rules).
pub fn merge(old: u64, reg: Reg, dest: u8, value: u64) -> u64 {
    match (dest, reg.high8) {
        (1, true) => (old & !0xFF00) | (value & 0xFF) << 8,
        (1, false) => (old & !0xFF) | (value & 0xFF),
        (2, _) => (old & !0xFFFF) | (value & 0xFFFF),
        (4, _) => value & 0xFFFF_FFFF,
        _ => value,
    }
}

/// The bytes a store writes from a register value.
pub fn source_value(value: u64, reg: Reg) -> u64 {
    if reg.high8 {
        value >> 8 & 0xFF
    } else {
        value
    }
}
