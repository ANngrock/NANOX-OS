#!/usr/bin/env python3
"""Writes crates/nanox-ir/tests/fixtures/{alu,programs}.txt from nir_ref.

Deterministic: its own xorshift64* stream, no dependence on the Python
version. `--check` compares with the checked-in files instead of writing.
"""
import os
import sys

import nir_ref as R

HERE = os.path.dirname(os.path.abspath(__file__))
FIX = os.path.join(HERE, "..", "..", "crates", "nanox-ir", "tests", "fixtures")
FNV_OFF, FNV_PRIME = 0xCBF29CE484222325, 0x100000001B3
EDGES = [0, 1, 2, 3, 7, 8, 0x7F, 0x80, 0xFF, 0x100, 0x7FFFFFFF, 0x80000000,
         0xFFFFFFFF, 0x100000000, 0x7FFFFFFFFFFFFFFF, 0x8000000000000000,
         0x8000000000000001, 0xFFFFFFFFFFFFFFFE, 0xFFFFFFFFFFFFFFFF, 63, 64, 65]


class Rng:
    def __init__(self, seed):
        self.s = seed & R.M64 or 1

    def next(self):
        x = self.s
        x ^= x >> 12
        x ^= (x << 25) & R.M64
        x ^= x >> 27
        self.s = x
        return (x * 0x2545F4914F6CDD1D) & R.M64

    def below(self, n):
        return self.next() % n


def fnv(digest, data):
    for b in data:
        digest = ((digest ^ b) * FNV_PRIME) & R.M64
    return digest


def alu_lines():
    lines = ["# NIR ALU digests: FNV-1a 64 over the results of alu(op, x, y).",
             "# width 8: x = 0..255 (outer), y = 0..255 (inner), 1 byte each.",
             "# width 64: EDGES x EDGES, then 4096 pairs (x, y) from xorshift64*",
             "# seeded with 0x9E3779B97F4A7C15, 8 bytes little-endian each."]
    pairs64 = [(x, y) for x in EDGES for y in EDGES]
    rng = Rng(0x9E3779B97F4A7C15)
    for _ in range(4096):
        pairs64.append((rng.next(), rng.next()))
    for w, pairs, nb in ((8, [(x, y) for x in range(256) for y in range(256)], 1),
                         (64, pairs64, 8)):
        for op in R.ALU:
            h = FNV_OFF
            for x, y in pairs:
                h = fnv(h, R.alu(op, x, y, w).to_bytes(nb, "little"))
            lines.append("%d %s %016x" % (w, op, h))
    return lines


def imm_for(rng, w):
    m = (1 << w) - 1
    pool = [0, 1, 2, m, m - 1, m >> 1, (m >> 1) + 1, 3, 7]
    return pool[rng.below(len(pool))] if rng.below(2) else rng.next() & m


def gen_program(rng, w):
    n = 2 + rng.below(30)
    nreg = 2 + rng.below(7)
    reg = lambda: rng.below(nreg)
    prog = []
    for pc in range(n - 1):
        k = rng.below(20)
        if k < 11:
            op = R.ALU[rng.below(len(R.ALU))]
            b = reg() if op in R.BINARY else 0
            prog.append((op, reg(), reg(), b, 0, 0))
        elif k < 13:
            prog.append(("const", reg(), 0, 0, 0, imm_for(rng, w)))
        elif k < 15:
            prog.append(("in", reg(), 0, 0, 0, rng.below(5)))
        elif k < 16:
            prog.append(("sel", reg(), reg(), reg(), reg(), 0))
        else:
            op = ("jmp", "jz", "jnz")[rng.below(3)]
            t = pc + 1 + rng.below(n - 1 - pc)
            prog.append((op, 0, reg() if op != "jmp" else 0, 0, 0, t))
    prog.append(("ret", 0, reg(), 0, 0, 0))
    assert R.verify(prog, w) is None, prog
    return prog


def input_vectors(rng, w):
    m = (1 << w) - 1
    pool = [0, 1, 2, 3, m, m - 1, m >> 1, (m >> 1) + 1, 0x80, 0xFF, 0x100]
    vs = []
    for _ in range(12):
        v = []
        for _ in range(3):
            k = rng.below(4)
            v.append(pool[rng.below(len(pool))] if k < 2 else
                     rng.next() & m if k == 2 else rng.next())
        vs.append(v)
    return vs


def bad_cases(rng, valid):
    """Corrupted encodings labelled with the reference decoder's verdict."""
    cases = []
    for w, prog in valid:
        data = R.encode(prog)
        n = len(prog)
        pick = lambda: rng.below(n)
        mutants = []
        # Whole-encoding damage.
        mutants.append((w, data[: len(data) - 1 - rng.below(15)]))
        mutants.append((16, data))
        bad_op = bytearray(data)
        bad_op[16 * pick()] = 30 + rng.below(226)
        mutants.append((w, bytes(bad_op)))
        pad = bytearray(data)
        pad[16 * pick() + 5 + rng.below(3)] = 1 + rng.below(255)
        mutants.append((w, bytes(pad)))
        # Instruction-level damage, re-encoded.
        def with_insn(pc, insn):
            q = list(prog)
            q[pc] = insn
            return (w, R.encode(q))
        pc = pick()
        op, d, a, b, c, imm = prog[pc]
        mutants.append((w, R.encode(prog[:-1] + [("mov", 0, 0, 0, 0, 0)])))
        f = rng.below(4)
        if R.used_fields(op)[f]:
            fields = [d, a, b, c]
            fields[f] = 16 + rng.below(240)
            mutants.append(with_insn(pc, (op, *fields, imm)))
        else:
            fields = [d, a, b, c]
            fields[f] = 1 + rng.below(15)
            mutants.append(with_insn(pc, (op, *fields, imm)))
        mutants.append(with_insn(pc, ("in", 0, 0, 0, 0, 8 + rng.below(1000))))
        mutants.append(with_insn(pc, ("jz", 0, 0, 0, 0, rng.below(pc + 1))))
        mutants.append(with_insn(pc, ("jnz", 0, 0, 0, 0, n + rng.below(50))))
        if w == 8:
            mutants.append(with_insn(pc, ("const", 0, 0, 0, 0, 0x100 + rng.below(1 << 40))))
        mutants.append(with_insn(pc, ("add", 0, 0, 0, 0, 1 + rng.below(255))))
        for mw, mdata in mutants:
            prog2, err = R.decode(mdata, mw)
            if err is not None:
                cases.append((mw, mdata, err))
    return cases


def programs_lines():
    rng = Rng(0x0123456789ABCDEF)
    lines = ["# NIR reference programs. prog <width> <hex of the 16-byte encoding>;",
             "# v <inputs, hex, comma separated> <result hex> <steps>;",
             "# bad <width> <hex> <error kind>."]
    valid = []
    for w in (8, 64):
        for _ in range(100):
            prog = gen_program(rng, w)
            valid.append((w, prog))
            lines.append("prog %d %s" % (w, R.encode(prog).hex()))
            for v in input_vectors(rng, w):
                out, steps = R.run(prog, w, v)
                lines.append("v %s %x %d" % (",".join("%x" % x for x in v), out, steps))
    kinds = set()
    short = [(w, p) for w, p in valid if len(p) <= 10]
    short = [x for x in short if x[0] == 8][:25] + [x for x in short if x[0] == 64][:25]
    for w, data, err in bad_cases(rng, short):
        kinds.add(err)
        lines.append("bad %d %s %s" % (w, data.hex(), err))
    want = {"length", "op", "padding", "width", "noret", "register", "input",
            "jump", "immediate", "canonical"}
    assert kinds >= want, want - kinds
    return lines


def main():
    check = "--check" in sys.argv
    files = {"alu.txt": alu_lines(), "programs.txt": programs_lines()}
    bad = 0
    for name, lines in files.items():
        text = "\n".join(lines) + "\n"
        path = os.path.join(FIX, name)
        if check:
            same = os.path.exists(path) and open(path, encoding="utf-8").read() == text
            print("%s: %s" % (name, "ok" if same else "DIFFERS"))
            bad += not same
        else:
            os.makedirs(FIX, exist_ok=True)
            open(path, "w", encoding="utf-8", newline="\n").write(text)
            print("wrote %s (%d lines)" % (path, len(lines)))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
