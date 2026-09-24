#!/usr/bin/env python3
"""NANOX environment check: compares the tools actually on PATH with toolchain.lock.

  doctor.py                 check the selected profile, write out/doctor.json
  doctor.py --print-lock    print a lock section describing this machine

Profile selection: --profile, else $NANOX_TOOLCHAIN_PROFILE, else "nix" when
$IN_NIX_SHELL is set, else "ubuntu-24.04".

Lock keys (see toolchain.lock):
  <tool> = X.Y.Z        exact version required
  <tool>.min = X.Y      minimum version (tools whose version cannot change outputs)
  <file>.sha256 = HEX   exact firmware file hash
Exit status: 0 when every required entry matches, 1 otherwise.
"""

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "tools" / "bench"))
import qemu  # noqa: E402  (canonical OVMF paths)

LOCK_PATH = REPO / "toolchain.lock"

# name -> (argv, regex capturing the version)
TOOLS = {
    "clang": ([os.environ.get("CLANG", "clang"), "--version"], r"clang version (\d+\.\d+\.\d+)"),
    "ld.lld": ([os.environ.get("LD_LLD", "ld.lld"), "--version"], r"LLD (\d+\.\d+\.\d+)"),
    "lld-link": ([os.environ.get("LLD_LINK", "lld-link"), "--version"], r"LLD (\d+\.\d+\.\d+)"),
    "qemu-system-x86_64": ([qemu.QEMU_BINARY, "--version"], r"version (\d+\.\d+\.\d+)"),
    "mtools": (["mtools", "--version"], r"(\d+\.\d+\.\d+)"),
    "make": (["make", "--version"], r"GNU Make (\d+\.\d+(?:\.\d+)?)"),
    "gdb": (["gdb", "--version"], r"GNU gdb .*?(\d+\.\d+(?:\.\d+)?)"),
}


def run_version(argv, regex):
    exe = shutil.which(argv[0])
    if not exe:
        return None, None
    try:
        out = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             timeout=30, check=False).stdout.decode("utf-8", "replace")
    except (OSError, subprocess.TimeoutExpired):
        return exe, None
    m = re.search(regex, out)
    return exe, (m.group(1) if m else None)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def collect():
    """Detects tool versions and firmware hashes on this machine."""
    tools = {}
    for name, (argv, regex) in TOOLS.items():
        exe, ver = run_version(argv, regex)
        tools[name] = {"path": exe, "version": ver}
    tools["python3"] = {"path": sys.executable, "version": platform.python_version()}
    firmware = {}
    for key, path in (("ovmf_code", qemu.ovmf_code_path()),
                      ("ovmf_vars", qemu.ovmf_vars_template_path())):
        entry = {"path": path, "sha256": None}
        if os.path.isfile(path):
            entry["sha256"] = sha256_file(path)
        firmware[key] = entry
    return {"tools": tools, "firmware": firmware, "host": {
        "system": platform.system(), "release": platform.release(),
        "machine": platform.machine(), "kvm": os.path.exists("/dev/kvm")}}


def parse_lock(path=LOCK_PATH):
    profiles, current = {}, None
    for lineno, raw in enumerate(Path(path).read_text().splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        m = re.fullmatch(r"\[profile\s+([A-Za-z0-9_.-]+)\]", line)
        if m:
            current = profiles.setdefault(m.group(1), {})
            continue
        if current is None or "=" not in line:
            raise ValueError("%s:%d: unexpected line %r" % (path, lineno, raw))
        key, value = (s.strip() for s in line.split("=", 1))
        current[key] = value
    return profiles


def select_profile(explicit=None):
    if explicit:
        return explicit
    if os.environ.get("NANOX_TOOLCHAIN_PROFILE"):
        return os.environ["NANOX_TOOLCHAIN_PROFILE"]
    if os.environ.get("IN_NIX_SHELL"):
        return "nix"
    return "ubuntu-24.04"


def version_tuple(v):
    return tuple(int(x) for x in v.split("."))


def check(detected, lock):
    """Returns a list of (key, expected, actual, ok) rows."""
    rows = []
    for key, expected in lock.items():
        if key in ("status", "note"):
            continue
        if key.endswith(".sha256"):
            fw = detected["firmware"].get(key[:-len(".sha256")])
            actual = fw["sha256"] if fw else None
            rows.append((key, expected, actual, actual == expected))
        elif key.endswith(".min"):
            tool = detected["tools"].get(key[:-len(".min")], {})
            actual = tool.get("version")
            ok = actual is not None and version_tuple(actual) >= version_tuple(expected)
            rows.append((key, ">= " + expected, actual, ok))
        else:
            tool = detected["tools"].get(key, {})
            actual = tool.get("version")
            rows.append((key, expected, actual, actual == expected))
    return rows


def evaluate(profile_name=None):
    """Returns (report dict, ok) without printing; used by the harness."""
    detected = collect()
    profiles = parse_lock()
    name = select_profile(profile_name)
    report = {"schema": "nanox.doctor.v1", "profile": name, "detected": detected}
    lock = profiles.get(name)
    if lock is None:
        report["error"] = "profile %r not in toolchain.lock" % name
        return report, False
    rows = check(detected, lock)
    report["status"] = lock.get("status", "unverified")
    report["checks"] = [{"key": k, "expected": e, "actual": a, "ok": ok} for k, e, a, ok in rows]
    ok = all(r[3] for r in rows) and report["status"] == "verified"
    if report["status"] != "verified":
        report["error"] = ("profile %r is not verified: record its values with "
                           "`tools/doctor.py --print-lock` after a successful `make test`" % name)
    report["ok"] = ok
    return report, ok


def print_lock(detected):
    t, fw = detected["tools"], detected["firmware"]
    print("[profile NAME]")
    print("status = verified")
    for name in ("clang", "ld.lld", "lld-link", "qemu-system-x86_64", "mtools"):
        print("%s = %s" % (name, t[name]["version"]))
    print("ovmf_code.sha256 = %s" % fw["ovmf_code"]["sha256"])
    print("ovmf_vars.sha256 = %s" % fw["ovmf_vars"]["sha256"])
    print("python3.min = 3.8")
    print("make.min = 4.0")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--profile")
    ap.add_argument("--print-lock", action="store_true")
    ap.add_argument("--json-out", default=str(REPO / "out" / "doctor.json"))
    args = ap.parse_args(argv)

    if args.print_lock:
        print_lock(collect())
        return 0

    report, ok = evaluate(args.profile)
    print("NANOX doctor: profile %s (%s)" % (report["profile"], report.get("status", "?")))
    for row in report.get("checks", []):
        print("  %-4s %-20s expected %-66s actual %s" % (
            "ok" if row["ok"] else "FAIL", row["key"], row["expected"], row["actual"]))
    gdb = report["detected"]["tools"]["gdb"]
    print("  info gdb (optional, for make debug): %s" % (gdb["version"] or "not found"))
    print("  info kvm: %s (the bench uses TCG either way)" %
          ("present" if report["detected"]["host"]["kvm"] else "absent"))
    if "error" in report:
        print("NANOX doctor: " + report["error"])
    Path(args.json_out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.json_out).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print("NANOX doctor: %s (report: %s)" % ("OK" if ok else "FAILED", args.json_out))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
