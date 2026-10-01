#!/usr/bin/env python3
"""Builds the sample native program with the user flags and checks the result
against the ELF contract (docs/specs/M10-NATIVE.md, section 3).

    nix develop --command python3 tools/native/build.py [--package nanox-hello]

The workspace config carries the kernel flags for x86_64-unknown-none; the
RUSTFLAGS environment variable replaces them for this build, so a user
program gets its own linker script and static relocation. The code model
stays the prebuilt libraries (kernel): its 32-bit sign-extended absolute
addresses cover the low 2 GiB where user programs live; the small code model
comes with the custom target and its own standard library build.
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
import check_elf  # noqa: E402

TARGET = "x86_64-unknown-none"
USER_FLAGS = [
    "-C", "link-arg=-Tuser/link.ld",
    "-C", "link-arg=--build-id=none",
    "-C", "link-arg=-zmax-page-size=4096",
    "-C", "relocation-model=static",
]


def build(package):
    env = dict(os.environ, RUSTFLAGS=" ".join(USER_FLAGS))
    cmd = ["cargo", "build", "--release", "--locked", "--offline", "-p", package,
           "--target", TARGET]
    print("+", " ".join(cmd))
    subprocess.run(cmd, cwd=ROOT, env=env, check=True)
    return os.path.join(ROOT, "target", TARGET, "release", package)


def main():
    package = "nanox-hello"
    if "--package" in sys.argv:
        package = sys.argv[sys.argv.index("--package") + 1]
    elf = build(package)
    data = open(elf, "rb").read()
    print("%s: %d bytes" % (elf, len(data)))
    subprocess.run(["readelf", "-lW", elf], check=False)
    bad = check_elf.check(data)
    for b in bad:
        print("VIOLATION:", b)
    print("contract:", "violated" if bad else "satisfied")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
