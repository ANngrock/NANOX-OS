import os
import random
import struct
import unittest

import check_elf as C

R, W_, X = C.PF_R, C.PF_W, C.PF_X


def make_elf(loads, entry, stack=R | W_, extra=(), e_type=2, machine=62,
             sections=(), size=0x5000):
    """A minimal ELF64 image: header, program headers, sections, padding."""
    phdrs = [(C.PT_LOAD, f, off, va, va, fs, ms, al) for (va, ms, fs, off, f, al) in loads]
    if stack is not None:
        phdrs.append((C.PT_GNU_STACK, stack, 0, 0, 0, 0, 0, 16))
    phdrs += list(extra)
    shoff = 0x4000 if sections else 0
    ehdr = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    ehdr += struct.pack("<HHIQQQIHHHHHH", e_type, machine, 1, entry, 64, shoff, 0, 64, 56,
                        len(phdrs), 64, len(sections), 0)
    body = bytearray(size)
    body[:len(ehdr)] = ehdr
    for i, p in enumerate(phdrs):
        struct.pack_into("<IIQQQQQQ", body, 64 + 56 * i, *p)
    for i, t in enumerate(sections):
        struct.pack_into("<I", body, shoff + 64 * i + 4, t)
    return bytes(body)


GOOD_LOADS = [
    (0x400000, 0x1000, 0x1000, 0x1000, R | X, 0x1000),
    (0x401000, 0x800, 0x800, 0x2000, R, 0x1000),
    (0x402000, 0x3000, 0x100, 0x3000, R | W_, 0x1000),
]


def good(**kw):
    return make_elf(GOOD_LOADS, 0x400010, **kw)


class Contract(unittest.TestCase):
    def test_a_conforming_file_passes(self):
        self.assertEqual(C.check(good()), [])

    def expect(self, data, text):
        bad = C.check(data)
        self.assertTrue(any(text in b for b in bad), "%r not in %r" % (text, bad))

    def test_header_violations(self):
        self.expect(good(e_type=3), "ET_EXEC")
        self.expect(good(machine=183), "x86-64")
        self.assertEqual(C.check(b"hello"), ["not an ELF file"])
        self.assertEqual(C.check(b"\x7fELF\x01\x01" + bytes(60)), ["not ELF64"])
        self.assertEqual(C.check(b"\x7fELF\x02\x02" + bytes(60)), ["not little-endian"])
        self.expect(good(size=0x5000)[:100], "beyond the end")

    def test_segment_violations(self):
        def loads(i, **ch):
            l = list(GOOD_LOADS)
            va, ms, fs, off, f, al = l[i]
            l[i] = (ch.get("va", va), ch.get("ms", ms), ch.get("fs", fs),
                    ch.get("off", off), ch.get("f", f), ch.get("al", al))
            return make_elf(l, 0x400010)

        self.expect(loads(2, f=R | W_ | X), "writable and executable")
        self.expect(loads(1, f=W_), "not readable")
        self.expect(loads(0, va=0x1000, off=0x1000), "outside the user range")
        self.expect(loads(0, va=C.USER_TOP - 0x800, ms=0x1000), "outside the user range")
        self.expect(loads(1, off=0x2010), "disagree modulo")
        self.expect(loads(1, al=8), "alignment")
        self.expect(loads(1, fs=0x900), "file size above memory size")
        self.expect(loads(2, off=0x4F00, fs=0x1000, ms=0x1000), "beyond the end")
        self.expect(loads(1, va=0x400800, off=0x2800), "share a page")
        self.expect(loads(2, ms=C.MAX_MEM + 0x1000), "256 MiB")

    def test_entry_point(self):
        self.expect(make_elf(GOOD_LOADS, 0x401000), "entry point")
        self.expect(make_elf(GOOD_LOADS, 0x1234), "entry point")
        # Inside the executable segment's zero-filled tail is not file-backed code.
        l = [(0x400000, 0x2000, 0x1000, 0x1000, R | X, 0x1000)] + GOOD_LOADS[1:]
        l[1] = (0x402000, 0x800, 0x800, 0x2000, R, 0x1000)
        l[2] = (0x403000, 0x100, 0x100, 0x3000, R | W_, 0x1000)
        self.expect(make_elf(l, 0x401800), "entry point")

    def test_stack_and_forbidden_segments(self):
        self.expect(good(stack=None), "PT_GNU_STACK")
        self.expect(good(stack=R | W_ | X), "executable stack")
        for t, text in [(C.PT_INTERP, "PT_INTERP"), (C.PT_DYNAMIC, "PT_DYNAMIC"),
                        (C.PT_TLS, "PT_TLS"), (C.PT_SHLIB, "PT_SHLIB")]:
            self.expect(good(extra=[(t, R, 0, 0, 0, 0, 0, 8)]), text)
        self.assertEqual(C.check(good(extra=[(C.PT_NOTE, R, 0, 0, 0, 0, 0, 4)])), [])

    def test_sections(self):
        self.expect(good(sections=[0, 1, C.SHT_RELA]), "relocation section")
        self.expect(good(sections=[0, C.SHT_REL]), "relocation section")
        self.expect(good(sections=[0, C.SHT_DYNAMIC]), "dynamic section")
        self.assertEqual(C.check(good(sections=[0, 1, 3, 8])), [])

    def test_size_limit(self):
        self.expect(good() + bytes(C.MAX_FILE), "16 MiB")

    def test_damaged_files_never_raise(self):
        rng = random.Random(7)
        base = good(sections=[0, 1, 2])
        for _ in range(20000):
            b = bytearray(base)
            for _ in range(rng.randint(1, 6)):
                b[rng.randrange(0, 0x4100)] = rng.randrange(256)
            if rng.random() < 0.2:
                b = b[:rng.randrange(len(b))]
            self.assertIsInstance(C.check(bytes(b)), list)


@unittest.skipUnless(os.environ.get("NANOX_HELLO_ELF"), "set NANOX_HELLO_ELF to a built program")
class Built(unittest.TestCase):
    def test_the_built_program_conforms(self):
        data = open(os.environ["NANOX_HELLO_ELF"], "rb").read()
        self.assertEqual(C.check(data), [])


if __name__ == "__main__":
    unittest.main()
