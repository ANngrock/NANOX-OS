#!/usr/bin/env python3
"""Deterministic NANOX boot disk writer (GPT + FAT32 EFI System Partition).

Pure Python standard library; no mtools/mkfs dependency and no host state in
the output: every byte is a function of the input files, the command line and
SOURCE_DATE_EPOCH (FAT timestamps).  Two runs with equal inputs produce
byte-identical images; `make repro-check` verifies that.

Layout (docs/m0-bench.md, "Образ диска"):
  LBA 0            protective MBR
  LBA 1..33        primary GPT header + 128 partition entries
  LBA 2048..       partition 1: EFI System Partition, FAT32, 512 B clusters
  last 33 LBAs     backup GPT entries + header

ESP content:
  \\EFI\\BOOT\\BOOTX64.EFI  loader (removable-media default path, UEFI 2.11 3.5.1)
  \\NANOX\\KERNEL.ELF       kernel
  \\NANOX\\INITRD.IMG       initramfs (cpio newc, tools/image/mkinitrd.py)
  \\NANOX\\MANIFEST.BIN     struct nx_manifest v2 (abi/nanox/manifest.h)
  \\NANOX\\CMDLINE.TXT      kernel command line (may be empty)

Fault-injection switches (used by the harness, never by `make`):
  --omit-kernel / --omit-initrd        leave the file out (manifest still describes it)
  --corrupt-kernel / --corrupt-initrd  flip one byte after hashing it
"""

import argparse
import hashlib
import os
import struct
import sys
import time
import uuid
import zlib

SECTOR = 512
DISK_BYTES = 64 * 1024 * 1024
DISK_SECTORS = DISK_BYTES // SECTOR
PART_FIRST_LBA = 2048
GPT_ENTRIES = 128
GPT_ENTRY_SIZE = 128
GPT_ENTRY_SECTORS = GPT_ENTRIES * GPT_ENTRY_SIZE // SECTOR  # 32
FIRST_USABLE_LBA = 2 + GPT_ENTRY_SECTORS  # 34
LAST_USABLE_LBA = DISK_SECTORS - 2 - GPT_ENTRY_SECTORS  # before backup entries
PART_LAST_LBA = LAST_USABLE_LBA

# Fixed identifiers: deterministic by construction (uuid5 of fixed names).
ESP_TYPE_GUID = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
DISK_GUID = uuid.uuid5(uuid.NAMESPACE_URL, "nanox-os:m0:disk")
PART_GUID = uuid.uuid5(uuid.NAMESPACE_URL, "nanox-os:m0:esp")
VOLUME_ID = 0x4E414E58  # "NANX"
VOLUME_LABEL = b"NANOX ESP  "

FAT32_RESERVED = 32
FAT32_NUM_FATS = 2
FAT32_SEC_PER_CLUS = 1
FAT32_MIN_CLUSTERS = 65525

MANIFEST_MAGIC = 0x464D584E
MANIFEST_VERSION = 2
MANIFEST_SIZE = 128

FAT_EPOCH = 315532800  # 1980-01-01T00:00:00Z, earliest FAT timestamp
ATTR_DIRECTORY = 0x10
ATTR_ARCHIVE = 0x20
ATTR_VOLUME_ID = 0x08


def source_date_epoch():
    """FAT timestamps: SOURCE_DATE_EPOCH if set, otherwise the FAT epoch."""
    value = os.environ.get("SOURCE_DATE_EPOCH")
    if value is None or value == "":
        return FAT_EPOCH
    return max(int(value), FAT_EPOCH)


def fat_datetime(epoch):
    t = time.gmtime(epoch)
    year = min(max(t.tm_year, 1980), 2107)
    date = ((year - 1980) << 9) | (t.tm_mon << 5) | t.tm_mday
    tod = (t.tm_hour << 11) | (t.tm_min << 5) | (t.tm_sec // 2)
    return date, tod


def build_manifest(kernel_bytes, initrd_bytes):
    data = struct.pack("<IHHQ32sQ32s40s", MANIFEST_MAGIC, MANIFEST_VERSION, MANIFEST_SIZE,
                       len(kernel_bytes), hashlib.sha256(kernel_bytes).digest(),
                       len(initrd_bytes), hashlib.sha256(initrd_bytes).digest(), b"\0" * 40)
    assert len(data) == MANIFEST_SIZE
    return data


def _flip_middle(data):
    out = bytearray(data)
    out[len(out) // 2] ^= 0xFF
    return bytes(out)


class Fat32Builder:
    """Builds a FAT32 volume in memory, allocating clusters sequentially."""

    def __init__(self, total_sectors, hidden_sectors, timestamp):
        self.total_sectors = total_sectors
        self.hidden_sectors = hidden_sectors
        self.date, self.time = fat_datetime(timestamp)
        # Microsoft FAT specification, "FAT Type Determination" sizing formula.
        tmp1 = total_sectors - FAT32_RESERVED
        tmp2 = (256 * FAT32_SEC_PER_CLUS + FAT32_NUM_FATS) // 2
        self.fat_sectors = (tmp1 + tmp2 - 1) // tmp2
        self.data_start = FAT32_RESERVED + FAT32_NUM_FATS * self.fat_sectors
        self.cluster_count = (total_sectors - self.data_start) // FAT32_SEC_PER_CLUS
        if self.cluster_count < FAT32_MIN_CLUSTERS:
            raise ValueError("partition too small for FAT32")
        self.cluster_bytes = SECTOR * FAT32_SEC_PER_CLUS
        self.fat = [0x0FFFFFF8, 0x0FFFFFFF]
        self.clusters = {}  # cluster number -> bytes
        self.next_cluster = 2

    def _alloc_chain(self, data):
        """Stores `data` in a fresh contiguous chain; returns first cluster (0 if empty)."""
        if not data:
            return 0
        n = (len(data) + self.cluster_bytes - 1) // self.cluster_bytes
        first = self.next_cluster
        if first + n - 2 > self.cluster_count:
            raise ValueError("volume full")
        for i in range(n):
            c = first + i
            chunk = data[i * self.cluster_bytes:(i + 1) * self.cluster_bytes]
            self.clusters[c] = chunk.ljust(self.cluster_bytes, b"\0")
            self.fat.append(c + 1 if i + 1 < n else 0x0FFFFFFF)
        self.next_cluster = first + n
        return first

    def _entry(self, name83, attr, cluster, size):
        return struct.pack("<11sBBBHHHHHHHI", name83, attr, 0, 0, self.time, self.date,
                           self.date, cluster >> 16, self.time, self.date, cluster & 0xFFFF,
                           size)

    @staticmethod
    def name83(name):
        base, _, ext = name.partition(".")
        if not (1 <= len(base) <= 8 and len(ext) <= 3) or name != name.upper():
            raise ValueError("not an upper-case 8.3 name: %r" % name)
        return base.encode("ascii").ljust(8) + ext.encode("ascii").ljust(3)

    def build_tree(self, tree):
        """tree: dict name -> bytes (file) or dict (directory). Root is cluster 2."""
        root_cluster = self._reserve_dir_clusters(tree, is_root=True)
        self._fill_dir(tree, root_cluster, parent_cluster=0, is_root=True)
        return root_cluster

    def _dir_entry_count(self, tree, is_root):
        return len(tree) + (1 if is_root else 2)  # volume label or "." + ".."

    def _reserve_dir_clusters(self, tree, is_root):
        size = self._dir_entry_count(tree, is_root) * 32
        return self._alloc_chain(b"\0" * size)

    def _fill_dir(self, tree, cluster, parent_cluster, is_root):
        entries = []
        if is_root:
            entries.append(self._entry(VOLUME_LABEL, ATTR_VOLUME_ID, 0, 0))
        else:
            entries.append(self._entry(b".          ", ATTR_DIRECTORY, cluster, 0))
            entries.append(self._entry(b"..         ", ATTR_DIRECTORY, parent_cluster, 0))
        for name in sorted(tree):
            node = tree[name]
            if isinstance(node, dict):
                sub = self._reserve_dir_clusters(node, is_root=False)
                entries.append(self._entry(self.name83(name), ATTR_DIRECTORY, sub, 0))
                # ".." of a first-level directory refers to the root as cluster 0.
                self._fill_dir(node, sub, 0 if is_root else cluster, is_root=False)
            else:
                first = self._alloc_chain(node)
                entries.append(self._entry(self.name83(name), ATTR_ARCHIVE, first, len(node)))
        self._write_chain(cluster, b"".join(entries))

    def _write_chain(self, first, data):
        c, off = first, 0
        while True:
            chunk = data[off:off + self.cluster_bytes]
            self.clusters[c] = chunk.ljust(self.cluster_bytes, b"\0")
            off += self.cluster_bytes
            nxt = self.fat[c]
            if nxt >= 0x0FFFFFF8:
                break
            c = nxt
        assert off >= len(data)

    def boot_sector(self, root_cluster):
        bs = bytearray(SECTOR)
        bs[0:3] = b"\xEB\x58\x90"
        bs[3:11] = b"NANOX   "
        struct.pack_into("<HBHBHHBHHHII", bs, 11, SECTOR, FAT32_SEC_PER_CLUS, FAT32_RESERVED,
                         FAT32_NUM_FATS, 0, 0, 0xF8, 0, 63, 255, self.hidden_sectors,
                         self.total_sectors)
        struct.pack_into("<IHHIHH12sBBBI11s8s", bs, 36, self.fat_sectors, 0, 0, root_cluster,
                         1, 6, b"\0" * 12, 0x80, 0, 0x29, VOLUME_ID, VOLUME_LABEL, b"FAT32   ")
        bs[510:512] = b"\x55\xAA"
        return bytes(bs)

    def fsinfo_sector(self):
        fs = bytearray(SECTOR)
        free = self.cluster_count - (self.next_cluster - 2)
        struct.pack_into("<I", fs, 0, 0x41615252)
        struct.pack_into("<III", fs, 484, 0x61417272, free, self.next_cluster)
        struct.pack_into("<I", fs, 508, 0xAA550000)
        return bytes(fs)

    def write(self, img, part_offset, root_cluster):
        bs = self.boot_sector(root_cluster)
        fsinfo = self.fsinfo_sector()
        for base in (0, 6):  # primary and backup boot sector + FSInfo
            img[part_offset + (base + 0) * SECTOR:part_offset + (base + 1) * SECTOR] = bs
            img[part_offset + (base + 1) * SECTOR:part_offset + (base + 2) * SECTOR] = fsinfo
            # Sector 2 of the boot record: only the trailing signature.
            sig_off = part_offset + (base + 2) * SECTOR + 510
            img[sig_off:sig_off + 2] = b"\x55\xAA"
        fat_bytes = struct.pack("<%dI" % len(self.fat), *self.fat)
        for i in range(FAT32_NUM_FATS):
            off = part_offset + (FAT32_RESERVED + i * self.fat_sectors) * SECTOR
            img[off:off + len(fat_bytes)] = fat_bytes
        for c, data in self.clusters.items():
            off = part_offset + (self.data_start + (c - 2) * FAT32_SEC_PER_CLUS) * SECTOR
            img[off:off + len(data)] = data


def gpt_header(current_lba, backup_lba, entries_lba, entries_crc):
    hdr = bytearray(92)
    struct.pack_into("<8sIIIIQQQQ16sQIII", hdr, 0, b"EFI PART", 0x00010000, 92, 0, 0,
                     current_lba, backup_lba, FIRST_USABLE_LBA, LAST_USABLE_LBA,
                     DISK_GUID.bytes_le, entries_lba, GPT_ENTRIES, GPT_ENTRY_SIZE, entries_crc)
    struct.pack_into("<I", hdr, 16, zlib.crc32(bytes(hdr)) & 0xFFFFFFFF)
    return bytes(hdr).ljust(SECTOR, b"\0")


def write_gpt(img):
    mbr = bytearray(SECTOR)
    # Protective MBR: one partition of type 0xEE covering the disk.
    size = min(DISK_SECTORS - 1, 0xFFFFFFFF)
    struct.pack_into("<B3sB3sII", mbr, 446, 0x00, b"\x00\x02\x00", 0xEE, b"\xFF\xFF\xFF", 1,
                     size)
    mbr[510:512] = b"\x55\xAA"
    img[0:SECTOR] = mbr

    entry = bytearray(GPT_ENTRY_SIZE)
    name = "NANOX ESP".encode("utf-16-le")
    struct.pack_into("<16s16sQQQ72s", entry, 0, ESP_TYPE_GUID.bytes_le, PART_GUID.bytes_le,
                     PART_FIRST_LBA, PART_LAST_LBA, 0, name.ljust(72, b"\0"))
    entries = bytes(entry).ljust(GPT_ENTRIES * GPT_ENTRY_SIZE, b"\0")
    crc = zlib.crc32(entries) & 0xFFFFFFFF

    last_lba = DISK_SECTORS - 1
    backup_entries_lba = last_lba - GPT_ENTRY_SECTORS
    img[SECTOR:2 * SECTOR] = gpt_header(1, last_lba, 2, crc)
    img[2 * SECTOR:2 * SECTOR + len(entries)] = entries
    off = backup_entries_lba * SECTOR
    img[off:off + len(entries)] = entries
    img[last_lba * SECTOR:(last_lba + 1) * SECTOR] = gpt_header(last_lba, 1,
                                                                 backup_entries_lba, crc)


def build_image(loader, kernel, initrd, cmdline="", omit_kernel=False, corrupt_kernel=False,
                omit_initrd=False, corrupt_initrd=False, timestamp=None):
    """Returns the disk image as bytes."""
    if timestamp is None:
        timestamp = source_date_epoch()
    manifest = build_manifest(kernel, initrd)
    nanox = {"MANIFEST.BIN": manifest, "CMDLINE.TXT": cmdline.encode("ascii")}
    if not omit_kernel:
        nanox["KERNEL.ELF"] = _flip_middle(kernel) if corrupt_kernel else kernel
    if not omit_initrd:
        nanox["INITRD.IMG"] = _flip_middle(initrd) if corrupt_initrd else initrd
    tree = {"EFI": {"BOOT": {"BOOTX64.EFI": loader}}, "NANOX": nanox}

    img = bytearray(DISK_BYTES)
    write_gpt(img)
    part_sectors = PART_LAST_LBA - PART_FIRST_LBA + 1
    fat = Fat32Builder(part_sectors, PART_FIRST_LBA, timestamp)
    root = fat.build_tree(tree)
    fat.write(img, PART_FIRST_LBA * SECTOR, root)
    return bytes(img)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--loader", required=True)
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--initrd", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--cmdline", default="")
    ap.add_argument("--omit-kernel", action="store_true")
    ap.add_argument("--corrupt-kernel", action="store_true")
    ap.add_argument("--omit-initrd", action="store_true")
    ap.add_argument("--corrupt-initrd", action="store_true")
    args = ap.parse_args(argv)
    with open(args.loader, "rb") as f:
        loader = f.read()
    with open(args.kernel, "rb") as f:
        kernel = f.read()
    with open(args.initrd, "rb") as f:
        initrd = f.read()
    img = build_image(loader, kernel, initrd, args.cmdline, args.omit_kernel,
                      args.corrupt_kernel, args.omit_initrd, args.corrupt_initrd)
    tmp = args.out + ".tmp"
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    with open(tmp, "wb") as f:
        f.write(img)
    os.replace(tmp, args.out)
    print("mkimage: %s sha256=%s" % (args.out, hashlib.sha256(img).hexdigest()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
