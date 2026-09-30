#!/usr/bin/env python3
"""QEMU command lines shaped like the ones Proxmox VE (qemu-server) starts,
for checking NANOX as a Proxmox guest without a Proxmox host
(docs/specs/M11-SERVER.md §4).

The lines follow `qm showcmd` for a UEFI q35 VM: OVMF as two pflash units,
the boot disk on `virtio-scsi-pci` (`virtio-scsi-single` is a controller per
disk), the guest agent channel `org.qemu.guest_agent.0` on virtio-serial,
`virtio-net`, `virtio-balloon`, `virtio-rng`, the `i6300esb` watchdog and a
VGA display. What differs on purpose, because this runs without KVM, TAP
networking or a management daemon: TCG instead of KVM, a hub-port network
backend (the pinned QEMU has neither TAP nor slirp), no -daemonize, the
display off, and the verdict device of the M0 harness (isa-debug-exit)
plus a serial log file.
"""

import json
import os
import socket
import subprocess
import time
from pathlib import Path

PROFILES = ("min", "full")


def pve_argv(
    *,
    name="nanox",
    disk,
    code_fd,
    vars_fd,
    serial_log,
    qmp_sock=None,
    qga_sock=None,
    cores=1,
    ram_mib=512,
    profile="full",
    cpu="qemu64,+nx",
    debug_exit=True,
    machine="q35",
    no_shutdown=False,
):
    """The argv (without the program name) for one VM."""
    if profile not in PROFILES:
        raise ValueError(f"profile must be one of {PROFILES}")
    a = [
        "-name", f"{name},debug-threads=on",
        "-machine", machine,
        "-accel", "tcg,thread=single",
        "-cpu", cpu,
        "-smp", f"{cores},sockets=1,cores={cores},maxcpus={cores}",
        "-m", str(ram_mib),
        "-nodefaults",
        "-display", "none",
        "-monitor", "none",
        # UEFI: read-only code, writable variable store.
        "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={code_fd}",
        "-drive", f"if=pflash,unit=1,id=drive-efidisk0,format=raw,file={vars_fd}",
        # Boot disk: one virtio-scsi controller per disk (virtio-scsi-single).
        "-device", "virtio-scsi-pci,id=virtioscsi0,bus=pcie.0",
        "-drive", f"file={disk},if=none,id=drive-scsi0,format=raw,cache=none,detect-zeroes=on",
        "-device", "scsi-hd,bus=virtioscsi0.0,channel=0,scsi-id=0,lun=0,drive=drive-scsi0,id=scsi0,bootindex=100",
        "-serial", f"file:{serial_log}",
    ]
    if no_shutdown:
        # Proxmox always passes this: a guest shutdown leaves the VM paused
        # for the management daemon instead of ending QEMU. With it the
        # pinned QEMU's debug-exit device does not end the process either,
        # so the boot verdict runs without it.
        a.append("-no-shutdown")
    if debug_exit:
        a += ["-device", "isa-debug-exit,iobase=0xf4,iosize=4"]
    if qmp_sock:
        a += ["-qmp", f"unix:{qmp_sock},server=on,wait=off"]
    if profile == "full":
        a += [
            "-device", "VGA,id=vga,bus=pcie.0",
            "-device", "virtio-balloon-pci,id=balloon0,deflate-on-oom=on",
            "-object", "rng-random,filename=/dev/urandom,id=rng0",
            "-device", "virtio-rng-pci,rng=rng0,max-bytes=1024,period=1000",
            "-netdev", "hubport,id=net0,hubid=0",
            "-device", "virtio-net-pci,netdev=net0,id=net0,bootindex=102",
            "-device", "i6300esb,id=watchdog0",
            "-watchdog-action", "reset",
        ]
        if qga_sock:
            a += [
                "-chardev", f"socket,path={qga_sock},server=on,wait=off,id=qga0",
                "-device", "virtio-serial,id=qga0-serial",
                "-device", "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0",
            ]
    return a


class Qmp:
    """The QMP monitor, as Proxmox uses it to shut a guest down."""

    def __init__(self, path, timeout=10, proc=None):
        deadline = time.monotonic() + timeout
        while True:
            if proc is not None and proc.poll() is not None:
                raise RuntimeError(
                    f"QEMU exited with status {proc.returncode} before the monitor came up"
                )
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.connect(path)
                break
            except OSError:
                self.s.close()
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)
        self.s.settimeout(timeout)
        self.f = self.s.makefile("rw")
        json.loads(self.f.readline())  # greeting
        self.cmd("qmp_capabilities")

    def cmd(self, name, **args):
        self.f.write(json.dumps({"execute": name, "arguments": args}) + "\n")
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if "return" in r or "error" in r:
                return r

    def close(self):
        self.s.close()


def run(argv, *, qemu="qemu-system-x86_64", timeout=90):
    """Runs the VM to its end (the debug-exit device or the timeout)."""
    p = subprocess.run([qemu, *argv], capture_output=True, timeout=timeout)
    return p.returncode, p.stderr.decode(errors="replace")


def spawn(argv, *, qemu="qemu-system-x86_64", stderr_path=None):
    err = open(stderr_path, "wb") if stderr_path else subprocess.DEVNULL
    return subprocess.Popen([qemu, *argv], stdout=subprocess.DEVNULL, stderr=err)


def status_of(qmp):
    r = qmp.cmd("query-status")
    return r.get("return", {}).get("status")


def wait_for_file_text(path, needle, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if needle.encode() in Path(path).read_bytes():
                return True
        except OSError:
            pass
        time.sleep(0.2)
    return False


def sha256(path):
    import hashlib
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()
