#!/usr/bin/env python3
"""Measures what a host kernel has to provide to run the Rust toolchain: the
libc functions the toolchain binaries import, and the system calls one real
build issues (compiling and linking test executables). The numbers feed
docs/specs/M10-NATIVE.md, section 6; they are a measurement of this
toolchain (Nix, glibc, rustc 1.90), not a minimum.

    nix develop --command python3 tools/native/surface.py [--out DIR]

Needs strace and nm. Writes libc-names.txt, toolchain-imports.tsv and
build-syscalls.tsv into DIR (default docs/research/native-surface).
"""
import glob
import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GROUPS = {
    "memory": "mmap munmap mprotect madvise brk mremap",
    "files": ("openat open read write pread64 pwrite64 lseek close fstat newfstatat statx stat "
              "lstat readlink getdents64 access faccessat2 ftruncate fsync flock fcntl dup2 "
              "pipe2 rename unlink unlinkat linkat mkdir chdir getcwd utimensat statfs ioctl"),
    "processes and threads": ("clone clone3 execve wait4 getpid getppid getpgrp tgkill prctl "
                              "arch_prctl set_tid_address rseq set_robust_list prlimit64 "
                              "sched_getaffinity sched_yield uname getuid geteuid getgid getegid"),
    "synchronization": "futex poll restart_syscall",
    "signals": "rt_sigaction rt_sigprocmask rt_sigreturn sigaltstack",
    "other": "getrandom recvfrom socketpair",
}


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def imports(path):
    out = run(["nm", "-D", "--undefined-only", path]).stdout
    return {l.split()[1] for l in out.splitlines() if len(l.split()) >= 2 and l.split()[0] == "U"}


def real_elf(path):
    """Nix wraps the compiler driver in a shell script; follow it to the binary."""
    if not path or not os.path.exists(path):
        return None
    head = open(path, "rb").read(4)
    if head == b"ELF":
        return path
    import re
    text = open(path, "rb").read().decode("latin-1")
    m = re.search(r"/nix/store/[A-Za-z0-9._+-]+/bin/gcc", text)
    return m.group(0) if m and os.path.exists(m.group(0)) else None


def toolchain_binaries():
    sysroot = run(["rustc", "--print", "sysroot"]).stdout.strip()
    pick = lambda pattern: (sorted(glob.glob(pattern)) or [None])[0]
    lld = pick("/nix/store/*-rustc-*/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld") or \
        pick(sysroot + "/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld")
    collect2 = run(["gcc", "-print-prog-name=collect2"]).stdout.strip()
    return {
        "cargo": shutil.which("cargo"),
        "librustc_driver": pick(sysroot + "/lib/librustc_driver-*.so"),
        "libLLVM": pick(sysroot + "/lib/libLLVM.so.*"),
        "rust-lld": lld,
        "gcc driver": real_elf(shutil.which("gcc")),
        "collect2": collect2 if os.path.exists(collect2) else None,
    }


def libc_surface(out):
    per, union = [], set()
    for name, path in toolchain_binaries().items():
        if not path or not os.path.exists(path):
            per.append((name, "missing", 0))
            continue
        syms = imports(path)
        union |= syms
        per.append((name, os.path.basename(os.path.realpath(path)), len(syms)))
    names = sorted({s.split("@")[0] for s in union if "@GLIBC_" in s})
    with open(os.path.join(out, "libc-names.txt"), "w") as f:
        f.write("\n".join(names) + "\n")
    with open(os.path.join(out, "toolchain-imports.tsv"), "w") as f:
        f.write("# binary<TAB>file<TAB>imported dynamic symbols (nm -D --undefined-only)\n")
        for name, file, n in per:
            f.write("%s\t%s\t%d\n" % (name, file, n))
    print("libc: %d distinct glibc function names imported by the toolchain binaries" % len(names))
    for name, file, n in per:
        print("  %-16s %5d symbols" % (name, n))


def syscalls(out):
    tmp = tempfile.mkdtemp(prefix="nanox-surface-")
    summary = os.path.join(tmp, "strace.sum")
    env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(tmp, "target"))
    cmd = ["strace", "-f", "-c", "-o", summary, "cargo", "test", "--offline", "--no-run",
           "-p", "dep-sched"]
    print("+", " ".join(cmd))
    r = run(cmd, cwd=ROOT, env=env)
    if r.returncode != 0:
        print(r.stderr[-2000:])
        sys.exit(1)
    rows = []
    for l in open(summary).read().splitlines()[2:]:
        f = l.split()
        if f and not f[0].startswith("-") and f[-1] != "total":
            rows.append((f[-1], int(f[3])))
    rows.sort(key=lambda x: (-x[1], x[0]))
    where = {n: g for g, ns in GROUPS.items() for n in ns.split()}
    with open(os.path.join(out, "build-syscalls.tsv"), "w") as f:
        f.write("# syscall<TAB>calls<TAB>group: cargo test --no-run -p dep-sched under strace -f\n")
        for n, c in rows:
            f.write("%s\t%d\t%s\n" % (n, c, where.get(n, "unclassified")))
    shutil.rmtree(tmp, ignore_errors=True)
    print("syscalls: %d distinct, %d calls" % (len(rows), sum(c for _, c in rows)))
    groups = {}
    for n, c in rows:
        g = groups.setdefault(where.get(n, "unclassified"), [0, []])
        g[0] += c
        g[1].append(n)
    for name, (c, ns) in sorted(groups.items(), key=lambda kv: -kv[1][0]):
        print("  %-22s %6d calls, %2d syscalls: %s" % (name, c, len(ns), " ".join(sorted(ns))))


def main():
    out = os.path.join(ROOT, "docs", "research", "native-surface")
    if "--out" in sys.argv:
        out = sys.argv[sys.argv.index("--out") + 1]
    os.makedirs(out, exist_ok=True)
    print(run(["rustc", "--version"]).stdout.strip())
    print(run(["gcc", "--version"]).stdout.splitlines()[0])
    libc_surface(out)
    syscalls(out)


if __name__ == "__main__":
    main()
