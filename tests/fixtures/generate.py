#!/usr/bin/env python3
"""Generate tiny independent golden fixtures. --check never writes files."""
from pathlib import Path
import struct
import sys

ROOT = Path(__file__).resolve().parent
BASE = 0xFFFFFFFF80000000
ARENA = 0xFFFFFFFF90000000


def executable():
    data = bytearray(4100)
    struct.pack_into("16sHHIQQQIHHHHHH", data, 0,
                     b"\x7fELF\x02\x01\x01" + bytes(9),
                     2, 62, 1, BASE, 64, 0, 0, 64, 56, 1, 0, 0, 0)
    struct.pack_into("IIQQQQQQ", data, 64, 1, 5, 4096, BASE,
                     0xDEADBEEF, 4, 8192, 4096)
    data[4096:] = b"\xfa\xf4\xeb\xfd"
    return bytes(data)


def boot_info():
    # Every field is packed individually at the documented offset, independently
    # of the Rust struct representation under test. No implicit Python padding.
    result = bytearray(160)
    fields = [
        (0, "8s", b"NXBOOT01"), (8, "H", 1), (10, "H", 0),
        (12, "I", 160), (16, "Q", 160), (24, "Q", 3),
        (32, "Q", 0x104000), (40, "Q", ARENA + 16384), (48, "Q", 48),
        (56, "I", 48), (60, "I", 1), (64, "Q", 0x101000),
        (72, "Q", ARENA + 4096), (80, "I", 5), (84, "I", 24),
        (88, "Q", 0), (96, "Q", BASE), (104, "Q", 0x102000),
        (112, "Q", ARENA + 8192), (120, "I", 1), (124, "I", 32),
        (128, "Q", 0x200000), (136, "Q", 0xFFFFFFFF92000000),
        (144, "H", 0x3F8), (146, "6s", bytes(6)), (152, "Q", 42),
    ]
    for offset, fmt, value in fields:
        struct.pack_into("<" + fmt, result, offset, value)
    return bytes(result)


valid = executable()
overlap = bytearray(valid)
struct.pack_into("<H", overlap, 56, 2)
overlap[120:176] = overlap[64:120]
wx = bytearray(valid)
struct.pack_into("<I", wx, 68, 7)
overflow = bytearray(valid)
struct.pack_into("<Q", overflow, 72, 0xFFFFFFFFFFFFFFFC)
fixtures = {
    "valid-minimal.elf": valid,
    "truncated-header.elf": valid[:32],
    "overlapping-loads.elf": bytes(overlap),
    "writable-executable.elf": bytes(wx),
    "overflow-offset.elf": bytes(overflow),
    "boot-info-v1.bin": boot_info(),
}
for name, data in fixtures.items():
    path = ROOT / name
    if "--check" in sys.argv:
        if path.read_bytes() != data:
            raise SystemExit(f"fixture differs: {name}")
    else:
        path.write_bytes(data)
print(f"{'Checked' if '--check' in sys.argv else 'Generated'} {len(fixtures)} fixtures")
