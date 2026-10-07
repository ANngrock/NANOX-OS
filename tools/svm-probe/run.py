#!/usr/bin/env python3
"""Runs svm-probe under QEMU TCG with OVMF (docs/specs/M10-VMM.md).

Run inside `nix develop` (pinned QEMU, NANOX_OVMF_CODE/NANOX_OVMF_VARS):

    python3 tools/svm-probe/run.py [--no-build] [--m1-kernel PATH] [--repro [--runs N]]
        [--linux-kernel BZIMAGE [--linux-init ELF] [--linux-cmdline TEXT] [--linux-only]
         [--linux-host-tick NS|off] [--linux-timeout S] [--linux-disk IMAGE]
         [--show | --shell-test] [--linux-busybox BUSYBOX]]

With --repro the svm profile runs N times (default 2) with identical inputs
and the per-case digest (verdict, every counter and the serial bytes) of each
run is compared: every case except the host-timed ones must be identical
(docs/research/REPRODUCIBILITY.md); records go to out/svm-repro-<utc>/.

With --m1-kernel, an M1 kernel ELF (built from codex/m1-m8-continuation)
is also handed over (fw_cfg opt/nanox/kernel-m1.elf) and booted with the M1
handoff in its nine test scenarios.

With --linux-kernel, a fourth profile, linux, hands a Linux bzImage (fw_cfg
opt/nanox/bzimage), an initramfs with the measurement init
(tools/hostguest/init, built by tools/native/build.py; the archive is made by
linux_surface.py's cpio_newc) and the command line to the probe, whose
`linux` case boots it on the whole emulated platform and passes when the
guest prints NANOX_GUEST_REPORT_END (docs/specs/M11-WINDOW.md §5). The
profile has 1 GiB (the case allocates 256 MiB of guest RAM from the
firmware) and a long timeout: nested paging under TCG is slow. --linux-only
runs only that profile. The guest also has a screen (a linear framebuffer the
kernel's console draws on); the probe dumps it at the end and it is saved as
linux/screen.png.

With --show the linux profile runs in a window instead (QEMU with GTK, from
`nix build .#qemu-display --out-link ~/.nix-qemu-display`; on Windows 11 the
window opens through WSLg): the NANOX screen the probe draws shows the guest
booting in the server window, and at the end the probe halts instead of
ending the run, so the last screen stays until the window is closed. The
verdict then comes from the RESULT line, not from the exit status, and
there is no timeout: the run lasts until the window is closed.

--show is also interactive: the initramfs gets a static busybox (from
`nix build .#guest-busybox --out-link ~/.nix-guest-busybox`, or
--linux-busybox), the command line `nanox.shell`, and after its report the
init starts a busybox shell on the guest's first virtual terminal; the
probe hands what is typed into the window on to the guest's keyboard
(fw_cfg opt/nanox/interactive) and runs without limits. `exit` in the
shell powers the guest off. --shell-test checks that path without a
window: QEMU's own keyboard gets commands through QMP (send-key) once the
shell is up: one writes a line to the guest's serial port, one sleeps 3 s
first (real time: the probe keeps the guest's virtual time from running
ahead of its own clock, so at least 3 s must pass between the Enter and
the line), then `exit`. The run passes when both lines arrive in time and
the guest powers off.

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
import struct
import subprocess
import sys
import time
import zlib
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
LINUX_KERNEL = option("--linux-kernel")
LINUX_INIT = option("--linux-init")
LINUX_INIT_DEFAULT = ROOT / "target/x86_64-unknown-none/release/linux-probe-init"
# The kernel command line of the linux profile, each item with its reason
# (docs/specs/M11-WINDOW.md, "Первый запуск Linux под VMM NANOX"):
LINUX_CMDLINE = option("--linux-cmdline") or " ".join([
    "console=tty0",  # the kernel's messages on the screen too
    "console=ttyS0",  # the kernel's console on COM1, which the probe prints (the last
                      # console= is /dev/console: init's output goes to COM1)
    "earlyprintk=serial,ttyS0,115200",  # output before the 8250 driver is up
    "panic=-1",  # a panic reboots at once: the VMM sees Reset, not a hang
])
# Virtual time (ns) a host tick exit (~1 ms of host time) counts, or "off"
# (the probe's default when absent: no tick, a run independent of host timing;
# fw_cfg opt/nanox/host-tick-ns).
LINUX_HOST_TICK = option("--linux-host-tick")
# A disk image for the guest's virtio-blk; without one a small test disk is made.
LINUX_DISK = option("--linux-disk")
LINUX_TIMEOUT_S = int(option("--linux-timeout") or 7200)
LINUX_PROFILE = ("linux", f"qemu64,{SVM_FLAGS}", 33, "NANOX:SVM-PROBE:RESULT PASS")
# --show: the linux profile in a window, with a QEMU that has one.
SHOW = "--show" in sys.argv
# --shell-test: the interactive shell driven through QMP, without a window.
SHELL_TESTING = "--shell-test" in sys.argv
INTERACTIVE = SHOW or SHELL_TESTING
LINUX_BUSYBOX = option("--linux-busybox") or os.path.expanduser(
    "~/.nix-guest-busybox/bin/busybox")
if INTERACTIVE:
    LINUX_CMDLINE += " nanox.shell"
# What --shell-test types, line by line, each with the mark it waits for on the
# guest's serial port and the least real time that must pass from its Enter to the mark.
SHELL_TEST = [
    ("echo NANOX_SHELL_OK $(uname -r) $(ls /bin | wc -l) > /dev/ttyS0\n", "NANOX_SHELL_OK", 0),
    ("sleep 3; echo NANOX_SHELL_SLEPT > /dev/ttyS0\n", "NANOX_SHELL_SLEPT", 3),
    ("exit\n", None, 0),
]
SHELL_TEST_WAIT_S = 120
QEMU_DISPLAY = os.environ.get("NANOX_QEMU_DISPLAY",
                              os.path.expanduser("~/.nix-qemu-display/bin/qemu-system-x86_64"))


def linux_initrd(out: Path) -> Path:
    """The measurement initramfs: /init only, as linux_surface.py makes it; for an
    interactive run also /bin/busybox."""
    sys.path.insert(0, str(ROOT / "tools/hostguest"))
    import linux_surface  # noqa: E402

    init = Path(LINUX_INIT) if LINUX_INIT else LINUX_INIT_DEFAULT
    files = [("init", 0o100755, init.read_bytes())]
    if INTERACTIVE:
        files += [("bin", 0o040755, b""),
                  ("bin/busybox", 0o100755, Path(LINUX_BUSYBOX).read_bytes())]
    img = out / "initrd.img"
    img.write_bytes(linux_surface.cpio_newc(files))
    return img


# QMP key names of the characters --shell-test types (a list: shift first).
QCODES = {" ": ["spc"], "\n": ["ret"], "-": ["minus"], "/": ["slash"], ".": ["dot"],
          ";": ["semicolon"],
          ">": ["shift", "dot"], "$": ["shift", "4"], "(": ["shift", "9"],
          ")": ["shift", "0"], "|": ["shift", "backslash"], "_": ["shift", "minus"]}


def qcodes(c: str):
    if c in QCODES:
        return QCODES[c]
    if c.isupper():
        return ["shift", c.lower()]
    return [c]


def wait_for_mark(serial: Path, mark: bytes, wait_s: float):
    """Seconds until `mark` is on the serial port, or None after `wait_s`."""
    start = time.monotonic()
    while time.monotonic() - start < wait_s:
        if serial.exists() and mark in serial.read_bytes():
            return time.monotonic() - start
        time.sleep(0.05)
    return None


def type_through_qmp(sock_path: Path, serial: Path, steps, wait_s: float):
    """Waits for the guest's shell (NANOX_SHELL_READY on the serial port), then types
    each step's line on QEMU's own keyboard through QMP and waits for its mark; returns
    the seconds from each Enter to its mark (None: no mark, or it never came), or None
    if the shell never came up."""
    import socket

    if wait_for_mark(serial, b"NANOX_SHELL_READY", wait_s) is None:
        return None
    time.sleep(3)  # the shell starts after the marker
    s = socket.socket(socket.AF_UNIX)
    s.connect(str(sock_path))
    f = s.makefile("rw")
    f.readline()  # greeting

    def call(cmd, args=None):
        f.write(json.dumps({"execute": cmd, **({"arguments": args} if args else {})}) + "\n")
        f.flush()
        while True:
            reply = json.loads(f.readline())
            if "return" in reply or "error" in reply:
                return reply

    call("qmp_capabilities")
    seconds = []
    for text, mark, _ in steps:
        for c in text:
            keys = [{"type": "qcode", "data": k} for k in qcodes(c)]
            call("send-key", {"keys": keys, "hold-time": 40})
            time.sleep(0.12)
        t = wait_for_mark(serial, mark.encode(), SHELL_TEST_WAIT_S) if mark else None
        seconds.append(None if t is None else round(t, 2))
    s.close()
    return seconds


def linux_test_disk(out: Path) -> Path:
    """A 4 MiB disk with an MBR and one Linux partition from sector 2048, whose first
    sector carries a marker: Linux reads the table and prints `vda: vda1`."""
    sectors = 8192
    disk = bytearray(sectors * 512)
    entry = bytes([0x00, 0, 0, 0, 0x83, 0, 0, 0])
    entry += (2048).to_bytes(4, "little") + (sectors - 2048).to_bytes(4, "little")
    disk[446:462] = entry
    disk[510:512] = bytes([0x55, 0xAA])
    marker = b"NANOX virtio-blk test disk" + bytes([0x0A])
    disk[2048 * 512:2048 * 512 + len(marker)] = marker
    img = out / "disk.img"
    img.write_bytes(bytes(disk))
    return img


def run_profile(out: Path, name: str, cpu: str, code: Path, vars_src: Path, linux=None, disk=None):
    d = out / name
    esp = d / "esp/EFI/BOOT"
    esp.mkdir(parents=True)
    shutil.copyfile(EFI, esp / "BOOTX64.EFI")
    vars_fd = d / "vars.fd"
    shutil.copyfile(vars_src, vars_fd)
    vars_fd.chmod(0o644)
    serial = d / "serial.log"
    show = SHOW and bool(linux)
    argv = [
        QEMU_DISPLAY if show else "qemu-system-x86_64",
        "-machine", "q35",
        "-accel", "tcg,thread=single",
        "-cpu", cpu,
        "-smp", "1",
        "-m", "1G" if linux else "256M",
        "-display", "gtk" if show else "none",
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
    if linux:
        argv += [
            "-fw_cfg", f"name=opt/nanox/bzimage,file={LINUX_KERNEL}",
            "-fw_cfg", f"name=opt/nanox/initrd,file={linux}",
            # QEMU's option syntax: a comma in a value is doubled.
            "-fw_cfg", f"name=opt/nanox/cmdline,string={LINUX_CMDLINE.replace(',', ',,')}",
        ]
        if LINUX_HOST_TICK:
            argv += ["-fw_cfg", f"name=opt/nanox/host-tick-ns,string={LINUX_HOST_TICK}"]
        if disk:
            argv += ["-fw_cfg", f"name=opt/nanox/disk,file={disk}"]
        if show:
            argv += ["-fw_cfg", "name=opt/nanox/hold,string=1"]
        if INTERACTIVE:
            argv += ["-fw_cfg", "name=opt/nanox/interactive,string=1"]
    qmp = d / "qmp.sock"
    typing = SHELL_TESTING and bool(linux)
    if typing:
        argv += ["-qmp", f"unix:{qmp},server=on,wait=off"]
    (d / "argv.json").write_text(json.dumps(argv, indent=1) + "\n")
    started = time.monotonic()
    # A shown run lasts until its window is closed.
    timeout = None if show else LINUX_TIMEOUT_S if linux else TIMEOUT_S
    typed = None
    try:
        if typing:
            p = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            typed = type_through_qmp(qmp, serial, SHELL_TEST, timeout)
            _, stderr = p.communicate(timeout=timeout)
            status = p.returncode
        else:
            p = subprocess.run(argv, capture_output=True, timeout=timeout)
            status, stderr = p.returncode, p.stderr
    except subprocess.TimeoutExpired as e:
        if typing:
            p.kill()
            _, stderr = p.communicate()
        else:
            stderr = e.stderr or b""
        status = "timeout"
    (d / "stderr.log").write_bytes(stderr)
    text = serial.read_text(errors="replace") if serial.exists() else ""
    lines = text.splitlines()
    r = {
        "cpu": cpu,
        "shown": show,
        "typed": typed,
        "status": status,
        "seconds": round(time.monotonic() - started, 1),
        # The guest's own lines stay in serial.log.
        "serial_lines": [l for l in lines if l.startswith("NANOX:SVM-PROBE")
                         and not l.startswith(("NANOX:SVM-PROBE:LINUX ",
                                               "NANOX:SVM-PROBE:LINUX-PROGRESS",
                                               "NANOX:SVM-PROBE:LINUX-FB ",
                                               "NANOX:SVM-PROBE:NANOX-FB "))],
    }
    if linux:
        guest = [l[len("NANOX:SVM-PROBE:LINUX "):] for l in lines
                 if l.startswith("NANOX:SVM-PROBE:LINUX ")]
        r["linux"] = linux_summary(guest, lines)
        r["linux"]["screen"] = linux_screen(lines, d / "screen.png")
        r["linux"]["nanox_screen"] = linux_screen(lines, d / "nanox-screen.png", "NANOX")
        if typing:
            marks = [m for _, m, _ in SHELL_TEST if m]
            r["linux"]["shell_test"] = {
                "lines": [next((g for g in guest if m in g), None) for m in marks],
                "seconds_to_mark": typed,
                "ok": bool(typed) and all(
                    s is not None and s >= least
                    for s, (_, m, least) in zip(typed, SHELL_TEST) if m),
            }
    return r


FNV_OFFSET = 0xCBF29CE484222325
FNV_PRIME = 0x100000001B3


def linux_screen(lines, png: Path, tag="LINUX"):
    """A screen from the probe's dump (svm-probe linux.rs `dump_screen`; LINUX is the
    guest's, NANOX the probe's display) as a PNG; its pixels are checked against the
    probe's hash."""
    head = [l for l in lines if l.startswith(f"NANOX:SVM-PROBE:{tag}-SCREEN ")]
    if not head:
        return None
    m = re.fullmatch(rf"NANOX:SVM-PROBE:{tag}-SCREEN width=(\d+) height=(\d+)", head[0])
    w, h = int(m[1]), int(m[2])
    rows = []
    for l in lines:
        if not l.startswith(f"NANOX:SVM-PROBE:{tag}-FB "):
            continue
        runs = l.split()[1:]
        if runs == ["="]:
            rows.append(rows[-1])
            continue
        row = b"".join(bytes.fromhex(px) * int(n, 16) for n, px in (r.split(":") for r in runs))
        rows.append(row)
    end = [l for l in lines if l.startswith(f"NANOX:SVM-PROBE:{tag}-SCREEN-END ")]
    pixels = b"".join(rows)
    fnv = FNV_OFFSET
    for b in pixels:
        fnv = ((fnv ^ b) * FNV_PRIME) & 0xFFFFFFFFFFFFFFFF
    complete = len(rows) == h and all(len(r) == 3 * w for r in rows) and bool(end)
    ok = complete and end[0] == f"NANOX:SVM-PROBE:{tag}-SCREEN-END fnv={fnv:016x}"
    if ok:
        png.write_bytes(png_rgb(w, h, rows))
    return {
        "width": w,
        "height": h,
        "rows": len(rows),
        "fnv": f"{fnv:016x}",
        "match": ok,
        "lit_pixels": sum(1 for i in range(0, len(pixels), 3) if pixels[i:i + 3] != bytes(3)),
        "png": str(png) if ok else None,
    }


def png_rgb(w, h, rows):
    """An 8-bit RGB PNG of `rows` (3 * w bytes each)."""
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data)))
    raw = b"".join(b"\0" + r for r in rows)
    return (bytes([0x89]) + b"PNG" + bytes([0x0D, 0x0A, 0x1A, 0x0A])
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def linux_summary(guest, lines):
    """What the Linux guest printed: its report, and how far it got."""
    begin = next((i for i, l in enumerate(guest) if "NANOX_GUEST_REPORT_BEGIN" in l), None)
    end = next((i for i, l in enumerate(guest) if "NANOX_GUEST_REPORT_END" in l), None)
    progress = [l for l in lines if l.startswith("NANOX:SVM-PROBE:LINUX-PROGRESS")]
    return {
        "guest_lines": len(guest),
        "report_complete": begin is not None and end is not None and end > begin,
        "report": guest[begin:end + 1] if begin is not None and end is not None else None,
        "last_guest_lines": guest[-20:],
        "last_progress": progress[-1] if progress else None,
        "case": next((l for l in lines if l.startswith("NANOX:SVM-PROBE:CASE linux ")), None),
        "agent": next((l for l in lines if l.startswith("NANOX:SVM-PROBE:LINUX-AGENT ")), None),
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
        if LINUX_KERNEL and not LINUX_INIT:
            subprocess.run([sys.executable, "tools/native/build.py", "--package",
                            "linux-probe-init"], cwd=ROOT, check=True)
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
    if SHOW:
        if not os.path.exists(QEMU_DISPLAY):
            print(f"--show needs a QEMU with a window at {QEMU_DISPLAY}: "
                  "nix build .#qemu-display --out-link ~/.nix-qemu-display")
            return 2
        summary["qemu_display"] = subprocess.run(
            [QEMU_DISPLAY, "--version"], capture_output=True, text=True).stdout.splitlines()[0]
    profiles = [] if "--linux-only" in sys.argv else list(PROFILES)
    initrd = None
    disk = None
    if LINUX_KERNEL:
        initrd = linux_initrd(out)
        disk = Path(LINUX_DISK) if LINUX_DISK else linux_test_disk(out)
        init = Path(LINUX_INIT) if LINUX_INIT else LINUX_INIT_DEFAULT
        summary["linux"] = {
            "kernel": LINUX_KERNEL,
            "kernel_sha256": sha256(Path(LINUX_KERNEL)),
            "init": str(init),
            "init_sha256": sha256(init),
            "initrd_bytes": initrd.stat().st_size,
            "initrd_sha256": sha256(initrd),
            "cmdline": LINUX_CMDLINE,
            "host_tick_ns": LINUX_HOST_TICK or "probe default",
            "disk": str(disk),
            "disk_sha256": sha256(disk),
        }
        profiles.append(LINUX_PROFILE)
    ok = True
    for name, cpu, want_status, want_line in profiles:
        r = run_profile(out, name, cpu, code, vars_src,
                        linux=initrd if name == "linux" else None,
                        disk=disk if name == "linux" else None)
        r["expected_status"] = want_status
        r["expected_line"] = want_line
        # Shown, the run ends when the window is closed: only the RESULT line counts.
        status_ok = r["status"] == want_status or r["shown"]
        r["match"] = status_ok and want_line in r["serial_lines"]
        if SHELL_TESTING and "linux" in r:
            r["match"] &= r["linux"]["shell_test"]["ok"]
        ok &= r["match"]
        summary["profiles"][name] = r
        print(f"{name:7} status={r['status']} match={r['match']} ({r['seconds']} s)")
        if "linux" in r:
            print(f"        {r['linux']['case']}")
            print(f"        screen: {r['linux']['screen']}")
            print(f"        nanox screen: {r['linux']['nanox_screen']}")
            if "shell_test" in r["linux"]:
                print(f"        shell test: {r['linux']['shell_test']}")
    if initrd:
        # The archive is rebuilt from the init on every run; its hash is recorded.
        initrd.unlink()
    if "svm" in summary["profiles"]:
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
