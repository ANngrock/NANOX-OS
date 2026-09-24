"""Tests for the deterministic initramfs writer (tools/image/mkinitrd.py)."""

import os
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "image"))

import mkinitrd  # noqa: E402


def parse_newc(data):
    """Minimal independent newc reader: returns [(name, mode, bytes)] up to the trailer."""
    out, off = [], 0
    while True:
        hdr = data[off:off + 110]
        assert hdr[:6] == b"070701", hdr[:6]
        f = [int(hdr[6 + 8 * i:14 + 8 * i], 16) for i in range(13)]
        mode, size, namesize = f[1], f[6], f[11]
        name = data[off + 110:off + 110 + namesize - 1].decode()
        assert data[off + 110 + namesize - 1] == 0
        doff = (off + 110 + namesize + 3) & ~3
        if name == "TRAILER!!!":
            return out, doff + size
        out.append((name, mode, data[doff:doff + size]))
        off = (doff + size + 3) & ~3


class MkinitrdTest(unittest.TestCase):
    def make_tree(self, d):
        os.makedirs(os.path.join(d, "etc", "nanox"))
        Path(d, "etc", "nanox", "release").write_bytes(b"R\n")
        Path(d, "b.txt").write_bytes(b"x" * 5)

    def test_layout_and_determinism(self):
        with tempfile.TemporaryDirectory() as d:
            self.make_tree(d)
            a = mkinitrd.build_cpio(mkinitrd.collect(d))
            os.utime(os.path.join(d, "b.txt"), (0, 12345678))  # host mtimes do not leak
            b = mkinitrd.build_cpio(mkinitrd.collect(d))
            self.assertEqual(a, b)
            entries, end = parse_newc(a)
            self.assertEqual(end, len(a))
            self.assertEqual([(n, m) for n, m, _ in entries],
                             [("b.txt", 0o100644), ("etc", 0o040755), ("etc/nanox", 0o040755),
                              ("etc/nanox/release", 0o100644)])
            self.assertEqual(entries[3][2], b"R\n")
            self.assertEqual(len(a) % 4, 0)

    def test_rejects_symlink(self):
        with tempfile.TemporaryDirectory() as d:
            self.make_tree(d)
            os.symlink("b.txt", os.path.join(d, "link"))
            with self.assertRaises(ValueError):
                mkinitrd.collect(d)

    def test_repository_initrd(self):
        entries, _ = parse_newc(mkinitrd.build_cpio(mkinitrd.collect(REPO / "initrd")))
        names = [n for n, _, _ in entries]
        self.assertIn("etc/nanox/release", names)


if __name__ == "__main__":
    unittest.main()
