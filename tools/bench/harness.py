#!/usr/bin/env python3
"""Headless QEMU test harness of the NANOX M0 bench.

  harness.py test [NAME ...]    run scenarios (all by default); exit 0 iff every
                                scenario produced its expected verdict
  harness.py run NAME [--echo]  run one scenario, exit 0 iff the guest verdict is PASS
  harness.py repeat NAME... [--count N]
                                run each scenario N times (default 3) and require the
                                expected verdict every time and identical serial
                                markers (after masking timing values); a scenario may
                                name marker lines whose order legitimately varies
                                (repeat.unordered): those are compared as a multiset
  harness.py list               list scenarios

Every run writes out/runs/<UTC time>-<scenario>/ with record.json (schema
nanox.run-record.v1, docs/m0-bench.md), serial.log and qemu-output.log.

Guest verdict rules (both the serial marker and the exit status are required):
  PASS  exit 33 and exactly one "NANOX: TEST PASS" after "NANOX: loader start",
        "NANOX: loader exit_boot_services ok", "NANOX: kernel_main",
        "NANOX: bootinfo ok" (in this order), and no FAIL/PANIC/LOADER ERROR line
  FAIL  everything else, classified as test_fail (35 + TEST FAIL), panic
        (37 + PANIC), loader_error (39 + LOADER ERROR), exception (41 +
        EXCEPTION report), timeout (killed by the harness), inconsistent (exit
        status and markers disagree) or unexpected_exit (any other status, e.g.
        0 after a triple fault with -no-reboot)

Exception reports and BACKTRACE lines are parsed and symbolised with the
symbol table of out/kernel.elf (tools/bench/elfsym.py) and stored in the run
record.
"""

import argparse
import datetime
import hashlib
import json
import os
import platform
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(REPO / "tools" / "image"))
sys.path.insert(0, str(REPO / "tools"))
sys.path.insert(0, str(REPO / "tools" / "bridge"))
import doctor  # noqa: E402
import elfsym  # noqa: E402
import mkimage  # noqa: E402
import qemu  # noqa: E402
import session as bridge_session  # noqa: E402

OUT = REPO / "out"
SCENARIOS = REPO / "tests" / "qemu" / "scenarios.json"
LOADER_EFI = OUT / "BOOTX64.EFI"
KERNEL_ELF = OUT / "kernel.elf"
INITRD = OUT / "initrd.img"

EXIT_PASS, EXIT_FAIL, EXIT_PANIC, EXIT_LOADER, EXIT_EXCEPTION = 33, 35, 37, 39, 41
PASS_SEQUENCE = ("NANOX: loader start", "NANOX: loader exit_boot_services ok",
                 "NANOX: kernel_main", "NANOX: bootinfo ok")
ANSI_RE = re.compile(r"\x1b\[[0-9;=?]*[A-Za-z]")
LOADER_ERROR_RE = re.compile(r"^NANOX: LOADER ERROR (E_[A-Z_]+) \((\d+)\)")
EXCEPTION_RE = re.compile(r"^NANOX: EXCEPTION (\S+) vector=(\d+) error=0x([0-9a-f]+) "
                          r"rip=0x([0-9a-f]+) cr2=0x([0-9a-f]+)$")
BACKTRACE_RE = re.compile(r"^NANOX: BACKTRACE (\d+) 0x([0-9a-f]+)$")


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
    exceptions = [l for l in markers if l.startswith("NANOX: EXCEPTION")]
    loader_errors = [m for m in (LOADER_ERROR_RE.match(l) for l in markers) if m]
    loader_error = loader_errors[0].group(1) if loader_errors else None

    def result(verdict, cls, reason):
        return {"verdict": verdict, "failure_class": cls, "loader_error": loader_error,
                "reason": reason}

    if timed_out:
        return result("FAIL", "timeout", "no exit before the harness timeout")
    if exit_status == EXIT_PASS:
        if fails or panics or loader_errors or exceptions:
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
    if exit_status == EXIT_EXCEPTION:
        if exceptions:
            return result("FAIL", "exception", exceptions[0])
        return result("FAIL", "inconsistent", "exit 41 without 'NANOX: EXCEPTION'")
    return result("FAIL", "unexpected_exit", "exit status %r" % (exit_status,))


def analyze_report(serial_text, symbolizer=None):
    """Parses the first exception report and the BACKTRACE lines.

    Returns {"exception": {...} or None, "backtrace": [...]}.  Frame 0 of an
    exception is the faulting RIP; other frames are return addresses and are
    symbolised at address - 1 (the call instruction)."""
    exception, backtrace = None, []
    for line in serial_lines(serial_text):
        m = EXCEPTION_RE.match(line)
        if m and exception is None:
            exception = {"mnemonic": m.group(1), "vector": int(m.group(2)),
                         "error": int(m.group(3), 16), "rip": "0x" + m.group(4),
                         "cr2": "0x" + m.group(5), "rip_symbol": None}
            if symbolizer:
                exception["rip_symbol"] = symbolizer.describe(int(m.group(4), 16))
            continue
        m = BACKTRACE_RE.match(line)
        if m:
            index, addr = int(m.group(1)), int(m.group(2), 16)
            look = addr if (index == 0 and exception) else addr - 1
            backtrace.append({"index": index, "addr": "0x%016x" % addr,
                              "symbol": symbolizer.describe(look) if symbolizer else None})
    return {"exception": exception, "backtrace": backtrace}


def _function(symbol):
    return symbol.split("+", 1)[0] if symbol else None


def check_bridge(expect, bridge):
    """M3: compares the host bridge session with expect["bridge"] =
    {"ok": bool, "problems": [regex, ...]}: every regex must match one of
    the problems the host-side checks reported."""
    problems = []
    if bridge is None:
        return ["bridge: expected a host bridge session, none ran"]
    if "ok" in expect and bridge.get("ok") != expect["ok"]:
        problems.append("bridge.ok: expected %r, got %r (problems: %s)" % (
            expect["ok"], bridge.get("ok"), "; ".join(bridge.get("problems", [])[:5])))
    for pattern in expect.get("problems", []):
        if not any(re.search(pattern, p) for p in bridge.get("problems", [])):
            problems.append("bridge: no host-side problem matches %s (have %s)" % (
                pattern, bridge.get("problems", [])[:5]))
    return problems


def check_expectation(outcome, expect, serial_text, substitutions, report=None, bridge=None):
    """Returns a list of human-readable mismatches (empty = expectation met)."""
    problems = []
    for key in ("verdict", "failure_class", "loader_error"):
        if key in expect and outcome.get(key) != expect[key]:
            problems.append("%s: expected %r, got %r" % (key, expect[key], outcome.get(key)))
    report = report or {"exception": None, "backtrace": []}
    if "exception" in expect:
        exc = report["exception"]
        if exc is None:
            problems.append("exception report expected, none parsed")
        else:
            for key, want in expect["exception"].items():
                have = _function(exc["rip_symbol"]) if key == "rip_function" else exc.get(key)
                if key == "cr2_is_rip":  # instruction fetch faults: CR2 is the RIP
                    have = int(exc["cr2"], 16) == int(exc["rip"], 16)
                if key == "cr2":
                    have = int(exc["cr2"], 16)
                    want = int(want, 16)
                if have != want:
                    problems.append("exception.%s: expected %r, got %r" % (key, want, have))
    if "backtrace_functions" in expect:
        have = [_function(f["symbol"]) for f in report["backtrace"]]
        for fn in expect["backtrace_functions"]:
            if fn not in have:
                problems.append("backtrace lacks %s (have %s)" % (fn, have))
    text = "\n".join(serial_lines(serial_text))
    for pattern in expect.get("patterns", []):
        pattern = pattern.format(**substitutions)
        if not re.search(pattern, text, re.MULTILINE):
            problems.append("serial pattern not found: %s" % pattern)
    if "interleave" in expect:
        problems += check_interleave(serial_text, expect["interleave"])
    if "bridge" in expect:
        problems += check_bridge(expect["bridge"], bridge)
    return problems


def check_interleave(serial_text, spec):
    """Checks that several producers made progress interleaved (M2 scheduler).

    spec = {"pattern": regex with one group naming the producer, "groups": N}.
    The marker lines matching the pattern must come from exactly N producers,
    and every producer's first line must precede every producer's last line
    (no producer finished before all others had started reporting)."""
    rx = re.compile(spec["pattern"])
    first, last = {}, {}
    markers = [l for l in serial_lines(serial_text) if l.startswith("NANOX: ")]
    for i, line in enumerate(markers):
        m = rx.match(line)
        if m:
            first.setdefault(m.group(1), i)
            last[m.group(1)] = i
    if len(first) != spec["groups"]:
        return ["interleave: expected %d producers matching %s, found %d (%s)" % (
            spec["groups"], spec["pattern"], len(first), sorted(first))]
    latest_first = max(first.values())
    earliest_last = min(last.values())
    if latest_first >= earliest_last:
        who_first = [k for k, v in first.items() if v == latest_first][0]
        who_last = [k for k, v in last.items() if v == earliest_last][0]
        return ["interleave: %s reported its last line (marker %d) before %s reported its "
                "first (marker %d)" % (who_last, earliest_last, who_first, latest_first)]
    return []


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
    initrd = INITRD.read_bytes()
    if "initrd_text" in spec:  # replacement initramfs, still described by the manifest
        initrd = spec["initrd_text"].encode("ascii")
    img = mkimage.build_image(LOADER_EFI.read_bytes(), KERNEL_ELF.read_bytes(), initrd,
                              cmdline=spec.get("cmdline", ""),
                              omit_kernel=spec.get("omit_kernel", False),
                              corrupt_kernel=spec.get("corrupt_kernel", False),
                              omit_initrd=spec.get("omit_initrd", False),
                              corrupt_initrd=spec.get("corrupt_initrd", False))
    path = OUT / "images" / ("%s.img" % sc["name"])
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".img.tmp%d" % os.getpid())
    tmp.write_bytes(img)
    os.replace(tmp, path)
    return path


def new_run_dir(runs_root, name):
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    base = Path(runs_root) / ("%s-%s" % (stamp, name))
    Path(runs_root).mkdir(parents=True, exist_ok=True)
    path, n = base, 1
    while True:
        try:
            path.mkdir()  # atomic: concurrent harness processes get distinct dirs
            return path
        except FileExistsError:
            n += 1
            path = Path("%s-%d" % (base, n))


class BridgeRunner:
    """M3: the host bridge for one run.  Listens on a unix socket (in a short
    temporary directory: socket paths are limited to ~100 bytes), QEMU
    connects COM2 to it, and a thread runs the scenario's bridge session."""

    ACCEPT_TIMEOUT_S = 120

    def __init__(self, spec, run_dir, adapter_override=None):
        self.spec = dict(spec)
        if adapter_override:
            self.spec["adapter"] = adapter_override
        self.run_dir = run_dir
        self.dir = tempfile.mkdtemp(prefix="nxb-")
        self.path = os.path.join(self.dir, "bridge.sock")
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.path)
        self.listener.listen(1)
        self.conn = None
        self.result = None
        self.thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self.thread.start()

    def _run(self):
        try:
            self.listener.settimeout(self.ACCEPT_TIMEOUT_S)
            self.conn, _ = self.listener.accept()
            self.result = bridge_session.run_session(
                self.conn, self.spec["script"], self.spec.get("adapter"),
                self.spec.get("request"), str(self.run_dir / "bridge-trace.jsonl"),
                str(self.run_dir / "bridge-wire.log"))
        except OSError as e:
            self.result = {"ok": False, "problems": ["bridge_error: %s" % e]}

    def finish(self):
        self.thread.join(timeout=15)
        if self.thread.is_alive() and self.conn:
            try:
                self.conn.shutdown(socket.SHUT_RDWR)  # unblocks a pending read
            except OSError:
                pass
            self.thread.join(timeout=5)
        for s in (self.conn, self.listener):
            if s:
                s.close()
        shutil.rmtree(self.dir, ignore_errors=True)
        res = self.result or {"ok": False, "problems": ["bridge_error: session did not finish"]}
        res["spec"] = self.spec
        return res


def run_scenario(sc, runs_root, toolchain, echo=False, adapter_override=None):
    image = build_scenario_image(sc)
    run_dir = new_run_dir(runs_root, sc["name"])
    serial_path = run_dir / "serial.log"
    serial_path.touch()
    vars_path = qemu.prepare_vars(run_dir / "OVMF_VARS.fd")
    bridge = BridgeRunner(sc["bridge"], run_dir, adapter_override) if sc.get("bridge") else None
    argv = qemu.base_argv(image, vars_path, "file:%s" % serial_path, extra=sc["qemu_extra"],
                          bridge_socket=bridge.path if bridge else None)

    started = datetime.datetime.now(datetime.timezone.utc)
    t0 = time.monotonic()
    timed_out = False
    with open(run_dir / "qemu-output.log", "wb") as qout, open(serial_path, "rb") as tail:
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=qout,
                                stderr=subprocess.STDOUT)
        if bridge:
            bridge.start()
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
    bridge_result = bridge.finish() if bridge else None
    os.unlink(vars_path)  # 540 KiB per run; the template hash is recorded instead

    serial_bytes = serial_path.read_bytes()
    serial_text = serial_bytes.decode("utf-8", "replace")
    outcome = classify(serial_text, None if timed_out else exit_status, timed_out)
    subs = {"kernel_sha256": sha256_file(KERNEL_ELF), "initrd_sha256": sha256_file(INITRD)}
    report = analyze_report(serial_text, elfsym.Symbolizer(KERNEL_ELF))
    problems = check_expectation(outcome, sc["expect"], serial_text, subs, report, bridge_result)
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
            "initrd": artifact(INITRD),
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
        "exception": report["exception"],
        "backtrace": report["backtrace"],
        "bridge": bridge_result,
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
    missing = [str(p) for p in (LOADER_EFI, KERNEL_ELF, INITRD) if not p.is_file()]
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
        print("%-4s %-18s verdict=%-4s class=%-12s exit=%-4s %6.1fs  %s" % (
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
                                   echo=args.echo, adapter_override=args.adapter)
    print("\nharness: %s verdict=%s class=%s exit=%s record=%s" % (
        args.name, record["verdict"], record["failure_class"] or "-",
        record["result"]["exit_status"], run_dir / "record.json"))
    return 0 if record["verdict"] == "PASS" else 1


# Serial values that legitimately differ between runs: timing measurements
# (M1) and scheduling statistics (M2: where the timer happened to preempt).
VOLATILE_RE = re.compile(r"\b(tsc_delta|ticks|lapic_per_10ms|preempted|preemptions|switches|"
                         r"latest_first|earliest_last)=[0-9]+(,[0-9]+)*")


def normalized_markers(markers):
    return [VOLATILE_RE.sub(lambda m: m.group(1) + "=*", l) for l in markers]


def repeat_view(markers, unordered=None):
    """Normalised markers split for comparison between runs.

    Returns (ordered, unordered): lines matching the `unordered` regex (whose
    relative order may vary, e.g. output of preempted tasks) are taken out of
    the sequence and returned sorted, i.e. compared as a multiset; all other
    lines keep their order."""
    lines = normalized_markers(markers)
    if not unordered:
        return lines, []
    rx = re.compile(unordered)
    return ([l for l in lines if not rx.search(l)], sorted(l for l in lines if rx.search(l)))


def cmd_repeat(args):
    require_artifacts()
    scenarios = {s["name"]: s for s in load_scenarios()}
    unknown = [n for n in args.names if n not in scenarios]
    if unknown:
        sys.stderr.write("harness: unknown scenario(s): %s\n" % ", ".join(unknown))
        return 2
    toolchain = toolchain_snapshot()
    all_ok = True
    for name in args.names:
        unordered = scenarios[name].get("repeat", {}).get("unordered")
        runs, reference, problems = [], None, []
        for i in range(args.count):
            record, run_dir = run_scenario(scenarios[name], args.runs_dir, toolchain)
            ordered, unordered_lines = repeat_view(record["serial"]["markers"], unordered)
            runs.append({"record": str(run_dir / "record.json"),
                         "expectation_met": record["expectation_met"],
                         "markers_sha256": hashlib.sha256(
                             "\n".join(ordered + ["--"] + unordered_lines).encode()).hexdigest(),
                         "unordered_lines": len(unordered_lines)})
            if not record["expectation_met"]:
                problems.append("run %d: %s" % (i + 1, "; ".join(record["expectation_problems"])))
            if reference is None:
                reference = (ordered, unordered_lines)
                continue
            if ordered != reference[0]:
                diff = [(a, b) for a, b in zip(reference[0], ordered) if a != b][:3]
                problems.append("run %d: markers differ from run 1 (%d vs %d lines), first: %s"
                                % (i + 1, len(reference[0]), len(ordered), diff))
            if unordered_lines != reference[1]:
                gone = sorted(set(reference[1]) - set(unordered_lines))[:3]
                new = sorted(set(unordered_lines) - set(reference[1]))[:3]
                problems.append("run %d: unordered lines differ from run 1 as a multiset "
                                "(%d vs %d lines), only in run 1: %s, only here: %s"
                                % (i + 1, len(reference[1]), len(unordered_lines), gone, new))
        ok = not problems
        all_ok &= ok
        identical = len({r["markers_sha256"] for r in runs}) == 1
        out_dir = new_run_dir(args.runs_dir, "repeat-" + name)
        (out_dir / "repeat.json").write_text(json.dumps({
            "schema": "nanox.repeat.v1", "scenario": name, "count": args.count,
            "volatile_fields": VOLATILE_RE.pattern, "unordered": unordered,
            "source": git_source(), "runs": runs, "identical_markers": identical,
            "problems": problems, "ok": ok}, indent=2) + "\n")
        print("%-4s repeat %-18s %d runs, %d marker lines each%s, identical=%s  %s" % (
            "ok" if ok else "FAIL", name, args.count, len(reference[0]) if reference else 0,
            " + %d unordered" % len(reference[1]) if reference and unordered else "",
            identical, display_path(out_dir)))
        for p in problems:
            print("       " + p)
    return 0 if all_ok else 1


def cmd_list(args):
    for sc in load_scenarios():
        print("%-18s %s" % (sc["name"], sc.get("description", "")))
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
    r.add_argument("--adapter", default=None,
                   help="M3: model adapter of the host bridge (mock, unavailable, anthropic)")
    r.set_defaults(func=cmd_run)
    rp = sub.add_parser("repeat")
    rp.add_argument("names", nargs="+")
    rp.add_argument("--count", type=int, default=3)
    rp.set_defaults(func=cmd_repeat)
    sub.add_parser("list").set_defaults(func=cmd_list)
    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
