#!/usr/bin/env python3
"""Headless QEMU test harness of the NANOX M0 bench.

  harness.py test [NAME ...]    run scenarios (all by default); exit 0 iff every
                                scenario produced its expected verdict
  harness.py run NAME [--echo]  run one scenario, exit 0 iff the guest verdict is PASS
  harness.py list               list scenarios

Every run writes out/runs/<UTC time>-<scenario>/ with record.json (schema
nanox.run-record.v1, docs/m0-bench.md), serial.log and qemu-output.log.

Guest verdict rules (both the serial marker and the exit status are required):
  PASS  exit 33 and exactly one "NANOX: TEST PASS" after "NANOX: loader start",
        "NANOX: loader exit_boot_services ok", "NANOX: kernel_main",
        "NANOX: bootinfo ok" (in this order), and no FAIL/PANIC/LOADER ERROR line
  FAIL  everything else, classified as test_fail (35 + TEST FAIL), panic
        (37 + PANIC), loader_error (39 + LOADER ERROR), timeout (killed by the
        harness), inconsistent (exit status and markers disagree) or
        unexpected_exit (any other status, e.g. 0 after a triple fault with
        -no-reboot)
"""

import argparse
import datetime
import hashlib
import json
import os
import platform
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(REPO / "tools" / "image"))
sys.path.insert(0, str(REPO / "tools"))
import doctor  # noqa: E402
import mkimage  # noqa: E402
import qemu  # noqa: E402

OUT = REPO / "out"
SCENARIOS = REPO / "tests" / "qemu" / "scenarios.json"
LOADER_EFI = OUT / "BOOTX64.EFI"
KERNEL_ELF = OUT / "kernel.elf"

EXIT_PASS, EXIT_FAIL, EXIT_PANIC, EXIT_LOADER = 33, 35, 37, 39
PASS_SEQUENCE = ("NANOX: loader start", "NANOX: loader exit_boot_services ok",
                 "NANOX: kernel_main", "NANOX: bootinfo ok")
ANSI_RE = re.compile(r"\x1b\[[0-9;=?]*[A-Za-z]")
LOADER_ERROR_RE = re.compile(r"^NANOX: LOADER ERROR (E_[A-Z_]+) \((\d+)\)")


# ---------------------------------------------------------------------------
# Classification (pure; unit-tested in tests/host/test_harness.py)

def serial_lines(text):
    """Serial text -> list of lines without CR and ANSI escapes."""
    return [ANSI_RE.sub("", line).rstrip("\r") for line in text.replace("\r\n", "\n").split("\n")]


def classify(serial_text, exit_status, timed_out):
    """Returns dict(verdict, failure_class, loader_error, reason)."""
    lines = serial_lines(serial_text)
    markers = [l for l in lines if l.startswith("NANOX: ")]
    passes = [i for i, l in enumerate(markers) if l == "NANOX: TEST PASS"]
    fails = [l for l in markers if l.startswith("NANOX: TEST FAIL")]
    panics = [l for l in markers if l.startswith("NANOX: PANIC")]
    loader_errors = [m for m in (LOADER_ERROR_RE.match(l) for l in markers) if m]
    loader_error = loader_errors[0].group(1) if loader_errors else None

    def result(verdict, cls, reason):
        return {"verdict": verdict, "failure_class": cls, "loader_error": loader_error,
                "reason": reason}

    if timed_out:
        return result("FAIL", "timeout", "no exit before the harness timeout")
    if exit_status == EXIT_PASS:
        if fails or panics or loader_errors:
            return result("FAIL", "inconsistent", "exit 33 but failure markers present")
        if len(passes) != 1:
            return result("FAIL", "inconsistent",
                          "exit 33 with %d 'NANOX: TEST PASS' lines" % len(passes))
        pos = -1
        for want in PASS_SEQUENCE:
            idx = next((i for i, l in enumerate(markers)
                        if i > pos and (l == want or l.startswith(want + " "))), None)
            if idx is None:
                return result("FAIL", "inconsistent", "missing or out-of-order marker %r" % want)
            pos = idx
        if passes[0] < pos:
            return result("FAIL", "inconsistent", "TEST PASS before boot sequence completed")
        return result("PASS", None, "exit 33 and complete marker sequence")
    if exit_status == EXIT_FAIL:
        if fails:
            return result("FAIL", "test_fail", fails[0])
        return result("FAIL", "inconsistent", "exit 35 without 'NANOX: TEST FAIL'")
    if exit_status == EXIT_PANIC:
        if panics:
            return result("FAIL", "panic", panics[0])
        return result("FAIL", "inconsistent", "exit 37 without 'NANOX: PANIC'")
    if exit_status == EXIT_LOADER:
        if loader_errors:
            return result("FAIL", "loader_error", loader_errors[0].string)
        return result("FAIL", "inconsistent", "exit 39 without 'NANOX: LOADER ERROR'")
    return result("FAIL", "unexpected_exit", "exit status %r" % (exit_status,))


def check_expectation(outcome, expect, serial_text, substitutions):
    """Returns a list of human-readable mismatches (empty = expectation met)."""
    problems = []
    for key in ("verdict", "failure_class", "loader_error"):
        if key in expect and outcome.get(key) != expect[key]:
            problems.append("%s: expected %r, got %r" % (key, expect[key], outcome.get(key)))
    text = "\n".join(serial_lines(serial_text))
    for pattern in expect.get("patterns", []):
        pattern = pattern.format(**substitutions)
        if not re.search(pattern, text, re.MULTILINE):
            problems.append("serial pattern not found: %s" % pattern)
    return problems


# ---------------------------------------------------------------------------
# Running

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def artifact(path):
    path = Path(path)
    if not path.is_file():
        return {"path": str(path), "sha256": None, "size": None}
    return {"path": str(path), "sha256": sha256_file(path), "size": path.stat().st_size}


def git_source():
    def git(*args):
        return subprocess.run(["git", "-C", str(REPO)] + list(args), stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, check=True).stdout.decode().strip()
    try:
        rev = git("rev-parse", "HEAD")
        status = git("status", "--porcelain", "--untracked-files=normal")
        return {"git_rev": rev, "git_dirty": bool(status),
                "git_dirty_paths": [l[3:] for l in status.splitlines()][:50]}
    except (OSError, subprocess.CalledProcessError):
        return {"git_rev": None, "git_dirty": None, "git_dirty_paths": []}


def display_path(path):
    try:
        return Path(path).resolve().relative_to(REPO)
    except ValueError:
        return path


def load_scenarios():
    data = json.loads(SCENARIOS.read_text())
    assert data["schema"] == "nanox.scenarios.v1"
    for sc in data["scenarios"]:
        sc.setdefault("timeout_s", data["default_timeout_s"])
        sc.setdefault("image", {})
        sc.setdefault("qemu_extra", [])
    return data["scenarios"]


def build_scenario_image(sc):
    spec = sc["image"]
    img = mkimage.build_image(LOADER_EFI.read_bytes(), KERNEL_ELF.read_bytes(),
                              cmdline=spec.get("cmdline", ""),
                              omit_kernel=spec.get("omit_kernel", False),
                              corrupt_kernel=spec.get("corrupt_kernel", False))
    path = OUT / "images" / ("%s.img" % sc["name"])
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".img.tmp")
    tmp.write_bytes(img)
    os.replace(tmp, path)
    return path


def new_run_dir(runs_root, name):
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    base = Path(runs_root) / ("%s-%s" % (stamp, name))
    path, n = base, 1
    while path.exists():
        n += 1
        path = Path("%s-%d" % (base, n))
    path.mkdir(parents=True)
    return path


def run_scenario(sc, runs_root, toolchain, echo=False):
    image = build_scenario_image(sc)
    run_dir = new_run_dir(runs_root, sc["name"])
    serial_path = run_dir / "serial.log"
    serial_path.touch()
    vars_path = qemu.prepare_vars(run_dir / "OVMF_VARS.fd")
    argv = qemu.base_argv(image, vars_path, "file:%s" % serial_path, extra=sc["qemu_extra"])

    started = datetime.datetime.now(datetime.timezone.utc)
    t0 = time.monotonic()
    timed_out = False
    with open(run_dir / "qemu-output.log", "wb") as qout, open(serial_path, "rb") as tail:
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=qout,
                                stderr=subprocess.STDOUT)
        deadline = t0 + sc["timeout_s"]
        while proc.poll() is None:
            if echo:
                sys.stdout.write(tail.read().decode("utf-8", "replace"))
                sys.stdout.flush()
            if time.monotonic() >= deadline:
                timed_out = True
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
                break
            time.sleep(0.05)
        if echo:
            sys.stdout.write(tail.read().decode("utf-8", "replace"))
            sys.stdout.flush()
    duration = time.monotonic() - t0
    exit_status = proc.returncode
    os.unlink(vars_path)  # 540 KiB per run; the template hash is recorded instead

    serial_bytes = serial_path.read_bytes()
    serial_text = serial_bytes.decode("utf-8", "replace")
    outcome = classify(serial_text, None if timed_out else exit_status, timed_out)
    kernel_sha = sha256_file(KERNEL_ELF)
    problems = check_expectation(outcome, sc["expect"], serial_text,
                                 {"kernel_sha256": kernel_sha})
    record = {
        "schema": "nanox.run-record.v1",
        "scenario": sc["name"],
        "description": sc.get("description", ""),
        "started_utc": started.isoformat(timespec="seconds"),
        "duration_s": round(duration, 3),
        "source": git_source(),
        "qemu": {
            "argv": argv,
            "command_line": shlex.join(argv),
            "timeout_s": sc["timeout_s"],
        },
        "image_spec": sc["image"],
        "artifacts": {
            "image": artifact(image),
            "loader_efi": artifact(LOADER_EFI),
            "kernel_elf": artifact(KERNEL_ELF),
            "ovmf_code": artifact(qemu.ovmf_code_path()),
            "ovmf_vars_template": artifact(qemu.ovmf_vars_template_path()),
        },
        "toolchain": toolchain,
        "host": {"system": platform.system(), "release": platform.release(),
                 "machine": platform.machine(), "python": platform.python_version()},
        "result": {
            "exit_status": None if timed_out else exit_status,
            "timed_out": timed_out,
            "killed_returncode": exit_status if timed_out else None,
        },
        "serial": {
            "path": "serial.log",
            "sha256": hashlib.sha256(serial_bytes).hexdigest(),
            "markers": [l for l in serial_lines(serial_text) if l.startswith("NANOX: ")],
            "raw": serial_text,
        },
        "verdict": outcome["verdict"],
        "failure_class": outcome["failure_class"],
        "loader_error": outcome["loader_error"],
        "reason": outcome["reason"],
        "expected": sc["expect"],
        "expectation_met": not problems,
        "expectation_problems": problems,
    }
    (run_dir / "record.json").write_text(json.dumps(record, indent=2) + "\n")
    return record, run_dir


def toolchain_snapshot():
    report, ok = doctor.evaluate()
    tools = {k: v["version"] for k, v in report["detected"]["tools"].items()}
    return {"profile": report["profile"], "lock_ok": ok, "versions": tools}


def require_artifacts():
    missing = [str(p) for p in (LOADER_EFI, KERNEL_ELF) if not p.is_file()]
    if missing:
        sys.stderr.write("harness: missing %s; run `make` first\n" % ", ".join(missing))
        sys.exit(2)


def cmd_test(args):
    require_artifacts()
    scenarios = load_scenarios()
    if args.names:
        unknown = set(args.names) - {s["name"] for s in scenarios}
        if unknown:
            sys.stderr.write("harness: unknown scenario(s): %s\n" % ", ".join(sorted(unknown)))
            return 2
        scenarios = [s for s in scenarios if s["name"] in args.names]
    toolchain = toolchain_snapshot()
    rows, all_ok = [], True
    for sc in scenarios:
        record, run_dir = run_scenario(sc, args.runs_dir, toolchain)
        ok = record["expectation_met"]
        all_ok &= ok
        status = record["result"]["exit_status"]
        rows.append({"scenario": sc["name"], "ok": ok, "verdict": record["verdict"],
                     "failure_class": record["failure_class"],
                     "loader_error": record["loader_error"],
                     "exit_status": status, "timed_out": record["result"]["timed_out"],
                     "duration_s": record["duration_s"],
                     "record": str(run_dir / "record.json")})
        print("%-4s %-15s verdict=%-4s class=%-12s exit=%-4s %6.1fs  %s" % (
            "ok" if ok else "FAIL", sc["name"], record["verdict"],
            record["failure_class"] or "-", "T/O" if status is None else status,
            record["duration_s"], display_path(run_dir)))
        for p in record["expectation_problems"]:
            print("       " + p)
        sys.stdout.flush()
    summary_dir = new_run_dir(args.runs_dir, "suite")
    summary = {"schema": "nanox.suite-summary.v1", "source": git_source(),
               "toolchain": toolchain, "all_expectations_met": all_ok, "runs": rows}
    (summary_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print("harness: %d/%d scenarios as expected; summary %s" % (
        sum(r["ok"] for r in rows), len(rows), summary_dir / "summary.json"))
    return 0 if all_ok else 1


def cmd_run(args):
    require_artifacts()
    scenarios = {s["name"]: s for s in load_scenarios()}
    if args.name not in scenarios:
        sys.stderr.write("harness: unknown scenario %s\n" % args.name)
        return 2
    record, run_dir = run_scenario(scenarios[args.name], args.runs_dir, toolchain_snapshot(),
                                   echo=args.echo)
    print("\nharness: %s verdict=%s class=%s exit=%s record=%s" % (
        args.name, record["verdict"], record["failure_class"] or "-",
        record["result"]["exit_status"], run_dir / "record.json"))
    return 0 if record["verdict"] == "PASS" else 1


def cmd_list(args):
    for sc in load_scenarios():
        print("%-15s %s" % (sc["name"], sc.get("description", "")))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--runs-dir", default=str(OUT / "runs"))
    sub = ap.add_subparsers(dest="command", required=True)
    t = sub.add_parser("test")
    t.add_argument("names", nargs="*")
    t.set_defaults(func=cmd_test)
    r = sub.add_parser("run")
    r.add_argument("name")
    r.add_argument("--echo", action="store_true")
    r.set_defaults(func=cmd_run)
    sub.add_parser("list").set_defaults(func=cmd_list)
    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
