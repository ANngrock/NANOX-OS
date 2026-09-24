"""Tests of the host-side store implementation (tools/store/nxstore.py) and
cross-checks with lib/store.c through out/host/storetool (M4).

The two implementations share no code; they must agree on the format
(each reads what the other wrote) and on the recovery decisions for the
same damaged images."""

import os
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "store"))

import nxstore  # noqa: E402

STORETOOL = REPO / "out" / "host" / "storetool"
DATA_IMG = REPO / "out" / "data.img"


def storetool(*args):
    r = subprocess.run([str(STORETOOL)] + [str(a) for a in args], stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, check=True)
    return r.stdout.decode().strip()


def mount_fields(line):
    parts = line.split()
    out = {"result": parts[1]}
    out.update(p.split("=", 1) for p in parts[2:])
    return out


class FormatTest(unittest.TestCase):
    def test_empty_store_layout(self):
        img = nxstore.format_image(blocks=64, retain=3, store_id=0xABCD)
        self.assertEqual(len(img), 64 * 4096)
        sb = img[:4096]
        self.assertEqual(sb[:8], b"NXSTSUP1")
        self.assertEqual(struct.unpack_from("<I", sb, 4092)[0], zlib.crc32(sb[:4092]))
        self.assertEqual(img[4096:8192], bytes(4096))  # slot B empty
        m = nxstore.mount(nxstore.Image(img))
        self.assertEqual(m["slot"], 0)
        self.assertEqual(m["root"]["gen"], 1)
        self.assertEqual(m["root"]["retain"], 3)
        self.assertEqual(m["root"]["label"], "format")
        self.assertEqual(m["slots"][1]["state"], "empty")
        rep, problems = nxstore.check(nxstore.Image(img))
        self.assertEqual(problems, [])
        self.assertEqual(rep["free_blocks"], 64 - 3)

    def test_format_is_deterministic(self):
        self.assertEqual(nxstore.format_image(), nxstore.format_image())

    def test_build_image_matches(self):
        if not DATA_IMG.is_file():
            self.skipTest("out/data.img not built")
        self.assertEqual(DATA_IMG.read_bytes(), nxstore.format_image(blocks=256, retain=4))

    def test_bad_parameters(self):
        with self.assertRaises(ValueError):
            nxstore.format_image(blocks=8)
        with self.assertRaises(ValueError):
            nxstore.format_image(retain=1)


class BlobTest(unittest.TestCase):
    def test_pattern_matches_the_c_definition(self):
        # abi/nanox/m4.h: nx_m4_name_hash("b1") and the first bytes of a blob
        # of 9000 bytes, computed independently here.
        h = 2166136261
        for c in b"b1":
            h = ((h ^ c) * 16777619) & 0xFFFFFFFF
        self.assertEqual(nxstore.name_hash("b1"), h)
        data = bytes(nxstore.blob_byte(h, 9000, i) for i in range(9000))
        self.assertTrue(nxstore.blob_ok("b1", data))
        self.assertFalse(nxstore.blob_ok("b1", data[:-1] + bytes([data[-1] ^ 1])))


@unittest.skipUnless(STORETOOL.is_file(), "out/host/storetool not built")
class CrossTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "s.img"

    def tearDown(self):
        self.tmp.cleanup()

    def test_c_reads_python_format(self):
        self.path.write_bytes(nxstore.format_image(blocks=64, retain=3))
        f = mount_fields(storetool("mount", self.path))
        self.assertEqual((f["result"], f["gen"], f["slot"], f["check"]), ("ok", "1", "0", "ok"))

    def test_python_reads_c_store(self):
        storetool("sample", self.path)
        img = nxstore.Image(self.path.read_bytes())
        m = nxstore.mount(img)
        r = m["root"]
        # storetool sample: gens 2 ("one"), 3 ("two", pin p -> 2), 4..7 ("more"), 8 ("last").
        self.assertEqual(r["gen"], 8)
        self.assertEqual(r["label"], "last")
        self.assertEqual([l["label"] for l in r["log"]],
                         ["last", "more", "more", "more", "more", "two", "one", "format"])
        self.assertEqual([h["gen"] for h in r["hist"]], [7, 6])
        self.assertEqual([(p["name"], p["gen"]) for p in r["pins"]], [("p", 2)])
        objs = {o["name"]: o for o in r["objects"]}
        self.assertEqual(sorted(objs), ["blob/y", "cfg/a", "cfg/b", "empty"])
        self.assertEqual(objs["cfg/a"]["version"], 2)
        self.assertEqual(objs["cfg/a"]["len"], 4096)
        self.assertEqual(objs["blob/y"]["nblk"], 4)
        data, ok = nxstore.object_data(img, objs["blob/y"])
        self.assertTrue(ok)
        self.assertEqual(data, bytes((5 + i * 7) & 0xFF for i in range(3 * 4096 + 1)))
        rep, problems = nxstore.check(img)
        self.assertEqual(problems, [])
        # The pinned generation 2 is retained with the deleted blob.
        g2, why = nxstore.load_root(img, r["pins"][0]["blk"], 2, r["pins"][0]["crc"])
        self.assertIsNone(why)
        self.assertIn("blob/x", [o["name"] for o in g2["objects"]])
        self.assertEqual(rep["roots"], 4)
        c = mount_fields(storetool("mount", self.path))
        self.assertEqual((c["gen"], c["slot"], c["check"]), ("8", str(m["slot"]), "ok"))

    def test_same_recovery_decisions(self):
        """Damage the same image in several ways; both implementations must
        mount the same generation from the same slot (or refuse)."""
        storetool("sample", self.path)
        base = self.path.read_bytes()
        m = nxstore.mount(nxstore.Image(base))
        cur_root = m["root"]["blk"]
        cur_slot = m["slot"]
        # blob/y exists only in the newest generation (cfg/a is shared with
        # the older one, so damaging it would leave nothing to fall back to).
        blob = [o for o in m["root"]["objects"] if o["name"] == "blob/y"][0]

        def flip(data, blk, off):
            b = bytearray(data)
            b[blk * 4096 + off] ^= 0x40
            return bytes(b)

        def torn_super(data, slot):
            b = bytearray(data)
            b[slot * 4096 + 2048:(slot + 1) * 4096] = bytes(2048)
            return bytes(b)

        cases = {
            "intact": base,
            "current root": flip(base, cur_root, 1000),
            "current data": flip(base, blob["blk"], 5),
            "torn current superblock": torn_super(base, cur_slot),
            "both superblocks": flip(flip(base, 0, 100), 1, 100),
            "older superblock": flip(base, 1 - cur_slot, 100),
        }
        expected_gen = {"intact": 8, "current root": 7, "current data": 7,
                        "torn current superblock": 7, "both superblocks": None,
                        "older superblock": 8}
        for name, data in cases.items():
            with self.subTest(name):
                self.path.write_bytes(data)
                py = nxstore.mount(nxstore.Image(data))
                c = mount_fields(storetool("mount", self.path))
                if expected_gen[name] is None:
                    self.assertIsNone(py["root"])
                    self.assertEqual(c["result"], "no_valid_root")
                    continue
                self.assertEqual(py["root"]["gen"], expected_gen[name])
                self.assertEqual(c["result"], "ok")
                self.assertEqual(int(c["gen"]), py["root"]["gen"])
                self.assertEqual(int(c["slot"]), py["slot"])
                self.assertEqual(c["slots"].split(","),
                                 [s["state"] for s in py["slots"]])
                self.assertEqual(c["check"], "ok")

    def test_corrupt_helper(self):
        storetool("sample", self.path)
        data = self.path.read_bytes()
        new, what = nxstore.corrupt(data, "current-root")
        self.assertIn("generation 8", what)
        self.assertEqual(nxstore.mount(nxstore.Image(new))["root"]["gen"], 7)
        new, _ = nxstore.corrupt(data, "both-supers")
        self.assertIsNone(nxstore.mount(nxstore.Image(new))["root"])
        new, _ = nxstore.corrupt(data, "current-super")
        self.assertEqual(nxstore.mount(nxstore.Image(new))["root"]["gen"], 7)


class CheckTest(unittest.TestCase):
    def test_overlapping_extents_detected(self):
        """A root whose object points into another object's extent."""
        img = bytearray(nxstore.format_image(blocks=32, retain=2))
        root_blk = 2
        blk = bytearray(img[root_blk * 4096:(root_blk + 1) * 4096])
        # Two objects of 2 blocks at blocks 10-11 and 11-12 (overlap).
        data = bytes(8192)
        crc = zlib.crc32(data) & 0xFFFFFFFF
        for i, (b, name) in enumerate(((10, b"o1"), (11, b"o2"))):
            struct.pack_into(nxstore.OBJ_FMT, blk, nxstore.OFF_OBJ + 64 * i, i + 1, 1, 1, b,
                             8192, crc, 1, 2, name)
        struct.pack_into("<Q", blk, 32, 3)          # next_oid
        struct.pack_into("<I", blk, 48, 2)          # nobj
        struct.pack_into("<I", blk, 4092, zlib.crc32(bytes(blk[:4092])) & 0xFFFFFFFF)
        img[root_blk * 4096:(root_blk + 1) * 4096] = blk
        root = nxstore.parse_root(bytes(blk))
        self.assertIn("object 'o2'", nxstore.root_problems(root, 32))


if __name__ == "__main__":
    unittest.main()
