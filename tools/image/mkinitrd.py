#!/usr/bin/env python3
"""Deterministic initramfs writer: cpio "newc" (magic 070701) from a directory.

Every header field is derived from the tree contents only: entries are sorted
by path, inode numbers are sequential, uid/gid are 0, modes are fixed
(directories 040755, files 0100644), mtime is SOURCE_DATE_EPOCH or 0.  Only
regular files and directories are accepted.  Format: docs/m1-kernel.md.

Build outputs that are not in the source tree (the M2 user programs) are
added with --file ARCHIVE_PATH=HOST_FILE; missing parent directories are
created as directory entries.
"""

import argparse
import hashlib
import os
import stat
import sys

MAGIC = b"070701"
TRAILER = "TRAILER!!!"


def _pad4(n):
    return (4 - n % 4) % 4


def _entry(ino, mode, nlink, mtime, name, data):
    encoded = name.encode("ascii") + b"\0"
    fields = [ino, mode, 0, 0, nlink, mtime, len(data), 0, 0, 0, 0, len(encoded), 0]
    header = MAGIC + b"".join(b"%08X" % f for f in fields)
    out = header + encoded
    out += b"\0" * _pad4(len(out))
    out += data + b"\0" * _pad4(len(data))
    return out


def add_files(items, files):
    """Adds [(archive path, host file)] to items (as returned by collect)."""
    have = {name for name, _ in items}
    for name, src in files:
        parts = name.split("/")
        if not name or name.startswith("/") or "" in parts or "." in parts or ".." in parts:
            raise ValueError("%s: archive path must be relative and normalised" % name)
        if name in have:
            raise ValueError("%s: already in the archive" % name)
        for i in range(1, len(parts)):
            parent = "/".join(parts[:i])
            if parent not in have:
                items.append((parent, None))
                have.add(parent)
        with open(src, "rb") as f:
            items.append((name, f.read()))
        have.add(name)
    items.sort(key=lambda it: it[0])
    return items


def collect(root):
    """Returns sorted [(relative path, bytes or None for a directory)]."""
    items = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        rel_dir = os.path.relpath(dirpath, root)
        if rel_dir != ".":
            items.append((rel_dir.replace(os.sep, "/"), None))
        for fn in sorted(filenames):
            path = os.path.join(dirpath, fn)
            st = os.lstat(path)
            if not stat.S_ISREG(st.st_mode):
                raise ValueError("%s: only regular files and directories are allowed" % path)
            rel = os.path.relpath(path, root).replace(os.sep, "/")
            with open(path, "rb") as f:
                items.append((rel, f.read()))
    items.sort(key=lambda it: it[0])
    return items


def build_cpio(items, mtime=0):
    out = bytearray()
    for ino, (name, data) in enumerate(items, 1):
        if data is None:
            out += _entry(ino, 0o040755, 2, mtime, name, b"")
        else:
            out += _entry(ino, 0o100644, 1, mtime, name, data)
    out += _entry(0, 0, 1, 0, TRAILER, b"")
    return bytes(out)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--file", action="append", default=[], metavar="ARCHIVE_PATH=HOST_FILE",
                    help="add a file that is not under --root (repeatable)")
    args = ap.parse_args(argv)
    files = []
    for spec in args.file:
        name, sep, src = spec.partition("=")
        if not sep or not src:
            ap.error("--file expects ARCHIVE_PATH=HOST_FILE, got %r" % spec)
        files.append((name, src))
    mtime = int(os.environ.get("SOURCE_DATE_EPOCH") or 0)
    data = build_cpio(add_files(collect(args.root), files), mtime)
    tmp = args.out + ".tmp"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, args.out)
    print("mkinitrd: %s %d bytes sha256=%s" % (args.out, len(data),
                                                hashlib.sha256(data).hexdigest()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
