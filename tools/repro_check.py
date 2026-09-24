#!/usr/bin/env python3
"""Reproducibility check (make repro-check).

Copies the current source tree (tracked + untracked, not ignored files) into
two fresh directories with different absolute paths, runs `make all` in each
and in the working tree, and requires BOOTX64.EFI, kernel.elf, initrd.img and nanox.img to
be byte-identical across all three builds (plus initrd.img).  Writes out/repro-check.json.
Temporary trees are removed on success and kept for inspection on failure.
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
ARTIFACTS = ("BOOTX64.EFI", "kernel.elf", "initrd.img", "nanox.img")


def source_files():
    try:
        out = subprocess.run(["git", "-C", str(REPO), "ls-files", "-z", "--cached", "--others",
                              "--exclude-standard"], stdout=subprocess.PIPE, check=True).stdout
        names = [n for n in out.decode().split("\0") if n]
    except (OSError, subprocess.CalledProcessError):
        names = [str(p.relative_to(REPO)) for p in REPO.rglob("*")
                 if p.is_file() and not {".git", "out"} & set(p.relative_to(REPO).parts)]
    return [n for n in names if (REPO / n).is_file()]  # skip deleted-but-tracked


def copy_tree(dest):
    for name in source_files():
        target = dest / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(REPO / name, target)


def build(tree):
    env = dict(os.environ)
    env.pop("MAKEFLAGS", None)
    env.pop("MAKELEVEL", None)
    subprocess.run(["make", "-C", str(tree), "all"], env=env, check=True,
                   stdout=subprocess.DEVNULL)
    return {a: hashlib.sha256((tree / "out" / a).read_bytes()).hexdigest() for a in ARTIFACTS}


def main():
    tmp = Path(tempfile.mkdtemp(prefix="nanox-repro-"))
    trees = {"clean-a": tmp / "a" / "NANOX-OS",
             "clean-b": tmp / "b" / "some" / "deeper" / "path" / "src"}
    results = {}
    try:
        for label, tree in trees.items():
            copy_tree(tree)
            results[label] = build(tree)
        results["worktree"] = build(REPO)
    except subprocess.CalledProcessError as e:
        print("repro-check: build failed: %s (trees kept in %s)" % (e, tmp))
        return 1

    identical = {a: len({r[a] for r in results.values()}) == 1 for a in ARTIFACTS}
    # Byte-by-byte comparison of the two clean builds as well as hashes.
    for a in ARTIFACTS:
        blobs = [(t / "out" / a).read_bytes() for t in trees.values()]
        identical[a] = identical[a] and blobs[0] == blobs[1]
    ok = all(identical.values())
    for a in ARTIFACTS:
        print("%-4s %-12s %s" % ("ok" if identical[a] else "DIFF", a,
                                 "  ".join("%s=%s" % (k, v[a][:16]) for k, v in results.items())))
    report = {"schema": "nanox.repro-check.v1", "builds": {k: str(v) for k, v in trees.items()},
              "sha256": results, "identical": identical, "ok": ok,
              "source_date_epoch": os.environ.get("SOURCE_DATE_EPOCH")}
    (REPO / "out").mkdir(exist_ok=True)
    (REPO / "out" / "repro-check.json").write_text(json.dumps(report, indent=2) + "\n")
    if ok:
        shutil.rmtree(tmp)
        print("repro-check: OK, %d artifacts identical across 3 builds in different paths"
              % len(ARTIFACTS))
        return 0
    print("repro-check: FAILED, trees kept in %s" % tmp)
    return 1


if __name__ == "__main__":
    sys.exit(main())
