#!/usr/bin/env python3
"""Route C: the cross-build of native NANOX programs into one archive.

    nix develop --command python3 tools/native/pack.py [--package NAME ...] [--out DIR]
    python3 tools/native/pack.py --verify DIR/nanox-native.nxpk

For every program under user/ (or the packages named) it

  1. builds it twice, once in this checkout and once in a copy of it at another
     path, each with its own target directory, with the user flags
     (tools/native/build.py), the build paths remapped and a fixed
     SOURCE_DATE_EPOCH, so that no host path ends up in the output;
  2. requires the two builds to be byte-identical (a build that is not
     reproducible is reported and refused, never shipped);
  3. checks the ELF contract (tools/native/check_elf.py) on the result;

then writes one NXPK archive (tools/native/nxpk.py) with the programs and a
manifest.json that records what was built from what: source commit, whether
the tree was dirty, rustc, the flags, and each program's size, digest and
verdicts. Nothing in the archive depends on when or where it was built.

What this is not: it cross-builds freestanding (no_std) programs against the
native runtime. A Rust `std` or a C library for NANOX does not exist yet
(docs/specs/M10-NATIVE.md, tiers T1 and T2), so `rustc`, `cargo` and ordinary
crates are not cross-built by it.
"""
import argparse
import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
import build as native_build  # noqa: E402
import check_elf  # noqa: E402
import nxpk  # noqa: E402

SOURCE_DATE_EPOCH = "1790035200"  # the same as flake.nix and xtask
ARCHIVE = "nanox-native.nxpk"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def program_names(root=ROOT):
    """The packages under user/: the `name` of every user/*/Cargo.toml."""
    names = []
    for toml in sorted(glob.glob(os.path.join(root, "user", "*", "Cargo.toml"))):
        with open(toml, encoding="utf-8") as f:
            m = re.search(r'^\s*name\s*=\s*"([^"]+)"', f.read(), re.M)
        if m:
            names.append(m.group(1))
    return names


def rustflags(root, target_dir, cargo_home):
    """The user flags plus the remapping that keeps build locations out of the output."""
    flags = list(native_build.USER_FLAGS)
    for src, dst in ((root, "/nanox"), (target_dir, "/target"), (cargo_home, "/cargo")):
        flags += ["--remap-path-prefix=%s=%s" % (src, dst)]
    return " ".join(flags)


def build_once(package, root, target_dir, cargo_home, run=subprocess.run):
    env = dict(os.environ,
               RUSTFLAGS=rustflags(root, target_dir, cargo_home),
               CARGO_TARGET_DIR=target_dir,
               SOURCE_DATE_EPOCH=SOURCE_DATE_EPOCH)
    cmd = ["cargo", "build", "--release", "--locked", "--offline", "-p", package,
           "--target", native_build.TARGET]
    run(cmd, cwd=root, env=env, check=True)
    path = os.path.join(target_dir, native_build.TARGET, "release", package)
    with open(path, "rb") as f:
        return f.read()


def snapshot(root, dest):
    """A copy of the source tree at another path (without build output and history)."""
    shutil.copytree(root, dest, ignore=shutil.ignore_patterns("target", "out", ".git", "__pycache__", "*.pyc"))
    return dest


def first_difference(a, b):
    if len(a) != len(b):
        return "sizes differ: %d and %d bytes" % (len(a), len(b))
    for i, (x, y) in enumerate(zip(a, b)):
        if x != y:
            return "first difference at byte %d (%02x against %02x)" % (i, x, y)
    return None


def judge(first, second):
    """The verdicts on one program built twice: (reproducible, why not)."""
    why = first_difference(first, second)
    return why is None, why


def git(root, *args):
    try:
        return subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def source_stamp(root):
    status = git(root, "status", "--porcelain")
    return {"commit": git(root, "rev-parse", "HEAD"), "dirty": bool(status) if status is not None else None}


def manifest(stamp, rustc, programs, archive_bytes):
    return {
        "schema_version": 1,
        "source": stamp,
        "rustc": rustc,
        "target": native_build.TARGET,
        "flags": native_build.USER_FLAGS,
        "builds_per_program": 2,
        "programs": programs,
        "archive": {
            "name": ARCHIVE,
            "size": len(archive_bytes),
            # The trailer: the hash of every byte before it, which is what the reader checks.
            "digest": archive_bytes[-32:].hex(),
            "file_sha256": sha(archive_bytes),
        },
    }


def rustc_version():
    return subprocess.run(["rustc", "--version"], capture_output=True, text=True, check=True).stdout.strip()


def pack(packages, out, root=ROOT, log=print, rustc=None, copy=snapshot):
    """Builds, judges and archives. Returns (exit code, manifest of the attempt)."""
    cargo_home = os.environ.get("CARGO_HOME", os.path.expanduser("~/.cargo"))
    scratch = tempfile.mkdtemp(prefix="nxpk-")
    entries, programs, failed = [], [], False
    try:
        other = copy(root, os.path.join(scratch, "src"))
        for name in packages:
            where = [(root, os.path.join(scratch, "a")), (other, os.path.join(scratch, "b"))]
            log("building %s twice (here and in a copy at %s)" % (name, other))
            first, second = (build_once(name, r, d, cargo_home) for r, d in where)
            same, why = judge(first, second)
            violations = check_elf.check(first)
            record = {"name": name, "size": len(first), "sha256": sha(first),
                      "reproducible": same, "contract": "satisfied" if not violations else "violated"}
            if not same:
                record["difference"] = why
            if violations:
                record["violations"] = violations
            programs.append(record)
            log("  %s: %d bytes, reproducible=%s, contract=%s" % (name, len(first), same, record["contract"]))
            for v in violations:
                log("  VIOLATION: %s" % v)
            if not same:
                log("  NOT REPRODUCIBLE: %s" % why)
            if not same or violations:
                failed = True
            else:
                entries.append((name, 0o755, first))
            for _, d in where:
                shutil.rmtree(d, ignore_errors=True)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    if failed:
        return 1, {"programs": programs}
    blob = nxpk.build(entries)
    m = manifest(source_stamp(root), rustc or rustc_version(), programs, blob)
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, ARCHIVE), "wb") as f:
        f.write(blob)
    with open(os.path.join(out, "manifest.json"), "w", encoding="utf-8", newline="\n") as f:
        json.dump(m, f, indent=2, sort_keys=True)
        f.write("\n")
    log("wrote %s (%d bytes, digest %s)" % (os.path.join(out, ARCHIVE), len(blob), m["archive"]["digest"]))
    return 0, m


def verify(path, log=print):
    with open(path, "rb") as f:
        blob = f.read()
    try:
        items = nxpk.parse(blob)
    except nxpk.PackError as e:
        log("refused: %s" % e)
        return 1
    for name, mode, data in items:
        log("%o %8d %s %s" % (mode, len(data), sha(data), name))
        bad = check_elf.check(data)
        for b in bad:
            log("  VIOLATION: %s" % b)
        if bad:
            return 1
    log("archive ok: %d programs, digest %s" % (len(items), blob[-32:].hex()))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--package", action="append", help="a user program (default: all under user/)")
    ap.add_argument("--out", default=os.path.join(ROOT, "out", "dist"))
    ap.add_argument("--verify", metavar="ARCHIVE", help="check an archive instead of building one")
    args = ap.parse_args(argv)
    if args.verify:
        return verify(args.verify)
    packages = args.package or program_names()
    if not packages:
        print("no programs under user/")
        return 2
    code, _ = pack(packages, args.out)
    return code


if __name__ == "__main__":
    sys.exit(main())
