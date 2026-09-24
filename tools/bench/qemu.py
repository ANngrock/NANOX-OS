#!/usr/bin/env python3
"""Canonical QEMU/UEFI configuration of the NANOX M0 bench.

This module is the single source of truth for the QEMU command line; the
harness, `make run`, `make debug` and `make debug-check` all build their argv
here.  docs/m0-bench.md documents the result and the reasons for each option.

Usage:
  qemu.py print [--image PATH]   print the canonical command line
  qemu.py debug [--image PATH]   run with the GDB stub on :1234, CPU halted (-s -S)
"""

import argparse
import os
import shlex
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "out"

QEMU_BINARY = os.environ.get("NANOX_QEMU", "qemu-system-x86_64")

# Firmware lookup order: explicit environment, then distribution paths.
OVMF_CODE_CANDIDATES = [
    "/usr/share/OVMF/OVMF_CODE_4M.fd",  # Debian/Ubuntu (ovmf 2024.02)
]
OVMF_VARS_CANDIDATES = [
    "/usr/share/OVMF/OVMF_VARS_4M.fd",
]

DEBUG_EXIT_IOBASE = 0xF4
# Guest wall clock is fixed so runs do not depend on the host date.
RTC_BASE = "2026-01-01T00:00:00"


def _pick(env_name, candidates):
    value = os.environ.get(env_name)
    if value:
        return value
    for c in candidates:
        if os.path.exists(c):
            return c
    return candidates[0]


def ovmf_code_path():
    return _pick("NANOX_OVMF_CODE", OVMF_CODE_CANDIDATES)


def ovmf_vars_template_path():
    return _pick("NANOX_OVMF_VARS", OVMF_VARS_CANDIDATES)


def prepare_vars(dest):
    """Copies the pristine NVRAM template so every run starts from the same state."""
    dest = Path(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(ovmf_vars_template_path(), dest)
    return dest


# M4: the persistent data disk, a second virtio-blk device found by the
# kernel through its serial number (docs/m4-store.md).
DATA_DISK_SERIAL = "nanox-data"


def base_argv(image, vars_path, serial, extra=(), bridge_socket=None, data_disk=None):
    """Returns the canonical argv.

    image     raw disk image (opened read-only)
    vars_path writable copy of the OVMF NVRAM template
    serial    QEMU -serial backend, e.g. "file:out/runs/x/serial.log" or "stdio"
    extra     additional arguments appended at the end (scenario specific)
    bridge_socket
              M3 host bridge: the second serial port (COM2) is connected, as a
              client, to this unix socket, on which the host bridge listens
              (docs/m3-core.md)
    data_disk M4: raw image of the persistent data disk, attached writable as
              a second virtio-blk device with serial DATA_DISK_SERIAL
    """
    argv = [
        QEMU_BINARY,
        "-no-user-config",
        "-nodefaults",
        "-machine", "pc-q35-8.2",
        "-accel", "tcg",
        "-cpu", "qemu64",
        "-smp", "1",
        "-m", "256M",
        "-rtc", "base=%s,clock=vm" % RTC_BASE,
        "-display", "none",
        "-monitor", "none",
        "-no-reboot",
        "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=%s" % ovmf_code_path(),
        "-drive", "if=pflash,format=raw,unit=1,file=%s" % vars_path,
        "-drive", "if=none,id=nxdisk,format=raw,readonly=on,file=%s" % image,
        "-device", "virtio-blk-pci,drive=nxdisk,bootindex=0",
        "-device", "isa-debug-exit,iobase=0x%x,iosize=0x01" % DEBUG_EXIT_IOBASE,
        "-serial", serial,
    ]
    if data_disk:
        argv += ["-drive", "if=none,id=nxdata,format=raw,file=%s" % data_disk,
                 "-device", "virtio-blk-pci,drive=nxdata,serial=%s" % DATA_DISK_SERIAL]
    if bridge_socket:
        argv += ["-chardev", "socket,id=nxbridge,path=%s,server=off" % bridge_socket,
                 "-serial", "chardev:nxbridge"]
    argv.extend(extra)
    return argv


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("command", choices=["print", "debug"])
    ap.add_argument("--image", default=str(OUT / "nanox.img"))
    args = ap.parse_args(argv)

    if args.command == "print":
        vars_path = OUT / "debug" / "OVMF_VARS.fd"
        print(shlex.join(base_argv(args.image, vars_path, "stdio")))
        return 0

    vars_path = prepare_vars(OUT / "debug" / "OVMF_VARS.fd")
    cmd = base_argv(args.image, vars_path, "stdio", extra=["-s", "-S"])
    sys.stderr.write(
        "NANOX debug: QEMU is halted before the firmware; GDB stub on tcp::1234.\n"
        "In another terminal:\n"
        "  gdb -x tools/gdb/nanox.gdb\n"
        "Serial output follows below.  Stop QEMU with Ctrl-C here or 'kill' in GDB.\n"
        "command: %s\n" % shlex.join(cmd))
    sys.stderr.flush()
    os.execvp(cmd[0], cmd)


if __name__ == "__main__":
    sys.exit(main())
