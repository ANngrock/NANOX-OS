#!/usr/bin/env python3
"""Automated check of the GDB procedure (make debug-check).

Starts the canonical QEMU configuration with the CPU halted and a GDB stub on a
free local TCP port, runs GDB in batch mode with a hardware breakpoint on
kernel_main, and verifies that GDB stops there with the boot info magic in
RDI.  Writes out/runs/<time>-gdb/record.json with the GDB transcript.
"""

import json
import re
import shlex
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import harness  # noqa: E402
import qemu  # noqa: E402

MAGIC = "0x49425f584f4e414e"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    harness.require_artifacts()
    run_dir = harness.new_run_dir(harness.OUT / "runs", "gdb")
    port = free_port()
    vars_path = qemu.prepare_vars(run_dir / "OVMF_VARS.fd")
    image = harness.OUT / "nanox.img"
    argv = qemu.base_argv(image, vars_path, "file:%s" % (run_dir / "serial.log"),
                          extra=["-gdb", "tcp:127.0.0.1:%d" % port, "-S"])
    gdb_argv = ["gdb", "-batch", "-nx",
                "-ex", "set pagination off", "-ex", "set confirm off",
                "-ex", "set tcp connect-timeout 30",
                "-ex", "file %s" % harness.KERNEL_ELF,
                "-ex", "target remote 127.0.0.1:%d" % port,
                "-ex", "hbreak kernel_main",
                "-ex", "continue",
                "-ex", "info registers rip rdi rsp",
                "-ex", "print/x ((struct nx_boot_info *)$rdi)->magic",
                "-ex", "print ((struct nx_boot_info *)$rdi)->version_major",
                "-ex", "backtrace 1",
                "-ex", "kill"]
    qproc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL)
    t0 = time.monotonic()
    try:
        g = subprocess.run(gdb_argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           timeout=120)
        transcript = g.stdout.decode("utf-8", "replace")
        gdb_status = g.returncode
    except subprocess.TimeoutExpired as e:
        transcript = (e.stdout or b"").decode("utf-8", "replace") + "\n<gdb timeout>"
        gdb_status = None
    finally:
        try:
            qproc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            qproc.kill()
            qproc.wait()
    vars_path.unlink()
    (run_dir / "gdb.log").write_text(transcript)

    checks = {
        "stopped_at_kernel_main": bool(re.search(r"Breakpoint 1, (0x[0-9a-f]+ in )?kernel_main",
                                                 transcript)),
        "rip_at_kernel_main": bool(re.search(r"^rip\s+0x[0-9a-f]+\s+0x[0-9a-f]+ <kernel_main(\+\d+)?>",
                                             transcript, re.M)),
        "bootinfo_magic_in_rdi": MAGIC in transcript,
    }
    ok = all(checks.values())
    record = {
        "schema": "nanox.gdb-check.v1",
        "source": harness.git_source(),
        "qemu": {"argv": argv, "command_line": shlex.join(argv)},
        "gdb": {"argv": gdb_argv, "exit_status": gdb_status, "transcript": transcript},
        "artifacts": {"image": harness.artifact(image),
                      "kernel_elf": harness.artifact(harness.KERNEL_ELF)},
        "duration_s": round(time.monotonic() - t0, 3),
        "checks": checks,
        "verdict": "PASS" if ok else "FAIL",
    }
    (run_dir / "record.json").write_text(json.dumps(record, indent=2) + "\n")
    for k, v in checks.items():
        print("%-4s %s" % ("ok" if v else "FAIL", k))
    print("gdb-check: %s (%s)" % (record["verdict"], run_dir / "record.json"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
