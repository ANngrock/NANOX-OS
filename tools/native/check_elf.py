#!/usr/bin/env python3
"""Checks a native userspace ELF against the loader contract
(docs/specs/M10-NATIVE.md, section 3). Prints the violations and exits 1 if
there are any.

    check_elf.py PROGRAM.elf
"""
import struct
import sys

USER_BASE = 0x400000
USER_TOP = 0x0000_7FFF_FFFF_0000
PAGE = 4096
MAX_FILE = 16 << 20
MAX_MEM = 256 << 20
ET_EXEC, EM_X86_64 = 2, 62
PT_NULL, PT_LOAD, PT_DYNAMIC, PT_INTERP, PT_NOTE, PT_SHLIB, PT_PHDR, PT_TLS = range(8)
PT_GNU_EH_FRAME, PT_GNU_STACK, PT_GNU_RELRO = 0x6474E550, 0x6474E551, 0x6474E552
PF_X, PF_W, PF_R = 1, 2, 4
SHT_RELA, SHT_DYNAMIC, SHT_REL = 4, 6, 9
FORBIDDEN = {
    PT_INTERP: "PT_INTERP (a dynamic loader)",
    PT_DYNAMIC: "PT_DYNAMIC (dynamic linking)",
    PT_SHLIB: "PT_SHLIB",
    PT_TLS: "PT_TLS (thread-local storage is not supported yet)",
}


def align_up(x, a):
    return (x + a - 1) // a * a


def check(data):
    """Returns the list of contract violations (empty: the file conforms)."""
    if len(data) < 64 or data[:4] != b"\x7fELF":
        return ["not an ELF file"]
    bad = []
    if len(data) > MAX_FILE:
        bad.append("file larger than 16 MiB")
    if data[4] != 2:
        return bad + ["not ELF64"]
    if data[5] != 1:
        return bad + ["not little-endian"]
    (e_type, e_machine, e_version, e_entry, e_phoff, e_shoff, _flags, _ehsize,
     e_phentsize, e_phnum, e_shentsize, e_shnum, _shstrndx) = struct.unpack_from(
        "<HHIQQQIHHHHHH", data, 16)
    if e_type != ET_EXEC:
        bad.append("not a static executable (ET_EXEC): type %d" % e_type)
    if e_machine != EM_X86_64:
        bad.append("not x86-64: machine %d" % e_machine)
    if e_version != 1:
        bad.append("ELF version %d" % e_version)
    if e_phentsize != 56 or not 1 <= e_phnum <= 16:
        return bad + ["bad program header table: entry size %d, %d entries" % (e_phentsize, e_phnum)]
    if e_phoff + e_phnum * e_phentsize > len(data):
        return bad + ["program header table beyond the end of the file"]
    phdrs = [struct.unpack_from("<IIQQQQQQ", data, e_phoff + i * e_phentsize)
             for i in range(e_phnum)]
    loads, stack = [], None
    for (p_type, flags, offset, vaddr, _paddr, filesz, memsz, align) in phdrs:
        if p_type in FORBIDDEN:
            bad.append("forbidden segment " + FORBIDDEN[p_type])
        elif p_type == PT_GNU_STACK:
            stack = flags
        elif p_type == PT_LOAD:
            loads.append((vaddr, memsz, filesz, offset, flags, align))
    if stack is None:
        bad.append("no PT_GNU_STACK: the stack would default to executable")
    elif stack & PF_X:
        bad.append("executable stack")
    if not loads:
        return bad + ["no PT_LOAD segment"]
    total = 0
    for (vaddr, memsz, filesz, offset, flags, align) in loads:
        name = "segment at %#x" % vaddr
        if filesz > memsz:
            bad.append(name + ": file size above memory size")
        if vaddr < USER_BASE or vaddr + memsz > USER_TOP:
            bad.append(name + ": outside the user range %#x..%#x" % (USER_BASE, USER_TOP))
        if vaddr % PAGE != offset % PAGE:
            bad.append(name + ": address and file offset disagree modulo the page size")
        if align % PAGE != 0:
            bad.append(name + ": alignment %d is not a multiple of %d" % (align, PAGE))
        if offset + filesz > len(data):
            bad.append(name + ": file-backed part beyond the end of the file")
        if flags & PF_W and flags & PF_X:
            bad.append(name + ": writable and executable")
        if not flags & PF_R:
            bad.append(name + ": not readable")
        total += memsz
    if total > MAX_MEM:
        bad.append("loaded size above 256 MiB")
    loads.sort()
    for a, b in zip(loads, loads[1:]):
        if align_up(a[0] + a[1], PAGE) > b[0]:
            bad.append("segments at %#x and %#x share a page" % (a[0], b[0]))
    if not any(f & PF_X and vaddr <= e_entry < vaddr + filesz
               for (vaddr, _m, filesz, _o, f, _a) in loads):
        bad.append("entry point %#x is not inside an executable, file-backed segment" % e_entry)
    # Relocations and dynamic sections, if the section table is there.
    if e_shnum and e_shentsize == 64 and e_shoff + e_shnum * 64 <= len(data):
        for i in range(e_shnum):
            sh_type = struct.unpack_from("<I", data, e_shoff + i * 64 + 4)[0]
            if sh_type in (SHT_RELA, SHT_REL):
                bad.append("relocation section %d (the program must be fully linked)" % i)
            elif sh_type == SHT_DYNAMIC:
                bad.append("dynamic section %d" % i)
    return bad


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    data = open(sys.argv[1], "rb").read()
    bad = check(data)
    for b in bad:
        print("VIOLATION:", b)
    if not bad:
        print("%s: conforms (%d bytes)" % (sys.argv[1], len(data)))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
