#!/usr/bin/env python3
"""Measures what a real Linux kernel asks of the machine it boots on, so the
device model a NANOX VMM must provide to host a Linux guest (docs/specs/
M11-WINDOW.md, section 4) is grounded in a measurement, not a guess.

Boots a kernel with a tiny initramfs (a static init program, built by
tools/native/build.py --package linux-probe-init) under QEMU TCG
on a q35 machine, records every I/O port access, PCI configuration access and
interrupt-controller / timer register access with QEMU trace events, and
prints a summary by device. Also records what the guest itself reports
(/proc/ioports, /proc/iomem, /proc/interrupts, PCI devices) through the
serial console.

    nix develop --command python3 tools/hostguest/linux_surface.py \
        --kernel KERNEL --init INIT.elf [--out DIR]

The kernel is not part of the repository; any x86-64 bzImage works. The run
is a measurement of one kernel on one QEMU machine type, not a minimum.
"""
import collections
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile

PORT_NAMES = [
    (0x0020, 0x0021, "PIC master (8259)"),
    (0x00A0, 0x00A1, "PIC slave (8259)"),
    (0x0040, 0x0043, "PIT (8254)"),
    (0x0060, 0x0064, "i8042 keyboard controller"),
    (0x0061, 0x0061, "port B (speaker, PIT gate, NMI status)"),
    (0x0070, 0x0071, "RTC / CMOS"),
    (0x0080, 0x0080, "POST diagnostic port (delay)"),
    (0x0092, 0x0092, "fast A20 gate"),
    (0x00B2, 0x00B3, "APM / SMI control"),
    (0x00F0, 0x00FF, "x87 FPU error / legacy"),
    (0x03F8, 0x03FF, "COM1 UART (16550)"),
    (0x02F8, 0x02FF, "COM2 UART"),
    (0x03E8, 0x03EF, "COM3 UART"),
    (0x02E8, 0x02EF, "COM4 UART"),
    (0x03C0, 0x03DF, "VGA registers"),
    (0x0CF8, 0x0CFF, "PCI configuration (CF8/CFC)"),
    (0x0402, 0x0402, "debug console"),
    (0x0510, 0x0511, "fw_cfg"),
    (0x0600, 0x063F, "ACPI PM / power (q35 ICH9)"),
    (0x0700, 0x073F, "ICH9 SMBus / misc"),
    (0xB000, 0xB0FF, "ACPI PM (legacy PIIX)"),
    (0x0170, 0x0177, "IDE secondary"),
    (0x01F0, 0x01F7, "IDE primary"),
    (0x0376, 0x0376, "IDE secondary control"),
    (0x03F6, 0x03F6, "IDE primary control"),
    (0x0CD6, 0x0CD7, "chipset (PM index/data)"),
    (0x0CF9, 0x0CF9, "reset control"),
    (0xC000, 0xFFFF, "PCI device I/O BARs"),
]


def port_name(p):
    for lo, hi, name in PORT_NAMES:
        if lo <= p <= hi:
            return name
    return "other"


def cpio_newc(files):
    """files: list of (path, mode, data bytes). Returns an initramfs image."""
    out = bytearray()
    ino = 1

    def entry(name, mode, data, nlink=1):
        nonlocal ino
        nb = name.encode() + b"\0"
        hdr = "070701" + "".join("%08X" % v for v in (
            ino, mode, 0, 0, nlink, 0, len(data), 0, 0, 0, 0, len(nb), 0))
        ino += 1
        out.extend(hdr.encode())
        out.extend(nb)
        out.extend(b"\0" * (-(110 + len(nb)) % 4))
        out.extend(data)
        out.extend(b"\0" * (-len(data) % 4))

    for path, mode, data in files:
        entry(path, mode, data)
    entry("TRAILER!!!", 0, b"", 0)
    return bytes(out)


def run(kernel, init, out, memory="512", seconds=240):
    tmp = tempfile.mkdtemp(prefix="nanox-linux-surface-")
    img = cpio_newc([("init", 0o100755, open(init, "rb").read())])
    initrd = os.path.join(tmp, "initrd.img")
    open(initrd, "wb").write(img)
    log = os.path.join(tmp, "qemu.log")
    events = ["memory_region_ops_read", "memory_region_ops_write", "pci_cfg_read", "pci_cfg_write", "apic_register_read",
              "apic_register_write", "ioapic_mem_read", "ioapic_mem_write", "hpet_ram_read",
              "hpet_ram_write"]
    cmd = ["qemu-system-x86_64", "-M", "q35", "-accel", "tcg", "-cpu", "max", "-m", memory,
           "-smp", "1", "-display", "none", "-no-reboot", "-serial", "stdio",
           "-kernel", kernel, "-initrd", initrd,
           "-append", "console=ttyS0 panic=-1 quiet loglevel=3 nokaslr"]
    for e in events:
        cmd += ["-trace", e]
    cmd += ["-D", log]
    print("+", " ".join(cmd[:14]), "...")
    try:
        p = subprocess.run(cmd, capture_output=True, timeout=seconds)
        serial = p.stdout.decode("latin-1")
        status = p.returncode
    except subprocess.TimeoutExpired as e:
        serial = (e.stdout or b"").decode("latin-1")
        status = "timeout"
    return tmp, log, serial, status


REGION = re.compile(r"memory_region_ops_(read|write) cpu (-?\d+) mr \S+ addr (0x[0-9a-f]+) value (0x[0-9a-f]+) size (\d+) name .(.*).$")
PCI = re.compile(r"pci_cfg_(read|write) (\S+) (\S+) @(0x[0-9a-f]+)")


def summarize(lines, serial):
    regions = collections.OrderedDict()
    pci = collections.OrderedDict()
    counts = collections.Counter()
    for l in lines:
        m = REGION.match(l)
        if m:
            kind, _cpu, addr, _val, _size, name = m.groups()
            r = regions.setdefault(name, {"read": 0, "write": 0, "addrs": set()})
            r[kind] += 1
            r["addrs"].add(int(addr, 16))
            continue
        m = PCI.match(l)
        if m:
            kind, dev, bdf, off = m.groups()
            d = pci.setdefault((bdf, dev), {"read": 0, "write": 0, "offsets": set()})
            d[kind] += 1
            d["offsets"].add(int(off, 16))
            continue
        counts[l.split()[0] if l.split() else ""] += 1
    out = ["### Device regions touched (QEMU memory_region_ops: I/O ports and MMIO)", "",
           "| region | reads | writes | distinct addresses | lowest | highest |",
           "|---|---|---|---|---|---|"]
    for name, r in sorted(regions.items(), key=lambda kv: -(kv[1]["read"] + kv[1]["write"])):
        a = sorted(r["addrs"])
        out.append("| %s | %d | %d | %d | %#x | %#x |" % (name, r["read"], r["write"], len(a), a[0], a[-1]))
    out += ["", "### PCI configuration space accessed", "",
            "| device | address | reads | writes | distinct offsets |", "|---|---|---|---|---|"]
    empty = [k for k in pci if k[1] == "empty"]
    for (bdf, dev), d in pci.items():
        if dev != "empty":
            out.append("| %s | %s | %d | %d | %d |" % (dev, bdf, d["read"], d["write"], len(d["offsets"])))
    out.append("| (absent slots probed) | %d addresses | %d | 0 | - |" % (
        len(empty), sum(pci[k]["read"] for k in empty)))
    out += ["", "### Interrupt controller and timer accesses", "", "| event | count |", "|---|---|"]
    for k, v in sorted(counts.items(), key=lambda kv: -kv[1]):
        if k:
            out.append("| %s | %d |" % (k, v))
    begin = serial.find("NANOX_GUEST_REPORT_BEGIN")
    end = serial.find("NANOX_GUEST_REPORT_END")
    if begin >= 0 and end > begin:
        out += ["", "### What the guest reports about its machine", "", "```",
                serial[begin + len("NANOX_GUEST_REPORT_BEGIN"):end].strip(), "```"]
    return chr(10).join(out)


def main():
    args = sys.argv[1:]
    get = lambda name: args[args.index(name) + 1] if name in args else None
    kernel, init = get("--kernel"), get("--init")
    if not kernel or not init:
        print(__doc__)
        return 2
    tmp, log, serial, status = run(kernel, init, get("--out"))
    print("qemu exit:", status)
    lines = open(log, errors="replace").read().splitlines() if os.path.exists(log) else []
    ok = "NANOX_GUEST_REPORT_END" in serial
    head = "%s: qemu exit %s; guest report complete: %s; %d trace lines" % (
        os.path.basename(kernel), status, ok, len(lines))
    text = head + chr(10) + chr(10) + summarize(lines, serial) + chr(10)
    out = get("--out")
    if out:
        open(out, "w").write(text)
        print("wrote", out)
    else:
        print(text)
    shutil.rmtree(tmp, ignore_errors=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
