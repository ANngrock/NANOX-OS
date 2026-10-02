#!/usr/bin/env python3
"""Tests of the NXPK archive: one encoding per content, and every damaged
archive is refused. Run: python3 -m unittest tools/native/test_nxpk.py"""
import hashlib
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import nxpk  # noqa: E402

FIXTURE = os.path.join(os.path.dirname(os.path.dirname(HERE)), "crates", "runtime", "tests", "data", "sample.nxpk")


def retrailer(body):
    return body + hashlib.sha256(body).digest()


class Archive(unittest.TestCase):
    def test_round_trip_sorts_and_keeps_content(self):
        blob = nxpk.build(nxpk.SAMPLE)
        got = nxpk.parse(blob)
        self.assertEqual([n for n, _, _ in got], ["alpha.bin", "hello", "zeta"])
        self.assertEqual({n: (m, d) for n, m, d in got}, {n: (m, d) for n, m, d in nxpk.SAMPLE})

    def test_one_encoding_whatever_the_input_order(self):
        a = nxpk.build(nxpk.SAMPLE)
        self.assertEqual(a, nxpk.build(list(reversed(nxpk.SAMPLE))))
        self.assertEqual(a, nxpk.build(nxpk.SAMPLE))

    def test_empty_archive(self):
        blob = nxpk.build([])
        self.assertEqual(len(blob), 48)
        self.assertEqual(nxpk.parse(blob), [])

    def test_blobs_are_aligned_and_padded_with_zeros(self):
        blob = nxpk.build([("a", 0o755, b"x"), ("b", 0o755, b"y" * 16)])
        for _, _, data in nxpk.parse(blob):
            at = blob.index(data)
            self.assertEqual(at % 16, 0)
        self.assertEqual(len(blob) % 16, 0)

    def test_every_single_byte_change_is_refused(self):
        blob = nxpk.build(nxpk.SAMPLE)
        for i in range(len(blob)):
            bad = bytearray(blob)
            bad[i] ^= 0x01
            with self.assertRaises(nxpk.PackError, msg="byte %d" % i):
                nxpk.parse(bytes(bad))

    def test_every_truncation_and_extension_is_refused(self):
        blob = nxpk.build(nxpk.SAMPLE)
        for n in range(len(blob)):
            with self.assertRaises(nxpk.PackError, msg="cut at %d" % n):
                nxpk.parse(blob[:n])
        for extra in (b"\0", b"\0" * 16, b"junk"):
            with self.assertRaises(nxpk.PackError):
                nxpk.parse(blob + extra)

    # Where the fields of each directory entry of SAMPLE are (names sorted: alpha.bin, hello, zeta).
    ENTRY = {"alpha.bin": 16, "hello": 77, "zeta": 134}

    def at(self, name, what):
        n = len(name)
        return self.ENTRY[name] + {"name": 1, "kind": 1 + n, "mode": 2 + n, "size": 4 + n, "offset": 12 + n}[what]

    def test_structural_faults_are_refused_for_the_right_reason_even_with_a_valid_trailer(self):
        blob = nxpk.build(nxpk.SAMPLE)
        body = blob[:-32]
        a = "alpha.bin"
        dir_end = 190

        def corrupt(at, new, base=body):
            return retrailer(base[:at] + new + base[at + len(new):])

        cases = [
            ("bad magic", corrupt(0, b"NXPX")),
            ("unknown version", corrupt(4, b"\x02\x00")),
            ("unknown flags", corrupt(6, b"\x01\x00")),
            ("too many entries", corrupt(8, b"\xff\xff\x00\x00")),
            ("too many entries", corrupt(8, b"\x01\x01\x00\x00")),
            ("directory length does not match", corrupt(8, b"\x02\x00\x00\x00")),
            ("directory too short", corrupt(8, b"\x04\x00\x00\x00")),
            ("directory beyond the archive", corrupt(12, b"\xff\xff\x00\x00")),
            ("directory beyond the archive", corrupt(12, (len(body) - 16 + 1).to_bytes(4, "little"))),
            # 16 + 16 = 32 is aligned, so no padding is read; the first entry's fixed part does not fit
            ("directory entry cut short", corrupt(12, (16).to_bytes(4, "little"))),
            ("directory padding not zero", corrupt(dir_end, b"\x01")),
            ("bad name", corrupt(self.at(a, "name"), b" ")),
            ("names not strictly increasing", corrupt(self.at(a, "name"), b"z")),
            ("unknown kind", corrupt(self.at(a, "kind"), b"\x02")),
            ("bad mode", corrupt(self.at(a, "mode"), b"\xff\xff")),
            ("bad mode", corrupt(self.at(a, "mode"), b"\x00\x10")),
            ("canonical layout", corrupt(self.at(a, "offset"), b"\x20")),
            ("data beyond the archive", corrupt(self.at(a, "size") + 2, b"\xff")),
            ("entry hash mismatch", corrupt(self.at(a, "size"), b"\x61")),
            ("entry hash mismatch", corrupt(self.at(a, "size"), b"\x5f")),
            ("entry hash mismatch", corrupt(192 + 3, b"\xee")),
            ("data padding not zero", corrupt(328, b"\x01")),
        ]
        for why, bad in cases:
            with self.assertRaisesRegex(nxpk.PackError, why, msg=why):
                nxpk.parse(bad)

    def test_bytes_after_the_last_entry_are_refused(self):
        body = nxpk.build(nxpk.SAMPLE)[:-32]
        with self.assertRaisesRegex(nxpk.PackError, "bytes after the last entry"):
            nxpk.parse(retrailer(body + b"\0" * 16))

    def test_duplicate_names_are_refused_on_reading(self):
        body = nxpk.build([("aa", 0o755, b"1"), ("ab", 0o755, b"2")])[:-32]
        at = body.index(b"ab")
        with self.assertRaisesRegex(nxpk.PackError, "names not strictly increasing"):
            nxpk.parse(retrailer(body[:at] + b"aa" + body[at + 2:]))

    def test_the_largest_archive_is_accepted(self):
        entries = [("n%03d" % i, 0o755, b"") for i in range(256)]
        self.assertEqual(len(nxpk.parse(nxpk.build(entries))), 256)

    def test_short_inputs_are_called_short(self):
        for n in range(48):
            with self.assertRaisesRegex(nxpk.PackError, "too short"):
                nxpk.parse(b"N" * n)

    def test_padding_must_be_zero(self):
        blob = nxpk.build([("a", 0o755, b"x")])
        body = bytearray(blob[:-32])
        body[-1] = 1
        with self.assertRaises(nxpk.PackError):
            nxpk.parse(retrailer(bytes(body)))

    def test_build_refuses_bad_input(self):
        for entries in (
            [("a", 0o755, b""), ("a", 0o755, b"")],
            [("", 0o755, b"")],
            [("has space", 0o755, b"")],
            [("x" * 65, 0o755, b"")],
            [("a/b", 0o755, b"")],
            [("a", 0o10000, b"")],
            [("n%d" % i, 0o755, b"") for i in range(257)],
        ):
            with self.assertRaises(nxpk.PackError):
                nxpk.build(entries)

    def test_the_committed_fixture_is_what_build_gives(self):
        with open(FIXTURE, "rb") as f:
            self.assertEqual(f.read(), nxpk.build(nxpk.SAMPLE), "regenerate with: nxpk.py sample FIXTURE")


if __name__ == "__main__":
    unittest.main()
