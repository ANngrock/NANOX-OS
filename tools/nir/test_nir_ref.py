import os
import subprocess
import sys
import unittest

import gen_fixtures as G
import nir_ref as R


class Semantics(unittest.TestCase):
    """Hand-computed cases straight from the specification."""

    def test_total_division_and_remainder(self):
        self.assertEqual(R.alu("udiv", 5, 0, 8), 0xFF)
        self.assertEqual(R.alu("udiv", 5, 0, 64), (1 << 64) - 1)
        self.assertEqual(R.alu("urem", 5, 0, 8), 5)
        self.assertEqual(R.alu("udiv", 200, 7, 8), 28)
        self.assertEqual(R.alu("urem", 200, 7, 8), 4)

    def test_shifts_are_modulo_the_width(self):
        self.assertEqual(R.alu("shl", 1, 9, 8), 2)
        self.assertEqual(R.alu("shl", 1, 64, 64), 1)
        self.assertEqual(R.alu("shr", 0x80, 15, 8), 1)
        self.assertEqual(R.alu("sar", 0x80, 1, 8), 0xC0)
        self.assertEqual(R.alu("sar", 0x7F, 7, 8), 0)
        self.assertEqual(R.alu("sar", 1 << 63, 63, 64), (1 << 64) - 1)

    def test_signed_and_unsigned_order_differ(self):
        self.assertEqual(R.alu("lts", 0x80, 0x7F, 8), 1)
        self.assertEqual(R.alu("ltu", 0x80, 0x7F, 8), 0)
        self.assertEqual(R.alu("les", 0x80, 0x80, 8), 1)

    def test_wrapping_arithmetic(self):
        self.assertEqual(R.alu("add", 0xFF, 1, 8), 0)
        self.assertEqual(R.alu("sub", 0, 1, 8), 0xFF)
        self.assertEqual(R.alu("mul", 0x10, 0x10, 8), 0)
        self.assertEqual(R.alu("neg", 0x80, 0, 8), 0x80)
        self.assertEqual(R.alu("not", 0, 0, 64), (1 << 64) - 1)
        self.assertEqual(R.alu("popcnt", 0xFF, 0, 8), 8)

    def test_run_registers_start_at_zero_and_inputs_are_cut(self):
        prog = [("ret", 0, 5, 0, 0, 0)]
        self.assertEqual(R.run(prog, 8, []), (0, 1))
        prog = [("in", 1, 0, 0, 0, 0), ("in", 2, 0, 0, 0, 5),
                ("add", 3, 1, 2, 0, 0), ("ret", 0, 3, 0, 0, 0)]
        self.assertEqual(R.run(prog, 8, [0x123])[0], 0x23)

    def test_forward_jumps_and_select(self):
        prog = [("in", 1, 0, 0, 0, 0), ("jz", 0, 1, 0, 0, 3),
                ("const", 2, 0, 0, 0, 7), ("sel", 3, 2, 1, 1, 0),
                ("ret", 0, 3, 0, 0, 0)]
        self.assertEqual(R.verify(prog, 8), None)
        self.assertEqual(R.run(prog, 8, [0]), (0, 4))
        self.assertEqual(R.run(prog, 8, [9]), (7, 5))


class Verifier(unittest.TestCase):
    RET = ("ret", 0, 0, 0, 0, 0)

    def test_kinds(self):
        r = self.RET
        self.assertEqual(R.verify([r], 16), "width")
        self.assertEqual(R.verify([], 8), "empty")
        self.assertEqual(R.verify([r] * 257, 8), "toolong")
        self.assertEqual(R.verify([("const", 0, 0, 0, 0, 1)], 8), "noret")
        self.assertEqual(R.verify([("mov", 16, 0, 0, 0, 0), r], 8), "register")
        self.assertEqual(R.verify([("in", 0, 0, 0, 0, 8), r], 8), "input")
        self.assertEqual(R.verify([("jmp", 0, 0, 0, 0, 0), r], 8), "jump")
        self.assertEqual(R.verify([("const", 0, 0, 0, 0, 256), r], 8), "immediate")
        self.assertEqual(R.verify([("const", 0, 0, 0, 0, 255), r], 8), None)
        self.assertEqual(R.verify([("add", 0, 0, 0, 1, 0), r], 8), "canonical")

    def test_decode_kinds(self):
        good = R.encode([self.RET])
        self.assertEqual(R.decode(good, 8)[1], None)
        self.assertEqual(R.decode(b"", 8)[1], "length")
        self.assertEqual(R.decode(good[:15], 8)[1], "length")
        self.assertEqual(R.decode(bytes([30]) + good[1:], 8)[1], "op")
        self.assertEqual(R.decode(good[:5] + b"\1" + good[6:], 8)[1], "padding")


class Fixtures(unittest.TestCase):
    def test_generator_is_deterministic(self):
        self.assertEqual(G.alu_lines(), G.alu_lines())
        self.assertEqual(G.programs_lines(), G.programs_lines())

    def test_rng_is_xorshift64star(self):
        # seed 1: x = 1 -> 1 ^ (1 << 25) = 0x2000001, times the multiplier.
        self.assertEqual(G.Rng(1).next(), 0x47E4CE4B896CDD1D)
        self.assertEqual(G.Rng(0).next(), G.Rng(1).next(), "a zero seed becomes 1")

    def test_checked_in_fixtures_are_current(self):
        here = os.path.dirname(os.path.abspath(__file__))
        p = subprocess.run([sys.executable, "gen_fixtures.py", "--check"],
                           cwd=here, capture_output=True, text=True)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)


if __name__ == "__main__":
    unittest.main()
