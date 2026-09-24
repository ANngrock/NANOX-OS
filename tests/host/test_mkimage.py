"""Tests for the deterministic image writer (tools/image/mkimage.py).

The FAT32/GPT structures are checked twice: by parsing them here and by
reading the files back with mtools, an independent FAT implementation.
mtools is a pinned tool (toolchain.lock); without it these tests fail.
"""

import hashlib
import os
import struct
import subprocess
import sys
import tempfile
import unittest
import uuid
import zlib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "image"))

import mkimage  # noqa: E402

LOADER = b"MZ" + bytes(range(256)) * 40
KERNEL = b"\x7fELF" + bytes((i * 7) & 0xFF for i in range(70000))


class MkimageTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.img = mkimage.build_image(LOADER, KERNEL, cmdline="nanox.test=pass", timestamp=0)

    def test_deterministic(self):
        again = mkimage.build_image(LOADER, KERNEL, cmdline="nanox.test=pass", timestamp=0)
        self.assertEqual(hashlib.sha256(self.img).digest(), hashlib.sha256(again).digest())

    def test_inputs_change_output(self):
        other = mkimage.build_image(LOADER, KERNEL, cmdline="nanox.test=fail", timestamp=0)
        self.assertNotEqual(self.img, other)
        dated = mkimage.build_image(LOADER, KERNEL, cmdline="nanox.test=pass",
                                    timestamp=1767225600)
        self.assertNotEqual(self.img, dated)

    def test_gpt(self):
        img = self.img
        self.assertEqual(img[510:512], b"\x55\xAA")
        self.assertEqual(img[446 + 4], 0xEE)
        last = len(img) // 512 - 1
        for lba, other, entries_lba in ((1, last, 2), (last, 1, last - 32)):
            hdr = bytearray(img[lba * 512:lba * 512 + 92])
            self.assertEqual(hdr[:8], b"EFI PART")
            crc = struct.unpack_from("<I", hdr, 16)[0]
            struct.pack_into("<I", hdr, 16, 0)
            self.assertEqual(zlib.crc32(bytes(hdr)) & 0xFFFFFFFF, crc)
            my_lba, alt_lba = struct.unpack_from("<QQ", hdr, 24)
            self.assertEqual((my_lba, alt_lba), (lba, other))
            e_lba, n, size, e_crc = struct.unpack_from("<QIII", hdr, 72)
            self.assertEqual(e_lba, entries_lba)
            entries = img[e_lba * 512:e_lba * 512 + n * size]
            self.assertEqual(zlib.crc32(entries) & 0xFFFFFFFF, e_crc)
            self.assertEqual(uuid.UUID(bytes_le=entries[:16]), mkimage.ESP_TYPE_GUID)

    def test_fat32_geometry(self):
        bs = self.img[2048 * 512:2049 * 512]
        self.assertEqual(bs[82:90], b"FAT32   ")
        tot = struct.unpack_from("<I", bs, 32)[0]
        fatsz = struct.unpack_from("<I", bs, 36)[0]
        clusters = tot - 32 - 2 * fatsz
        self.assertGreaterEqual(clusters, 65525)  # FAT32 by cluster count
        self.assertEqual(self.img[(2048 + 6) * 512:(2048 + 7) * 512], bs)  # backup boot

    def test_manifest(self):
        m = mkimage.build_manifest(KERNEL)
        magic, ver, size, ksize, digest = struct.unpack_from("<IHHQ32s", m)
        self.assertEqual((magic, ver, size, ksize), (0x464D584E, 1, 64, len(KERNEL)))
        self.assertEqual(digest, hashlib.sha256(KERNEL).digest())
        self.assertEqual(m[48:], b"\0" * 16)

    def test_mtools_readback(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "t.img")
            Path(path).write_bytes(self.img)
            src = path + "@@1M"

            def read(name):
                return subprocess.run(["mcopy", "-n", "-i", src, "::" + name, "-"],
                                      stdout=subprocess.PIPE, check=True).stdout
            self.assertEqual(read("/EFI/BOOT/BOOTX64.EFI"), LOADER)
            self.assertEqual(read("/NANOX/KERNEL.ELF"), KERNEL)
            self.assertEqual(read("/NANOX/CMDLINE.TXT"), b"nanox.test=pass")
            self.assertEqual(read("/NANOX/MANIFEST.BIN"), mkimage.build_manifest(KERNEL))

    def test_fault_injection(self):
        with tempfile.TemporaryDirectory() as d:
            for kwargs, check in (({"omit_kernel": True}, "omit"),
                                  ({"corrupt_kernel": True}, "corrupt")):
                path = os.path.join(d, check + ".img")
                Path(path).write_bytes(mkimage.build_image(LOADER, KERNEL, timestamp=0, **kwargs))
                r = subprocess.run(["mcopy", "-n", "-i", path + "@@1M", "::/NANOX/KERNEL.ELF",
                                    "-"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                if check == "omit":
                    self.assertNotEqual(r.returncode, 0)
                else:
                    self.assertEqual(len(r.stdout), len(KERNEL))
                    self.assertNotEqual(r.stdout, KERNEL)


if __name__ == "__main__":
    unittest.main()
