#!/usr/bin/env python3
"""Independent reference for the NIR semantics (docs/research/IR-SEMANTICS.md).

Written from the specification, not from the Rust code: Python integers of
arbitrary size with explicit masking, its own verifier, its own encoder and
its own program generator. `gen` writes the fixtures that
crates/nanox-ir/tests/reference.rs checks both Rust implementations against.
"""
import os
import sys

OPS = ["ret", "const", "in", "mov", "add", "sub", "mul", "udiv", "urem", "and",
       "or", "xor", "shl", "shr", "sar", "eq", "ne", "ltu", "leu", "lts", "les",
       "not", "neg", "sel", "minu", "maxu", "popcnt", "jmp", "jz", "jnz"]
CODE = {n: i for i, n in enumerate(OPS)}
BINARY = {"add", "sub", "mul", "udiv", "urem", "and", "or", "xor", "shl", "shr",
          "sar", "eq", "ne", "ltu", "leu", "lts", "les", "minu", "maxu"}
UNARY = {"mov", "not", "neg", "popcnt"}
ALU = sorted(BINARY | UNARY, key=lambda n: CODE[n])
REGS, INPUTS, MAX_LEN = 16, 8, 256
M64 = (1 << 64) - 1


def used_fields(op):
    """Fields (d, a, b, c) that the operation uses (spec section 2)."""
    if op in ("ret", "jz", "jnz"):
        return (0, 1, 0, 0)
    if op in ("const", "in"):
        return (1, 0, 0, 0)
    if op == "jmp":
        return (0, 0, 0, 0)
    if op == "sel":
        return (1, 1, 1, 1)
    if op in BINARY:
        return (1, 1, 1, 0)
    return (1, 1, 0, 0)


def signed(x, w):
    return x - (1 << w) if x >> (w - 1) else x


def alu(op, x, y, w):
    m = (1 << w) - 1
    s = y % w
    if op == "mov":
        return x
    if op == "add":
        return (x + y) & m
    if op == "sub":
        return (x - y) & m
    if op == "mul":
        return (x * y) & m
    if op == "udiv":
        return m if y == 0 else x // y
    if op == "urem":
        return x if y == 0 else x % y
    if op == "and":
        return x & y
    if op == "or":
        return x | y
    if op == "xor":
        return x ^ y
    if op == "shl":
        return (x << s) & m
    if op == "shr":
        return x >> s
    if op == "sar":
        return (signed(x, w) >> s) & m
    if op == "eq":
        return int(x == y)
    if op == "ne":
        return int(x != y)
    if op == "ltu":
        return int(x < y)
    if op == "leu":
        return int(x <= y)
    if op == "lts":
        return int(signed(x, w) < signed(y, w))
    if op == "les":
        return int(signed(x, w) <= signed(y, w))
    if op == "not":
        return (~x) & m
    if op == "neg":
        return (-x) & m
    if op == "minu":
        return min(x, y)
    if op == "maxu":
        return max(x, y)
    if op == "popcnt":
        return bin(x).count("1")
    raise ValueError(op)


def verify(prog, w):
    """Returns None or the error kind (spec section 5, in check order)."""
    if w not in (8, 64):
        return "width"
    if not prog:
        return "empty"
    if len(prog) > MAX_LEN:
        return "toolong"
    if prog[-1][0] != "ret":
        return "noret"
    m = (1 << w) - 1
    for pc, (op, d, a, b, c, imm) in enumerate(prog):
        for used, f in zip(used_fields(op), (d, a, b, c)):
            if used and f >= REGS:
                return "register"
            if not used and f != 0:
                return "canonical"
        if op == "const":
            if imm & ~m:
                return "immediate"
        elif op == "in":
            if imm >= INPUTS:
                return "input"
        elif op in ("jmp", "jz", "jnz"):
            if imm <= pc or imm >= len(prog):
                return "jump"
        elif imm != 0:
            return "canonical"
    return None


def run(prog, w, inputs):
    m = (1 << w) - 1
    r = [0] * REGS
    pc = steps = 0
    while True:
        op, d, a, b, c, imm = prog[pc]
        steps += 1
        if op == "ret":
            return r[a], steps
        if op == "const":
            r[d] = imm
        elif op == "in":
            r[d] = (inputs[imm] if imm < len(inputs) else 0) & m
        elif op == "sel":
            r[d] = r[a] if r[c] != 0 else r[b]
        elif op == "jmp":
            pc = imm
            continue
        elif op in ("jz", "jnz"):
            if (r[a] == 0) == (op == "jz"):
                pc = imm
                continue
        else:
            r[d] = alu(op, r[a], r[b], w)
        pc += 1


def encode(prog):
    out = bytearray()
    for op, d, a, b, c, imm in prog:
        out += bytes([CODE[op], d, a, b, c, 0, 0, 0]) + imm.to_bytes(8, "little")
    return bytes(out)


def decode(data, w):
    """Returns (prog, None) or (None, error kind)."""
    if not data or len(data) % 16 != 0 or len(data) // 16 > MAX_LEN:
        return None, "length"
    prog = []
    for i in range(0, len(data), 16):
        ch = data[i:i + 16]
        if ch[0] >= len(OPS):
            return None, "op"
        if ch[5:8] != b"\0\0\0":
            return None, "padding"
        prog.append((OPS[ch[0]], ch[1], ch[2], ch[3], ch[4],
                     int.from_bytes(ch[8:], "little")))
    err = verify(prog, w)
    return (prog, None) if err is None else (None, err)
