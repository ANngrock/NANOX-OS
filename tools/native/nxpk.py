#!/usr/bin/env python3
"""NXPK: the archive a cross-built set of native programs is shipped in
(docs/specs/M10-ROUTES.md, route C). One canonical encoding per content, so
the same programs always give the same bytes, and a reader that accepts an
archive has accepted the only encoding of it.

Layout (little-endian):

    0   magic "NXPK"
    4   version u16 = 1
    6   flags   u16 = 0
    8   count   u32
    12  dir_len u32            bytes of the directory that follows
    16  directory: `count` entries sorted by name (bytes), each
            name_len u8 (1..=64), name (A-Z a-z 0-9 . _ -), kind u8 (1 = program),
            mode u16 (permission bits), size u64, offset u64, sha256 [32]
    ..  data: every blob at its offset, padded with zeros to 16 bytes;
        the first starts at align16(16 + dir_len), the next at the aligned end
        of the one before
    ..  trailer: sha256 of every byte before it (the last 32 bytes)

The Rust reader is crates/runtime/src/pkg.rs; both are tested against the
same fixture (crates/runtime/tests/data/sample.nxpk).
"""
import hashlib
import re
import struct
import sys

MAGIC = b"NXPK"
VERSION = 1
KIND_PROGRAM = 1
MAX_ENTRIES = 256
NAME_RE = re.compile(rb"^[A-Za-z0-9._-]{1,64}$")
ALIGN = 16


class PackError(Exception):
    pass


def align(n):
    return (n + ALIGN - 1) & ~(ALIGN - 1)


def build(entries):
    """entries: list of (name: str, mode: int, data: bytes). Returns the archive bytes."""
    if len(entries) > MAX_ENTRIES:
        raise PackError("too many entries")
    items = sorted(((n.encode(), m, d) for n, m, d in entries), key=lambda e: e[0])
    for i, (name, mode, _) in enumerate(items):
        if not NAME_RE.match(name):
            raise PackError("bad name %r" % name)
        if i and items[i - 1][0] == name:
            raise PackError("duplicate name %r" % name)
        if not 0 <= mode <= 0o7777:
            raise PackError("bad mode")
    dir_len = sum(1 + len(n) + 1 + 2 + 8 + 8 + 32 for n, _, _ in items)
    at = align(16 + dir_len)
    directory = b""
    blobs = b""
    for name, mode, data in items:
        directory += struct.pack("<B", len(name)) + name
        directory += struct.pack("<BHQQ", KIND_PROGRAM, mode, len(data), at)
        directory += hashlib.sha256(data).digest()
        pad = align(len(data)) - len(data)
        blobs += data + b"\0" * pad
        at += len(data) + pad
    head = MAGIC + struct.pack("<HHII", VERSION, 0, len(items), dir_len)
    body = head + directory
    body += b"\0" * (align(len(body)) - len(body)) + blobs
    return body + hashlib.sha256(body).digest()


def parse(blob):
    """Validates the structure and every hash; returns [(name, mode, data)]. Raises PackError."""
    if len(blob) < 16 + 32:
        raise PackError("too short")
    if blob[:4] != MAGIC:
        raise PackError("bad magic")
    version, flags, count, dir_len = struct.unpack_from("<HHII", blob, 4)
    if version != VERSION:
        raise PackError("unknown version")
    if flags != 0:
        raise PackError("unknown flags")
    if count > MAX_ENTRIES:
        raise PackError("too many entries")
    body_end = len(blob) - 32
    if blob[body_end:] != hashlib.sha256(blob[:body_end]).digest():
        raise PackError("trailer hash mismatch")
    pos, end = 16, 16 + dir_len
    if end > body_end:
        raise PackError("directory beyond the archive")
    at = align(end)
    if blob[end:at] != b"\0" * (at - end):
        raise PackError("directory padding not zero")
    out, prev = [], None
    for _ in range(count):
        if pos >= end:
            raise PackError("directory too short")
        n = blob[pos]
        name = blob[pos + 1:pos + 1 + n]
        pos += 1 + n
        if not NAME_RE.match(name):
            raise PackError("bad name")
        if prev is not None and name <= prev:
            raise PackError("names not strictly increasing")
        prev = name
        if pos + 1 + 2 + 8 + 8 + 32 > end:
            raise PackError("directory entry cut short")
        kind, mode, size, offset = struct.unpack_from("<BHQQ", blob, pos)
        digest = blob[pos + 19:pos + 51]
        pos += 51
        if kind != KIND_PROGRAM:
            raise PackError("unknown kind")
        if mode > 0o7777:
            raise PackError("bad mode")
        if offset != at:
            raise PackError("data not where the canonical layout puts it")
        stop = offset + size
        if stop > body_end:
            raise PackError("data beyond the archive")
        data = blob[offset:stop]
        if hashlib.sha256(data).digest() != digest:
            raise PackError("entry hash mismatch")
        pad_end = align(stop)
        if pad_end > body_end or blob[stop:pad_end] != b"\0" * (pad_end - stop):
            raise PackError("data padding not zero")
        at = pad_end
        out.append((name.decode(), mode, data))
    if pos != end:
        raise PackError("directory length does not match its entries")
    if at != body_end:
        raise PackError("bytes after the last entry")
    return out


# The fixture both readers are tested against: an empty program, one whose
# size is not a multiple of 16, and one that is not.
SAMPLE = [
    ("zeta", 0o644, b""),
    ("hello", 0o755, bytes(range(1, 41))),
    ("alpha.bin", 0o755, bytes((i * 7) % 256 for i in range(96))),
]


def main(argv):
    if len(argv) == 3 and argv[1] == "sample":
        with open(argv[2], "wb") as f:
            f.write(build(SAMPLE))
        return 0
    if len(argv) == 3 and argv[1] == "list":
        with open(argv[2], "rb") as f:
            blob = f.read()
        for name, mode, data in parse(blob):
            print("%o %8d %s %s" % (mode, len(data), hashlib.sha256(data).hexdigest(), name))
        print("archive", blob[-32:].hex(), "(the trailer: the hash of everything before it)")
        return 0
    print("usage: nxpk.py list ARCHIVE | sample OUT")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
