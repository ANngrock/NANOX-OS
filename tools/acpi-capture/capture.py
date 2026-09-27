#!/usr/bin/env python3
"""Capture the ACPI tables that OVMF publishes for a given QEMU machine.

Host-only M9 tool (docs/specs/M9-HARDWARE.md). QEMU runs with OVMF and no
boot disk; the tool polls guest RAM through QMP `pmemsave` until it finds an
RSDP with valid checksums whose RSDT/XSDT lists an APIC table, then writes
every table reachable from the root table (plus FACS/DSDT from the FADT) as raw bytes and
a manifest with argv, tool and firmware hashes and table addresses.

The capture is evidence for fixtures, not a guest test: NANOX code does not
run. Physical addresses equal dump offsets because the dump starts at 0 and
covers only low RAM (q35 maps the first 2 GiB of RAM contiguously).
"""

import argparse
import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PROFILES = {
    # Same machine, CPU model and RAM as the M0 profile, with 4 vCPUs.
    "q35-smp4": ["-smp", "4"],
    "q35-smp4-intel-iommu": ["-smp", "4", "-device", "intel-iommu"],
    "q35-smp4-amd-iommu": ["-smp", "4", "-device", "amd-iommu"],
}
RAM_MIB = 512


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def checksum_ok(data):
    return sum(data) & 0xFF == 0


class Qmp:
    def __init__(self, path, deadline):
        while True:
            try:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.connect(path)
                break
            except OSError:
                self.sock.close()
                if time.monotonic() > deadline:
                    raise RuntimeError("QMP socket did not appear")
                time.sleep(0.1)
        self.buf = b""
        self.read()  # greeting
        self.cmd("qmp_capabilities")

    def read(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("QMP connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def cmd(self, name, **args):
        self.sock.sendall(json.dumps({"execute": name, "arguments": args}).encode() + b"\n")
        while True:
            msg = self.read()
            if "event" in msg:
                continue
            if "error" in msg:
                raise RuntimeError(f"QMP {name}: {msg['error']}")
            return msg["return"]


def find_rsdps(mem):
    """Yield (address, bytes) of every RSDP candidate whose checksums hold."""
    pos = mem.find(b"RSD PTR ")
    while pos >= 0:
        # The RSDP is 16-byte aligned (ACPI 6.5 §5.2.5.1).
        if pos % 16 == 0 and pos + 20 <= len(mem) and checksum_ok(mem[pos:pos + 20]):
            revision = mem[pos + 15]
            if revision >= 2 and pos + 36 <= len(mem):
                length = struct.unpack_from("<I", mem, pos + 20)[0]
                if length >= 36 and pos + length <= len(mem) and checksum_ok(mem[pos:pos + length]):
                    yield pos, mem[pos:pos + length]
            else:
                yield pos, mem[pos:pos + 20]
        pos = mem.find(b"RSD PTR ", pos + 1)


def table_at(mem, addr):
    if addr + 36 > len(mem):
        raise ValueError(f"table at {addr:#x} outside dump")
    length = struct.unpack_from("<I", mem, addr + 4)[0]
    if length < 36 or addr + length > len(mem):
        raise ValueError(f"table at {addr:#x} has bad length {length}")
    data = mem[addr:addr + length]
    if not checksum_ok(data):
        raise ValueError(f"table at {addr:#x} has bad checksum")
    return data


def extract(mem):
    """Return the tables of the first RSDP whose whole XSDT chain validates.

    RAM also holds stale or unpatched copies (the fw_cfg ACPI blobs, an ACPI
    1.0 RSDP); those fail validation and are skipped.
    """
    for rsdp_addr, rsdp in find_rsdps(mem):
        try:
            tables = extract_from(mem, rsdp_addr, rsdp)
        except ValueError:
            continue
        if tables is not None:
            return tables
    return None


def extract_from(mem, rsdp_addr, rsdp):
    # Revision 0 (ACPI 1.0) has only a 32-bit RSDT; QEMU's q35 publishes that.
    if rsdp[15] >= 2:
        root_addr, root_sig, entry = struct.unpack_from("<Q", rsdp, 24)[0], b"XSDT", 8
    else:
        root_addr, root_sig, entry = struct.unpack_from("<I", rsdp, 16)[0], b"RSDT", 4
    root = table_at(mem, root_addr)
    if root[:4] != root_sig:
        raise ValueError("root table signature mismatch")
    tables = [("RSDP", rsdp_addr, rsdp), (root_sig.decode(), root_addr, root)]
    for i in range(36, len(root) - entry + 1, entry):
        addr = struct.unpack_from("<Q" if entry == 8 else "<I", root, i)[0]
        data = table_at(mem, addr)
        tables.append((data[:4].decode("ascii"), addr, data))
        if data[:4] == b"FACP":
            # FACS has no checksum field of the standard header; take its length.
            x_facs = struct.unpack_from("<Q", data, 132)[0] if len(data) >= 140 else 0
            facs = x_facs or struct.unpack_from("<I", data, 36)[0]
            if facs:
                flen = struct.unpack_from("<I", mem, facs + 4)[0]
                tables.append(("FACS", facs, mem[facs:facs + flen]))
            x_dsdt = struct.unpack_from("<Q", data, 140)[0] if len(data) >= 148 else 0
            dsdt = x_dsdt or struct.unpack_from("<I", data, 40)[0]
            tables.append(("DSDT", dsdt, table_at(mem, dsdt)))
    if not any(sig == "APIC" for sig, _, _ in tables):
        return None  # firmware has not finished publishing
    return tables


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("profile", choices=sorted(PROFILES))
    ap.add_argument("out_dir")
    ap.add_argument("--timeout", type=float, default=180.0)
    args = ap.parse_args()

    code = os.environ["NANOX_OVMF_CODE"]
    vars_template = os.environ["NANOX_OVMF_VARS"]
    machine = os.environ.get("NANOX_QEMU_MACHINE", "pc-q35-9.2")
    qemu = shutil.which("qemu-system-x86_64")
    if qemu is None:
        sys.exit("qemu-system-x86_64 not found; run inside `nix develop`")

    work = tempfile.mkdtemp(prefix="acpi-capture-")
    try:
        vars_copy = os.path.join(work, "VARS.fd")
        shutil.copyfile(vars_template, vars_copy)
        os.chmod(vars_copy, 0o600)
        qmp_path = os.path.join(work, "qmp.sock")
        dump = os.path.join(work, "mem.bin")
        argv = [
            qemu, "-no-user-config", "-nodefaults", "-machine", f"{machine},accel=tcg",
            "-cpu", "qemu64,+nx,-rdrand,-rdseed", "-m", str(RAM_MIB),
            "-rtc", "base=2026-09-22T00:00:00,clock=vm",
            "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
            "-drive", f"if=pflash,format=raw,unit=1,file={vars_copy}",
            "-display", "none", "-serial", "null", "-net", "none",
            "-qmp", f"unix:{qmp_path},server=on,wait=off",
        ] + PROFILES[args.profile]
        proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.monotonic() + args.timeout
        tables = None
        polls = 0
        try:
            qmp = Qmp(qmp_path, deadline)
            while tables is None:
                if time.monotonic() > deadline:
                    raise RuntimeError("timeout: firmware did not publish ACPI tables")
                time.sleep(1.0)
                qmp.cmd("stop")
                qmp.cmd("pmemsave", val=0, size=RAM_MIB << 20, filename=dump)
                qmp.cmd("cont")
                polls += 1
                with open(dump, "rb") as f:
                    tables = extract(f.read())
            qmp.cmd("quit")
        finally:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        stderr = proc.stderr.read().decode(errors="replace")

        os.makedirs(args.out_dir, exist_ok=True)
        counts = {}
        entries = []
        for sig, addr, data in tables:
            n = counts.get(sig, 0)
            counts[sig] = n + 1
            name = f"{sig}.bin" if n == 0 else f"{sig}{n}.bin"
            with open(os.path.join(args.out_dir, name), "wb") as f:
                f.write(data)
            entries.append({"signature": sig, "file": name, "phys": f"{addr:#x}",
                            "length": len(data), "sha256": hashlib.sha256(data).hexdigest()})
        manifest = {
            "schema_version": 1,
            "kind": "qemu-ovmf-acpi-capture",
            "profile": args.profile,
            "argv": [a.replace(vars_copy, "<fresh copy of NANOX_OVMF_VARS>") for a in argv],
            "qemu_version": subprocess.run([qemu, "--version"], capture_output=True,
                                           text=True).stdout.splitlines()[0],
            "qemu_sha256": sha256(os.path.realpath(qemu)),
            "firmware_code_sha256": sha256(code),
            "firmware_vars_sha256": sha256(vars_template),
            "polls": polls,
            "qemu_stderr": stderr,
            "tables": entries,
        }
        with open(os.path.join(args.out_dir, "manifest.json"), "w") as f:
            json.dump(manifest, f, indent=2)
            f.write("\n")
        print(f"{args.profile}: {len(entries)} tables -> {args.out_dir}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
