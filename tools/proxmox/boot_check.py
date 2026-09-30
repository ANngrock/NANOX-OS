#!/usr/bin/env python3
"""Boots the NANOX image the way Proxmox VE would start a UEFI q35 guest
(docs/specs/M11-SERVER.md §4, tools/proxmox/pve_qemu.py) and records what
happens. Run inside `nix develop` (pinned QEMU and OVMF):

    python3 tools/proxmox/boot_check.py [--kernel-run DIR] [--image PATH]

Checks:
  boot     test-profile image (a `cargo xtask test` pass run) in the "min" and
           "full" device sets with 1 and 2 vCPUs: must end with the harness
           verdict PASS (exit status 33) and the kernel's PASS line;
  agent    in the "full" set the guest-agent channel is a unix socket (it
           must accept a connection: the virtio-serial port exists) - the
           NANOX side of that channel is not implemented, so nothing answers;
  acpi     the image without its test BOOT.CFG (normal profile) is booted, `system_powerdown` (what
           `qm shutdown` sends) is delivered over QMP, and the result is
           recorded. Today the kernel has no ACPI driver, so the guest is
           expected to keep running: that is a recorded NEGATIVE result, not
           a pass; `--expect-acpi-shutdown` turns it into a requirement for
           when the driver exists.

Records: out/proxmox-<utc>/ with the argv, serial output and verdicts per
case and summary.json (input hashes, QEMU version). Exit status 0 when every
boot case passed and the acpi result is what was expected.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pve_qemu as pq  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]


def latest_run(kind):
    runs = sorted((ROOT / "out/runs").glob(f"*-{kind}-boot-test"), key=lambda p: p.stat().st_mtime)
    if not runs:
        sys.exit(f"no {kind} boot-test run under out/runs (run `cargo xtask test` first)")
    return runs[-1]


def boot_case(out, run_dir, profile, cores):
    name = f"boot-{profile}-{cores}cpu"
    d = out / name
    d.mkdir(parents=True)
    disk = d / "disk.img"
    shutil.copyfile(run_dir / "initial.img", disk)
    vars_fd = d / "vars.fd"
    shutil.copyfile(run_dir / "initial-vars.fd", vars_fd)
    vars_fd.chmod(0o644)
    serial = d / "serial.log"
    qga = d / "qga.sock"
    argv = pq.pve_argv(
        disk=disk, code_fd=run_dir / "initial-code.fd", vars_fd=vars_fd,
        serial_log=serial, qga_sock=qga if profile == "full" else None,
        cores=cores, profile=profile,
    )
    (d / "argv.json").write_text(json.dumps(argv, indent=1) + "\n")
    started = time.monotonic()
    p = pq.spawn(argv, stderr_path=d / "stderr.log")
    agent_socket = None
    try:
        if profile == "full":
            # The channel's socket appears as soon as QEMU has set up the device.
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not qga.exists():
                time.sleep(0.1)
            if qga.exists():
                import socket
                try:
                    c = socket.socket(socket.AF_UNIX)
                    c.settimeout(2)
                    c.connect(str(qga))
                    c.close()
                    agent_socket = "connect-ok"
                except OSError as e:
                    agent_socket = f"connect-failed: {e}"
            else:
                agent_socket = "no-socket"
        try:
            status = p.wait(timeout=90)
        except subprocess.TimeoutExpired:
            p.kill()
            status = "timeout"
    finally:
        if p.poll() is None:
            p.kill()
    text = serial.read_bytes().decode(errors="replace") if serial.exists() else ""
    ok = status == 33 and "NANOX:TEST:PASS" in text
    rec = {
        "case": name, "status": status, "pass": ok,
        "seconds": round(time.monotonic() - started, 1),
        "agent_channel": agent_socket,
        "kernel_lines": [l for l in text.splitlines() if l.startswith("NANOX:")],
    }
    print(f"{name:20} status={status} pass={ok} agent={agent_socket}")
    return rec


def acpi_case(out, image, code_fd, vars_src, expect):
    d = out / "acpi-shutdown"
    d.mkdir(parents=True)
    disk = d / "disk.img"
    shutil.copyfile(image, disk)
    # Every xtask image carries a test-profile BOOT.CFG; without it the
    # loader boots the normal profile (the kernel idles instead of exiting).
    subprocess.run(["mdel", "-i", f"{disk}@@1M", "::/NANOX/BOOT.CFG"], check=True)
    vars_fd = d / "vars.fd"
    shutil.copyfile(vars_src, vars_fd)
    vars_fd.chmod(0o644)
    serial = d / "serial.log"
    qmp = d / "qmp.sock"
    argv = pq.pve_argv(
        disk=disk, code_fd=code_fd, vars_fd=vars_fd, serial_log=serial,
        qmp_sock=qmp, cores=1, profile="full", debug_exit=False, no_shutdown=True,
    )
    (d / "argv.json").write_text(json.dumps(argv, indent=1) + "\n")
    p = pq.spawn(argv, stderr_path=d / "stderr.log")
    result = {"case": "acpi-shutdown"}
    try:
        q = pq.Qmp(str(qmp), timeout=15, proc=p)
        booted = pq.wait_for_file_text(serial, "NANOX:KERNEL:IDLE", 40)
        result["kernel_reached_idle"] = booted
        before = pq.status_of(q)
        q.cmd("system_powerdown")
        try:
            p.wait(timeout=15)
            exited = True
        except subprocess.TimeoutExpired:
            exited = False
        result["status_before"] = before
        after = None if exited else pq.status_of(q)
        result["status_after"] = after
        result["exited_after_powerdown"] = exited
        result["shutdown_state_after_powerdown"] = after == "shutdown"
        q.close()
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
    text = serial.read_bytes().decode(errors="replace") if serial.exists() else ""
    result["kernel_lines"] = [l for l in text.splitlines() if l.startswith("NANOX:")][:12]
    # With -no-shutdown a handled ACPI power-off ends in run state "shutdown".
    handled = result.get("exited_after_powerdown", False) or result.get("shutdown_state_after_powerdown", False)
    result["shutdown_handled"] = handled
    result["as_expected"] = handled if expect else (result.get("kernel_reached_idle") and not handled)
    print(f"acpi-shutdown        handled={handled} (expected: {'handled' if expect else 'not yet handled'})")
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel-run", help="an `xtask test` pass run directory (default: latest)")
    ap.add_argument("--image", default=None, help="disk image for the acpi case (default: the run's initial.img)")
    ap.add_argument("--expect-acpi-shutdown", action="store_true")
    ap.add_argument("--skip-acpi", action="store_true")
    args = ap.parse_args()

    run_dir = Path(args.kernel_run) if args.kernel_run else latest_run("pass")
    out = ROOT / "out" / time.strftime("proxmox-%Y%m%dT%H%M%SZ", time.gmtime())
    out.mkdir(parents=True)
    qemu = subprocess.run(["qemu-system-x86_64", "--version"], capture_output=True, text=True).stdout.splitlines()[0]
    summary = {
        "qemu": qemu,
        "test_image_sha256": pq.sha256(run_dir / "initial.img"),
        "ovmf_code_sha256": pq.sha256(run_dir / "initial-code.fd"),
        "run": run_dir.name,
        "cases": [],
    }
    ok = True
    for profile in pq.PROFILES:
        for cores in (1, 2):
            r = boot_case(out, run_dir, profile, cores)
            summary["cases"].append(r)
            ok &= r["pass"]
    if not args.skip_acpi:
        img = Path(args.image) if args.image else run_dir / "initial.img"
        summary["acpi_image_sha256"] = pq.sha256(img)
        r = acpi_case(out, img, run_dir / "initial-code.fd", run_dir / "initial-vars.fd", args.expect_acpi_shutdown)
        summary["cases"].append(r)
        ok &= bool(r["as_expected"])
    summary["ok"] = ok
    (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    print("records:", out)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
