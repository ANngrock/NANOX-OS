#!/usr/bin/env python3
"""Runs svm-probe under QEMU TCG with OVMF (docs/specs/M10-VMM.md).

Run inside `nix develop` (pinned QEMU, NANOX_OVMF_CODE/NANOX_OVMF_VARS):

    python3 tools/svm-probe/run.py [--no-build] [--m1-kernel PATH] [--repro [--runs N]]

With --repro the svm profile runs N times (default 2) with identical inputs
and the per-case digest (verdict, every counter and the serial bytes) of each
run is compared: every case except the host-timed ones must be identical
(docs/research/REPRODUCIBILITY.md); records go to out/svm-repro-<utc>/.

With --m1-kernel, an M1 kernel ELF (built from codex/m1-m8-continuation)
is also handed over (fw_cfg opt/nanox/kernel-m1.elf) and booted with the M1
handoff in its nine test scenarios.

Profiles:
  svm     qemu64 with SVM, nested paging, NRIP save: every case must pass
          (exit status 33, RESULT PASS);
  milan   EPYC-Milan (Zen 3, as the target Ryzen 7 5800H) with the same
          SVM features: the same expectation;
  no-svm  qemu64 without SVM: the probe must report SVM unavailable and
          fail (status 35) instead of passing.

The candidate kernel is out/KERNEL.ELF (`cargo xtask build`), handed to
the probe as fw_cfg file opt/nanox/kernel.elf; the probe boots it as a
guest (m0-* cases). For the svm profile the kernel's serial output under the
VMM is compared with the latest QEMU M0 record of the same scenario
(out/runs/*-<scenario>-boot-test) when that record used the same
KERNEL.ELF: every line must match, except that on the layout line
(`map_bytes=...`) only descriptor_stride, segments and epoch are compared
(map size, reservation count and PML4 address depend on the memory layout).

Records go to out/svm-probe-<utc>/: per profile the argv, serial output,
QEMU stderr and exit status; summary.json with input hashes, verdicts and
the M0 comparison. Exit status 0 when every profile matched its
expectation and no compared scenario differed.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EFI = ROOT / "target/x86_64-unknown-uefi/release/svm-probe.efi"
KERNEL = ROOT / "out/KERNEL.ELF"
# probe case -> M0 harness scenario
M0_SCENARIOS = {
    "m0-pass": "pass",
    "m0-fail": "kernel-fail",
    "m0-panic": "kernel-panic",
    "m0-hang": "kernel-hang",
}
SVM_FLAGS = "+svm,+npt,+nrip-save,+flushbyasid,+vmcb-clean"
PROFILES = [
    ("svm", f"qemu64,{SVM_FLAGS}", 33, "NANOX:SVM-PROBE:RESULT PASS"),
    ("milan", f"EPYC-Milan,{SVM_FLAGS}", 33, "NANOX:SVM-PROBE:RESULT PASS"),
    ("no-svm", "qemu64,-svm", 35, "NANOX:SVM-PROBE:UNAVAILABLE NotSupported"),
]
TIMEOUT_S = 900


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def option(name):
    """Value of `--name VALUE` on the command line, or None."""
    if name in sys.argv:
        i = sys.argv.index(name)
        if i + 1 < len(sys.argv):
            return sys.argv[i + 1]
    return None


M1_KERNEL = option("--m1-kernel")


def run_profile(out: Path, name: str, cpu: str, code: Path, vars_src: Path):
    d = out / name
    esp = d / "esp/EFI/BOOT"
    esp.mkdir(parents=True)
    shutil.copyfile(EFI, esp / "BOOTX64.EFI")
    vars_fd = d / "vars.fd"
    shutil.copyfile(vars_src, vars_fd)
    vars_fd.chmod(0o644)
    serial = d / "serial.log"
    argv = [
        "qemu-system-x86_64",
        "-machine", "q35",
        "-accel", "tcg,thread=single",
        "-cpu", cpu,
        "-smp", "1",
        "-m", "256M",
        "-display", "none",
        "-monitor", "none",
        "-net", "none",
        "-no-reboot",
        "-serial", f"file:{serial}",
        "-device", "isa-debug-exit,iobase=0xf4,iosize=4",
        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
        "-drive", f"if=pflash,format=raw,unit=1,file={vars_fd}",
        "-drive", f"format=raw,file=fat:rw:{d / 'esp'}",
        "-fw_cfg", f"name=opt/nanox/kernel.elf,file={KERNEL}",
    ]
    # The M1 scenarios are long under TCG: run them in one SVM profile.
    if M1_KERNEL and name == "svm":
        argv += ["-fw_cfg", f"name=opt/nanox/kernel-m1.elf,file={M1_KERNEL}"]
    (d / "argv.json").write_text(json.dumps(argv, indent=1) + "\n")
    started = time.monotonic()
    try:
        p = subprocess.run(argv, capture_output=True, timeout=TIMEOUT_S)
        status, stderr = p.returncode, p.stderr
    except subprocess.TimeoutExpired as e:
        status, stderr = "timeout", e.stderr or b""
    (d / "stderr.log").write_bytes(stderr)
    text = serial.read_text(errors="replace") if serial.exists() else ""
    return {
        "cpu": cpu,
        "status": status,
        "seconds": round(time.monotonic() - started, 1),
        "serial_lines": [l for l in text.splitlines() if l.startswith("NANOX:SVM-PROBE")],
    }


def unescape(s: str) -> str:
    return re.sub(r"\\x([0-9a-f]{2})|\\n",
                  lambda m: chr(int(m.group(1), 16)) if m.group(1) else "\n", s)


def guest_serial(lines, case):
    for l in lines:
        m = re.match(rf'NANOX:SVM-PROBE:CASE {re.escape(case)} \w+ .* serial="(.*)"$', l)
        if m:
            return unescape(m.group(1))
    return None


def normalize(text: str):
    out = []
    for line in text.splitlines():
        if line.startswith("map_bytes="):
            f = dict(kv.split("=", 1) for kv in line.split())
            line = " ".join(f"{k}={f.get(k)}" for k in ("descriptor_stride", "segments", "epoch"))
        out.append(line)
    return out


def compare_m0(lines, kernel_sha):
    """Kernel serial under the VMM vs the latest QEMU M0 record."""
    result = {}
    for case, scenario in M0_SCENARIOS.items():
        records = sorted((ROOT / "out/runs").glob(f"*-{scenario}-boot-test"),
                         key=lambda p: p.stat().st_mtime, reverse=True)
        vmm = guest_serial(lines, case)
        if not records or vmm is None:
            result[case] = {"compared": False, "reason": "no record or no case"}
            continue
        rec = records[0]
        if sha256(rec / "KERNEL.ELF") != kernel_sha:
            result[case] = {"compared": False, "record": rec.name,
                            "reason": "record used a different KERNEL.ELF"}
            continue
        qemu = (rec / "serial.bin").read_bytes().decode(errors="replace")
        qemu = qemu[qemu.find("NANOX:KERNEL:ENTER"):]
        a, b = normalize(qemu), normalize(vmm)
        result[case] = {"compared": True, "record": rec.name, "equal": a == b,
                        "qemu": a, "vmm": b}
    return result


# Cases whose result depends on host timing (a host interrupt tick keeps
# virtual time moving for a guest that never exits); every other case runs
# on virtual time alone and must reproduce exactly.
NONDETERMINISTIC = {"m1-preemption"}
DIGEST = re.compile(r"NANOX:SVM-PROBE:CASE (\S+) (?:PASS|FAIL) .*? digest=([0-9a-f]{16})")


def digests(lines):
    return {m.group(1): m.group(2) for l in lines if (m := DIGEST.match(l))}


def repro(runs=2) -> int:
    """Runs the svm profile `runs` times with identical inputs and compares
    the per-case digest (verdict, counters and serial bytes)."""
    code = Path(os.environ["NANOX_OVMF_CODE"])
    vars_src = Path(os.environ["NANOX_OVMF_VARS"])
    out = ROOT / "out" / time.strftime("svm-repro-%Y%m%dT%H%M%SZ", time.gmtime())
    out.mkdir(parents=True)
    qemu = subprocess.run(["qemu-system-x86_64", "--version"],
                          capture_output=True, text=True).stdout.splitlines()[0]
    name, cpu, _, _ = PROFILES[0]
    inputs = {
        "efi_sha256": sha256(EFI),
        "kernel_elf_sha256": sha256(KERNEL),
        "m1_kernel_sha256": sha256(Path(M1_KERNEL)) if M1_KERNEL else None,
        "ovmf_code_sha256": sha256(code),
        "ovmf_vars_sha256": sha256(vars_src),
        "qemu": qemu,
        "cpu": cpu,
    }
    results = []
    for i in range(runs):
        r = run_profile(out / f"run{i}", name, cpu, code, vars_src)
        r["digests"] = digests(r["serial_lines"])
        del r["serial_lines"]
        results.append(r)
        print(f"run {i}: status={r['status']} cases={len(r['digests'])} ({r['seconds']} s)")
    cases = sorted(results[0]["digests"])
    table, ok = {}, True
    for c in cases:
        seen = [r["digests"].get(c) for r in results]
        same = len(set(seen)) == 1 and seen[0] is not None
        by_design = c in NONDETERMINISTIC
        table[c] = {"digests": seen, "identical": same, "host_timed": by_design}
        good = same or by_design
        ok &= good
        tag = "identical" if same else ("differs (host-timed by design)" if by_design else "DIFFERS")
        print(f"  {c:20} {tag}")
    ok &= all(r["status"] == 33 for r in results) and cases == sorted(results[-1]["digests"])
    rec = {"inputs": inputs, "runs": results, "cases": table, "ok": ok,
           "deterministic": sum(t["identical"] for c, t in table.items() if not t["host_timed"]),
           "deterministic_expected": sum(1 for c in table if c not in NONDETERMINISTIC)}
    (out / "repro.json").write_text(json.dumps(rec, indent=1) + "\n")
    print(f"reproduced {rec['deterministic']} of {rec['deterministic_expected']} deterministic cases; records: {out}")
    return 0 if ok else 1


def main() -> int:
    if "--no-build" not in sys.argv:
        subprocess.run(["cargo", "xtask", "build"], cwd=ROOT, check=True)
        subprocess.run(
            ["cargo", "build", "--offline", "--locked", "--release",
             "-p", "svm-probe", "--target", "x86_64-unknown-uefi"],
            cwd=ROOT, check=True,
        )
    if "--repro" in sys.argv:
        return repro(int(option("--runs") or 2))
    code = Path(os.environ["NANOX_OVMF_CODE"])
    vars_src = Path(os.environ["NANOX_OVMF_VARS"])
    out = ROOT / "out" / time.strftime("svm-probe-%Y%m%dT%H%M%SZ", time.gmtime())
    out.mkdir(parents=True)
    qemu = subprocess.run(["qemu-system-x86_64", "--version"],
                          capture_output=True, text=True).stdout.splitlines()[0]
    kernel_sha = sha256(KERNEL)
    summary = {
        "efi_sha256": sha256(EFI),
        "kernel_elf_sha256": kernel_sha,
        "m1_kernel": M1_KERNEL,
        "m1_kernel_sha256": sha256(Path(M1_KERNEL)) if M1_KERNEL else None,
        "ovmf_code_sha256": sha256(code),
        "ovmf_vars_sha256": sha256(vars_src),
        "qemu": qemu,
        "profiles": {},
    }
    ok = True
    for name, cpu, want_status, want_line in PROFILES:
        r = run_profile(out, name, cpu, code, vars_src)
        r["expected_status"] = want_status
        r["expected_line"] = want_line
        r["match"] = r["status"] == want_status and want_line in r["serial_lines"]
        ok &= r["match"]
        summary["profiles"][name] = r
        print(f"{name:7} status={r['status']} match={r['match']} ({r['seconds']} s)")
    cmp = compare_m0(summary["profiles"]["svm"]["serial_lines"], kernel_sha)
    summary["m0_comparison"] = cmp
    for case, c in cmp.items():
        if c["compared"]:
            ok &= c["equal"]
            print(f"{case:9} vs QEMU {c['record']}: equal={c['equal']}")
        else:
            print(f"{case:9} not compared: {c['reason']}")
    summary["match"] = ok
    (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    print(f"records: {out}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
