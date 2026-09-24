#!/usr/bin/env python3
"""Minimal ELF64 symbol table reader for symbolising kernel addresses.

Standard library only.  Reads .symtab/.strtab of out/kernel.elf and maps an
address to "function+0xoffset".  Usage: elfsym.py KERNEL.ELF ADDR...
"""

import bisect
import struct
import sys

STT_FUNC = 2
STT_NOTYPE = 0


class Symbolizer:
    def __init__(self, path):
        with open(path, "rb") as f:
            data = f.read()
        if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
            raise ValueError("%s: not a little-endian ELF64 file" % path)
        shoff, = struct.unpack_from("<Q", data, 0x28)
        shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
        sections = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize)
                    for i in range(shnum)]
        funcs = []
        for sh in sections:
            if sh[1] != 2:  # SHT_SYMTAB
                continue
            _, _, _, _, off, size, link, _, _, entsize = sh
            str_off = sections[link][4]
            for i in range(size // entsize):
                name_off, info, _, shndx, value, sym_size = struct.unpack_from(
                    "<IBBHQQ", data, off + i * entsize)
                kind = info & 0xF
                # Skip undefined and absolute symbols (e.g. assembler counters).
                if kind not in (STT_FUNC, STT_NOTYPE) or shndx in (0, 0xFFF1):
                    continue
                end = data.index(b"\0", str_off + name_off)
                name = data[str_off + name_off:end].decode("ascii", "replace")
                if not name or name.startswith("."):
                    continue
                funcs.append((value, sym_size, name, kind))
        # At equal addresses the function symbol sorts last and wins.
        funcs.sort(key=lambda s: (s[0], s[3] == STT_FUNC))
        self._syms = funcs
        self._addrs = [s[0] for s in funcs]

    def lookup(self, addr):
        """Returns (name, offset) of the nearest symbol at or below addr, or None
        when that symbol is a sized function that ends before addr."""
        i = bisect.bisect_right(self._addrs, addr) - 1
        if i < 0:
            return None
        value, size, name, kind = self._syms[i]
        if kind == STT_FUNC and size and addr >= value + size:
            return None
        return name, addr - value

    def describe(self, addr):
        hit = self.lookup(addr)
        return "%s+0x%x" % hit if hit else None


def main(argv):
    sym = Symbolizer(argv[1])
    for a in argv[2:]:
        print("%s %s" % (a, sym.describe(int(a, 16)) or "?"))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
