#!/usr/bin/env python3
"""Runs svm-probe under QEMU TCG with OVMF (docs/specs/M10-VMM.md).

Run inside `nix develop` (pinned QEMU, NANOX_OVMF_CODE/NANOX_OVMF_VARS):

    python3 tools/svm-probe/run.py [--no-build]

Profiles:
  svm     qemu64 with SVM, nested paging, NRIP save: every case must pass
          (exit status 33, RESULT PASS);
  milan   EPYC-Milan (Zen 3, as the target Ryzen 7 5800H) with the same
          SVM features: the same expectation;
  no-svm  qemu64 without SVM: the probe must report SVM unavailable and
          fail (status 35) instead of passing.

Records go to out/svm-probe-<utc>/: per profile the argv, serial output,
QEMU stderr and exit status; summary.json with input hashes and verdicts.
Exit status 0 when every profile matched its expectation.
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EFI = ROOT / "target/x86_64-unknown-uefi/release/svm-probe.efi"
SVM_FLAGS = "+svm,+npt,+nrip-save,+flushbyasid,+vmcb-clean"
PROFILES = [
    ("svm", f"qemu64,{SVM_FLAGS}", 33, "NANOX:SVM-PROBE:RESULT PASS"),
    ("milan", f"EPYC-Milan,{SVM_FLAGS}", 33, "NANOX:SVM-PROBE:RESULT PASS"),
    ("no-svm", "qemu64,-svm", 35, "NANOX:SVM-PROBE:UNAVAILABLE NotSupported"),
]
TIMEOUT_S = 300


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


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
    ]
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


def main() -> int:
    if "--no-build" not in sys.argv:
        subprocess.run(
            ["cargo", "build", "--offline", "--locked", "--release",
             "-p", "svm-probe", "--target", "x86_64-unknown-uefi"],
            cwd=ROOT, check=True,
        )
    code = Path(os.environ["NANOX_OVMF_CODE"])
    vars_src = Path(os.environ["NANOX_OVMF_VARS"])
    out = ROOT / "out" / time.strftime("svm-probe-%Y%m%dT%H%M%SZ", time.gmtime())
    out.mkdir(parents=True)
    qemu = subprocess.run(["qemu-system-x86_64", "--version"],
                          capture_output=True, text=True).stdout.splitlines()[0]
    summary = {
        "efi_sha256": sha256(EFI),
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
    summary["match"] = ok
    (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    print(f"records: {out}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
