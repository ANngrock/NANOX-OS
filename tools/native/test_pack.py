#!/usr/bin/env python3
"""Tests of the cross-build packer without a compiler: the build step is
replaced, the rest (judging, contract check, archive, manifest, verify) is
the real code. Run: python3 -m unittest tools/native/test_pack.py"""
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import nxpk  # noqa: E402
import pack  # noqa: E402
from test_check_elf import good  # noqa: E402


class Names(unittest.TestCase):
    def test_programs_are_the_packages_under_user(self):
        with tempfile.TemporaryDirectory() as root:
            for d, name in (("b", "nanox-b"), ("a", "nanox-a")):
                os.makedirs(os.path.join(root, "user", d))
                with open(os.path.join(root, "user", d, "Cargo.toml"), "w") as f:
                    f.write('[package]\nname = "%s"\nversion = "0.1.0"\n' % name)
            os.makedirs(os.path.join(root, "user", "empty"))
            self.assertEqual(pack.program_names(root), ["nanox-a", "nanox-b"])

    def test_only_the_package_name_counts(self):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, "user", "a"))
            with open(os.path.join(root, "user", "a", "Cargo.toml"), "w") as f:
                lines = ['[package]', 'rename = "wrong"', '  name = "right"', '[dependencies]', 'x = { name = "other" }']
                f.write(chr(10).join(lines) + chr(10))
            self.assertEqual(pack.program_names(root), ["right"])

    def test_the_real_tree_has_the_sample_program(self):
        self.assertIn("nanox-hello", pack.program_names())


class Flags(unittest.TestCase):
    def test_user_flags_and_every_remap_are_present(self):
        f = pack.rustflags("/src/repo", "/tmp/t/a", "/home/u/.cargo")
        for flag in ("link-arg=-Tuser/link.ld", "relocation-model=static", "--remap-path-prefix=/src/repo=/nanox",
                     "--remap-path-prefix=/tmp/t/a=/target", "--remap-path-prefix=/home/u/.cargo=/cargo"):
            self.assertIn(flag, f)

    def test_the_two_builds_get_different_target_dirs_but_one_text(self):
        a = pack.rustflags("/r", "/t/a", "/c").replace("/t/a", "X")
        b = pack.rustflags("/r", "/t/b", "/c").replace("/t/b", "X")
        self.assertEqual(a, b)


class BuildStep(unittest.TestCase):
    def test_cargo_is_run_with_the_pinned_environment(self):
        seen = {}

        def run(cmd, cwd, env, check):
            seen.update(cmd=cmd, cwd=cwd, env=env, check=check)
            out = os.path.join(td, "t", "x86_64-unknown-none", "release")
            os.makedirs(out)
            with open(os.path.join(out, "nanox-x"), "wb") as f:
                f.write(b"image")

        with tempfile.TemporaryDirectory() as td:
            data = pack.build_once("nanox-x", "/the/root", os.path.join(td, "t"), "/home/u/.cargo", run=run)
        self.assertEqual(data, b"image")
        self.assertEqual(seen["cwd"], "/the/root")
        self.assertTrue(seen["check"], "a failing build stops the packer")
        cmd = seen["cmd"]
        for part in ("--release", "--locked", "--offline", "-p", "nanox-x", "--target", "x86_64-unknown-none"):
            self.assertIn(part, cmd)
        self.assertEqual(cmd[:2], ["cargo", "build"])
        self.assertEqual(seen["env"]["SOURCE_DATE_EPOCH"], "1790035200")
        self.assertEqual(seen["env"]["CARGO_TARGET_DIR"], os.path.join(td, "t"))
        self.assertIn("--remap-path-prefix=/the/root=/nanox", seen["env"]["RUSTFLAGS"])


class Judging(unittest.TestCase):
    def test_identical(self):
        self.assertEqual(pack.judge(b"abc", b"abc"), (True, None))

    def test_different_content_names_the_byte(self):
        same, why = pack.judge(b"abcd", b"abXd")
        self.assertFalse(same)
        self.assertIn("byte 2", why)
        self.assertIn("63 against 58", why)

    def test_different_length(self):
        same, why = pack.judge(b"abc", b"abcd")
        self.assertFalse(same)
        self.assertIn("3 and 4", why)


def fake_builder(by_dir_suffix):
    """A build_once that returns one image for the first target dir and another for the second."""
    calls = []

    def build_once(package, root, target_dir, cargo_home, run=None):
        calls.append((package, os.path.basename(target_dir), root))
        return by_dir_suffix[os.path.basename(target_dir)](package)

    return build_once, calls


class Packing(unittest.TestCase):
    def setUp(self):
        self.real = pack.build_once
        self.addCleanup(lambda: setattr(pack, "build_once", self.real))
        self.out = tempfile.TemporaryDirectory()
        self.addCleanup(self.out.cleanup)
        self.log = []

    def run_pack(self, builder, names=("nanox-x",)):
        pack.build_once = builder
        return pack.pack(list(names), self.out.name, root="/the/checkout", log=self.log.append,
                         rustc="rustc 1.90.0 (test)", copy=lambda root, dest: "/a/copy/elsewhere")

    def test_a_reproducible_conforming_program_is_archived(self):
        elf = good()
        builder, calls = fake_builder({"a": lambda p: elf, "b": lambda p: elf})
        code, m = self.run_pack(builder)
        self.assertEqual(code, 0, self.log)
        self.assertEqual(calls, [("nanox-x", "a", "/the/checkout"), ("nanox-x", "b", "/a/copy/elsewhere")],
                         "built twice: here and in a copy at another path, in different target directories")
        with open(os.path.join(self.out.name, pack.ARCHIVE), "rb") as f:
            blob = f.read()
        self.assertEqual(nxpk.parse(blob), [("nanox-x", 0o755, elf)])
        prog = m["programs"][0]
        self.assertEqual((prog["name"], prog["size"], prog["reproducible"], prog["contract"]),
                         ("nanox-x", len(elf), True, "satisfied"))
        self.assertEqual(prog["sha256"], pack.sha(elf))
        self.assertEqual(m["rustc"], "rustc 1.90.0 (test)")
        self.assertEqual(m["builds_per_program"], 2)
        self.assertEqual(m["target"], "x86_64-unknown-none")
        self.assertEqual(m["archive"]["digest"], blob[-32:].hex())
        self.assertEqual(m["archive"]["file_sha256"], pack.sha(blob))
        with open(os.path.join(self.out.name, "manifest.json")) as f:
            self.assertEqual(json.load(f), m)

    def test_a_build_that_is_not_reproducible_is_refused(self):
        a, b = good(), bytearray(good())
        b[100] ^= 1
        builder, _ = fake_builder({"a": lambda p: a, "b": lambda p: bytes(b)})
        code, m = self.run_pack(builder)
        self.assertEqual(code, 1)
        self.assertFalse(os.path.exists(os.path.join(self.out.name, pack.ARCHIVE)), "nothing is shipped")
        self.assertFalse(m["programs"][0]["reproducible"])
        self.assertIn("byte 100", m["programs"][0]["difference"])
        self.assertTrue(any("NOT REPRODUCIBLE" in line for line in self.log))

    def test_a_program_that_breaks_the_contract_is_refused(self):
        elf = good(e_type=3)
        builder, _ = fake_builder({"a": lambda p: elf, "b": lambda p: elf})
        code, m = self.run_pack(builder)
        self.assertEqual(code, 1)
        self.assertEqual(m["programs"][0]["contract"], "violated")
        self.assertTrue(m["programs"][0]["violations"])
        self.assertFalse(os.path.exists(os.path.join(self.out.name, pack.ARCHIVE)))

    def test_one_bad_program_stops_the_whole_set(self):
        good_elf, bad_elf = good(), good(e_type=3)
        builder, _ = fake_builder({"a": lambda p: good_elf if p == "p1" else bad_elf,
                                   "b": lambda p: good_elf if p == "p1" else bad_elf})
        code, m = self.run_pack(builder, ("p1", "p2"))
        self.assertEqual(code, 1)
        self.assertEqual([p["contract"] for p in m["programs"]], ["satisfied", "violated"])
        self.assertFalse(os.path.exists(os.path.join(self.out.name, pack.ARCHIVE)))

    def test_several_programs_share_one_archive_in_name_order(self):
        e1, e2 = good(), good(size=0x6000)
        builder, _ = fake_builder({"a": lambda p: {"zz": e1, "aa": e2}[p], "b": lambda p: {"zz": e1, "aa": e2}[p]})
        code, _ = self.run_pack(builder, ("zz", "aa"))
        self.assertEqual(code, 0)
        with open(os.path.join(self.out.name, pack.ARCHIVE), "rb") as f:
            self.assertEqual([n for n, _, _ in nxpk.parse(f.read())], ["aa", "zz"])

    def test_the_same_inputs_give_the_same_archive_bytes(self):
        elf = good()
        builder, _ = fake_builder({"a": lambda p: elf, "b": lambda p: elf})
        self.run_pack(builder)
        with open(os.path.join(self.out.name, pack.ARCHIVE), "rb") as f:
            first = f.read()
        self.run_pack(builder)
        with open(os.path.join(self.out.name, pack.ARCHIVE), "rb") as f:
            self.assertEqual(f.read(), first)


class Verifying(unittest.TestCase):
    def archive(self, entries):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        path = os.path.join(d.name, "x.nxpk")
        with open(path, "wb") as f:
            f.write(nxpk.build(entries))
        return path

    def test_a_good_archive(self):
        log = []
        self.assertEqual(pack.verify(self.archive([("p", 0o755, good())]), log.append), 0)
        self.assertTrue(any("archive ok: 1 programs" in line for line in log))

    def test_a_damaged_archive(self):
        path = self.archive([("p", 0o755, good())])
        with open(path, "rb") as f:
            blob = bytearray(f.read())
        blob[200] ^= 1
        with open(path, "wb") as f:
            f.write(blob)
        log = []
        self.assertEqual(pack.verify(path, log.append), 1)
        self.assertTrue(log[0].startswith("refused"))

    def test_a_program_that_is_not_to_contract(self):
        log = []
        self.assertEqual(pack.verify(self.archive([("p", 0o755, b"not an elf")]), log.append), 1)
        self.assertTrue(any("VIOLATION" in line for line in log))


class Snapshot(unittest.TestCase):
    def test_a_copy_leaves_out_build_output_and_history(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = os.path.join(d, "src"), os.path.join(d, "copy")
            for rel in ("a/x.rs", "target/t.o", "out/log", ".git/HEAD", "p/__pycache__/m.pyc"):
                os.makedirs(os.path.dirname(os.path.join(src, rel)), exist_ok=True)
                with open(os.path.join(src, rel), "w") as f:
                    f.write("x")
            self.assertEqual(pack.snapshot(src, dst), dst)
            found = sorted(os.path.relpath(os.path.join(r, f), dst).replace(os.sep, "/")
                           for r, _, fs in os.walk(dst) for f in fs)
            self.assertEqual(found, ["a/x.rs"])


if __name__ == "__main__":
    unittest.main()
