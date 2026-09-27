"""Tests for tools/hw-inventory (stdlib unittest).

Run from the repository root:
    python3 -m unittest discover -s tools/hw-inventory -v
"""

import base64
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import struct
import subprocess
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
FIXTURES = os.path.join(ROOT, "tests", "fixtures", "acpi", "lenovo-82k8")
HW_DOCS = os.path.join(ROOT, "docs", "hardware")

# Loaded under its own name: "profile" would collide with the stdlib module.
_spec = importlib.util.spec_from_file_location("nanox_hw_profile", os.path.join(HERE, "profile.py"))
profile = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(profile)

SERIAL = "PF3ZK9QX"
UUID = "4c4c4544-0042-3510-8051-b4c04f4b3732"
MAC = "a4:c3:f0:12:34:56"
PRODUCT_KEY = "NF6HC-QT7RX-8KVTB-2JMWX-9GK3D"
HOSTNAME = "LAPTOP-NANOXQ7"


def read_text(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def acpi_entry(sig, data):
    return {"signature": sig, "instances": 1, "length": len(data), "revision": data[8],
            "checksum_ok": sum(data) & 0xFF == 0, "oem_id": data[10:16].decode().strip(),
            "oem_table_id": data[16:24].decode().strip(), "oem_revision": struct.unpack_from("<I", data, 24)[0],
            "sha256": hashlib.sha256(data).hexdigest(), "data_base64": base64.b64encode(data).decode()}


def table(sig, body):
    """A syntactically valid ACPI table with a correct checksum."""
    data = bytearray(sig.encode() + struct.pack("<I", 36 + len(body)) + b"\x01\x00" + b"TESTOE"
                     + b"TESTTBL " + struct.pack("<I", 1) + b"TEST" + struct.pack("<I", 1) + body)
    data[9] = (-sum(data)) & 0xFF
    return bytes(data)


def pci(bdf, ven, dev, cls, name, sv="17aa", sd="3801", rev="00"):
    return {"bdf": bdf, "vendor": ven, "device": dev, "subsys_vendor": sv, "subsys_device": sd,
            "revision": rev, "class": cls, "name": name, "driver": None}


def sample_inventory():
    tables = []
    for sig in ("APIC", "FACP", "HPET", "IVRS", "MCFG"):
        with open(os.path.join(FIXTURES, f"{sig}.bin"), "rb") as f:
            tables.append(acpi_entry(sig, f.read()))
    return {
        "schema_version": 1, "kind": "nanox-hw-inventory", "collector": "collect-windows.ps1",
        "collector_version": 1, "collected_utc": "2026-09-26T00:00:00Z",
        "host_os": {"family": "windows", "version": "10.0.26200", "build": "26200"}, "privileged": False,
        "system": {"manufacturer": "LENOVO", "model": "82K8", "family": "Legion S7 15ACH6",
                   "board_manufacturer": "LENOVO", "board_product": "LNVNB161216", "hypervisor_present": False},
        "bios": {"vendor": "LENOVO", "version": "HACN46WW", "date": "2024-11-14", "release": "1.46"},
        "firmware": {"type": "uefi", "secure_boot": "enabled", "dma_protection_available": True},
        "cpu": {"name": "AMD Ryzen 7 5800H with Radeon Graphics", "vendor": "AuthenticAMD", "family": 25,
                "model": 80, "stepping": 0, "packages": 1, "cores": 8, "threads": 16},
        "memory": {"installed_bytes": 42949672960, "visible_bytes": 42268614656,
                   "modules": [{"locator": "DIMM 0", "size_bytes": 8589934592, "part_number": "M471A1G44AB0-CWE"}]},
        "pci": [
            pci("0000:00:00.0", "1022", "1630", "060000", "Host bridge", "1022", "1630"),
            pci("0000:00:01.1", "1022", "1633", "060400", "PCI Express Root Port", "1022", "1453"),
            pci("0000:01:00.0", "10de", "2560", "030000", "NVIDIA GeForce RTX 3060 Laptop GPU", rev="a1"),
            pci("0000:02:00.0", "8086", "2723", "028000", "Wi-Fi AX200", "1a56", "1654", "1a"),
            pci("0000:04:00.0", "144d", "a808", "010802", "Samsung NVMe Controller", "144d", "a801"),
            pci("0000:06:00.0", "1002", "1638", "030000", "AMD Radeon(TM) Graphics", "17aa", "380c", "c5"),
            pci("0000:06:00.3", "1022", "1639", "0c0330", "xHCI", "1022", "1639"),
        ],
        "storage_drives": [{"model": "NVMe SAMSUNG MZVLB512", "bus": "NVMe", "size_bytes": 512110190592,
                            "firmware": "3L1Q"},
                           {"model": "Live USB stick", "bus": "USB", "size_bytes": 16000000000, "firmware": "1.00"}],
        "network_adapters": [{"name": "Wi-Fi AX200", "bus": "pci", "vendor": "8086", "device": "2723"}],
        "acpi_tables": tables, "acpi_skipped_count": 20,
        "iommu": {"firmware_tables": ["IVRS"], "groups": None},
        "limitations": ["IOMMU groups are not exposed by Windows"],
    }


class Workdir(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="hw-inventory-test-")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def write_inv(self, inv, name="inv.json"):
        path = os.path.join(self.dir, name)
        with open(path, "w", encoding="utf-8") as f:
            json.dump(inv, f)
        return path

    def run_main(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = profile.main(list(argv))
        return code, out.getvalue(), err.getvalue()

    def generate(self, inv, out_name="p.toml", *extra):
        out = os.path.join(self.dir, out_name)
        code, stdout, err = self.run_main(self.write_inv(inv), "--id", "test-82k8", "--out", out, *extra)
        return code, out, stdout + err


class GenerateTest(Workdir):
    def test_profile_from_sample(self):
        code, out, msg = self.generate(sample_inventory())
        self.assertEqual(code, 0, msg)
        with open(out, "rb") as f:
            prof = tomllib.load(f)
        self.assertEqual(prof["status"], "candidate")
        self.assertEqual(prof["bios"]["version"], "HACN46WW")
        roles = {d["bdf"]: (d["role"], d["nanox"]) for d in prof["pci"]}
        self.assertEqual(roles["0000:04:00.0"], ("storage", "planned"))
        self.assertEqual(roles["0000:02:00.0"], ("network", "none"))
        self.assertEqual(roles["0000:06:00.3"], ("usb", "planned"))
        self.assertEqual(roles["0000:00:01.1"], ("bridge", "planned"))
        self.assertEqual(roles["0000:01:00.0"], ("display", "none"))
        self.assertNotIn("driver", {v[1] for v in roles.values()})  # never assigned by the tool
        # USB-attached drives (the live stick) are not part of the machine.
        self.assertEqual([s["model"] for s in prof["storage"]], ["NVMe SAMSUNG MZVLB512"])
        acpi = {t["signature"]: t for t in prof["acpi"]}
        self.assertEqual(acpi["APIC"]["summary"]["cpus_enabled"], 16)
        self.assertEqual(len(acpi["APIC"]["summary"]["ioapics"]), 2)
        self.assertEqual(acpi["MCFG"]["summary"]["segments"][0]["bus_end"], 63)
        self.assertEqual(prof["iommu"]["kind"], "amd-vi")
        self.assertEqual(len(prof["iommu"]["units"]), 1)  # one IOMMU, three IVHD types
        risks = " ".join(prof["detected_risks"])
        self.assertIn("Secure Boot is enabled", risks)
        self.assertIn("Hybrid graphics", risks)
        self.assertIn("No wired Ethernet", risks)
        self.assertNotIn("data_base64", read_text(out))

    def test_editable_fields_survive_regeneration(self):
        inv = sample_inventory()
        code, out, msg = self.generate(inv)
        self.assertEqual(code, 0, msg)
        text = read_text(out)
        text = text.replace('risks = []', 'risks = ["USB boot stick must be written by the owner"]')
        text = re.sub(r'(bdf = "0000:02:00.0"(?:\n[^\n\[]+)*?\nnanox = )"none"', r'\1"planned"', text)
        with open(out, "w", encoding="utf-8") as f:
            f.write(text)
        code, out, msg = self.generate(inv)
        self.assertEqual(code, 0, msg)
        prof = tomllib.loads(read_text(out))
        self.assertEqual(prof["risks"], ["USB boot stick must be written by the owner"])
        self.assertEqual({d["bdf"]: d["nanox"] for d in prof["pci"]}["0000:02:00.0"], "planned")

    def test_confirmed_profile_not_overwritten_with_other_facts(self):
        inv = sample_inventory()
        code, out, msg = self.generate(inv)
        text = read_text(out).replace('status = "candidate"', 'status = "confirmed"')
        with open(out, "w", encoding="utf-8") as f:
            f.write(text)
        inv["bios"]["version"] = "HACN47WW"
        code, _, msg = self.generate(inv)
        self.assertEqual(code, 2)
        self.assertIn("confirmed", msg)
        self.assertEqual(read_text(out), text)
        code, _, msg = self.generate(inv, "p.toml", "--force")
        self.assertEqual(code, 0, msg)

    def test_invalid_input_rejected(self):
        inv = sample_inventory()
        inv["acpi_tables"][0]["sha256"] = "0" * 64
        code, _, msg = self.generate(inv)
        self.assertEqual(code, 2)
        self.assertIn("sha256", msg)
        inv = sample_inventory()
        inv["kind"] = "something-else"
        self.assertEqual(self.generate(inv)[0], 2)
        inv = sample_inventory()
        inv["pci"][0]["bdf"] = "../../etc"
        self.assertEqual(self.generate(inv)[0], 2)

    def test_malformed_acpi_table_is_reported_not_fatal(self):
        inv = sample_inventory()
        # MADT entry with length 0 would loop forever without the length check.
        bad = table("APIC", struct.pack("<II", 0xFEE00000, 1) + b"\x00\x00\x00\x00")
        inv["acpi_tables"] = [acpi_entry("APIC", bad)]
        code, out, msg = self.generate(inv)
        self.assertEqual(code, 0, msg)
        prof = tomllib.loads(read_text(out))
        self.assertIn("bad length", prof["acpi"][0]["summary_error"])
        self.assertTrue(any("did not decode" in r for r in prof["detected_risks"]))


class CheckTest(Workdir):
    def setUp(self):
        super().setUp()
        code, self.prof, msg = self.generate(sample_inventory())
        self.assertEqual(code, 0, msg)

    def check(self, inv):
        return self.run_main(self.write_inv(inv, "new.json"), "--check", self.prof)

    def test_same_collection_matches(self):
        code, out, _ = self.check(sample_inventory())
        self.assertEqual(code, 0, out)
        self.assertIn("matches", out)

    def test_new_and_missing_device(self):
        inv = sample_inventory()
        inv["pci"].append(pci("0000:00:00.2", "1022", "1631", "080600", "AMD IOMMU", "1022", "1631"))
        del inv["pci"][4]  # the NVMe controller
        code, out, _ = self.check(inv)
        self.assertEqual(code, 1)
        self.assertIn("new device: 0000:00:00.2 1022:1631", out)
        self.assertIn("device disappeared: 0000:04:00.0 144d:a808", out)

    def test_changed_device_revision(self):
        inv = sample_inventory()
        inv["pci"][3]["revision"] = "1b"
        code, out, _ = self.check(inv)
        self.assertEqual(code, 1)
        self.assertIn("0000:02:00.0 changed", out)

    def test_bios_update_and_acpi_change(self):
        inv = sample_inventory()
        inv["bios"]["version"] = "HACN47WW"
        inv["bios"]["date"] = "2025-03-01"
        with open(os.path.join(FIXTURES, "HPET.bin"), "rb") as f:
            hpet = bytearray(f.read())
        hpet[44] ^= 0x10  # another HPET base address
        hpet[9] = 0
        hpet[9] = (-sum(hpet)) & 0xFF
        inv["acpi_tables"][2] = acpi_entry("HPET", bytes(hpet))
        code, out, _ = self.check(inv)
        self.assertEqual(code, 1)
        self.assertIn("bios.version: 'HACN46WW' -> 'HACN47WW'", out)
        self.assertIn("bios.date", out)
        self.assertIn("acpi: HPET sha256", out)

    def test_unobserved_fields_are_not_differences(self):
        # A non-root Linux collection: no ACPI tables, Secure Boot unknown.
        inv = sample_inventory()
        inv["acpi_tables"] = []
        inv["firmware"]["secure_boot"] = "unknown"
        inv["memory"]["installed_bytes"] = None
        code, out, _ = self.check(inv)
        self.assertEqual(code, 0, out)
        self.assertIn("not observed by this collection: acpi", out)
        self.assertIn("not observed by this collection: firmware.secure_boot", out)

    def test_storage_firmware_update_is_a_difference(self):
        inv = sample_inventory()
        inv["storage_drives"][0]["firmware"] = "5L2Q"
        code, out, _ = self.check(inv)
        self.assertEqual(code, 1)
        self.assertIn("storage", out)


class RedactionTest(Workdir):
    def poisoned(self):
        inv = sample_inventory()
        inv["system"]["serial_number"] = SERIAL
        inv["system"]["uuid"] = UUID
        inv["computer_name"] = HOSTNAME
        inv["network_adapters"][0]["mac_address"] = MAC
        inv["pci"][3]["instance_id"] = f"PCI\\VEN_8086&DEV_2723\\{SERIAL}"
        inv["storage_drives"][0]["serial_number"] = SERIAL + "-DISK"
        msdm = table("MSDM", struct.pack("<IIII", 1, 0, 1, 29) + PRODUCT_KEY.encode())
        inv["acpi_tables"].append(acpi_entry("MSDM", msdm))
        inv["acpi_tables"].append(acpi_entry("VFCT", table("VFCT", b"ATOMBIOS VBIOS IMAGE" * 4)))
        return inv

    def test_sensitive_inputs_do_not_reach_the_profile(self):
        code, out, msg = self.generate(self.poisoned())
        self.assertEqual(code, 0, msg)
        text = read_text(out)
        for secret in (SERIAL, UUID, MAC, MAC.replace(":", "-"), PRODUCT_KEY, HOSTNAME, "MSDM", "VFCT",
                       "ATOMBIOS", "serial", "uuid", "mac_address"):
            self.assertNotIn(secret.lower(), text.lower(), secret)
        self.assertEqual({t["signature"] for t in tomllib.loads(text)["acpi"]},
                         {"APIC", "FACP", "HPET", "IVRS", "MCFG"})

    def test_identifier_shaped_names_are_redacted(self):
        inv = sample_inventory()
        inv["pci"][3]["name"] = f"Wi-Fi {MAC.upper()} {UUID}"
        code, out, msg = self.generate(inv)
        self.assertEqual(code, 0, msg)
        text = read_text(out)
        self.assertNotIn(MAC.upper(), text)
        self.assertNotIn(UUID, text)
        self.assertIn("Wi-Fi <redacted> <redacted>", text)

    def test_known_secret_inside_an_allowed_field_blocks_writing(self):
        inv = self.poisoned()
        inv["pci"][3]["name"] = f"Wi-Fi AX200 SN {SERIAL}"
        code, out, msg = self.generate(inv)
        self.assertEqual(code, 2)
        self.assertIn("refusing to write", msg)
        self.assertFalse(os.path.exists(out))

    def test_product_key_in_allowed_field_is_redacted(self):
        inv = sample_inventory()
        inv["system"]["sku"] = PRODUCT_KEY
        code, out, msg = self.generate(inv)
        self.assertEqual(code, 0, msg)
        self.assertNotIn(PRODUCT_KEY, read_text(out))


class CollectorsTest(unittest.TestCase):
    def test_windows_collector_reads_identifiers_only_in_the_self_check(self):
        with open(os.path.join(HERE, "collect-windows.ps1"), encoding="utf-8") as f:
            src = f.read()
        head, sep, tail = src.partition("# ------------------------------------------------------- leak self-check")
        self.assertTrue(sep, "self-check marker missing")
        for word in ("SerialNumber", "MACAddress", "IdentifyingNumber", "COMPUTERNAME", "USERNAME", "AssetTag"):
            self.assertNotIn(word, head, word)
        self.assertNotRegex(head, r"\bUUID\b")
        for word in ("SerialNumber", "MACAddress", "COMPUTERNAME", "USERNAME"):
            self.assertIn(word, tail)
        allowed = re.search(r"\$AllowedAcpi = @\(([^)]*)\)", head).group(1)
        self.assertEqual(re.findall(r"'(\w{4})'", allowed), list(profile.ALLOWED_ACPI))

    @unittest.skipUnless(shutil.which("bash"), "bash not available")
    def test_linux_collector_zeroes_device_serial_number(self):
        # Header lines, 256 bytes of standard config space, then extended
        # capabilities: AER (0001h) at 100h -> DSN (0003h) at 140h -> end.
        cfg = bytearray(512)
        cfg[0:4] = bytes.fromhex("86802327")
        cfg[0x100:0x104] = struct.pack("<I", 0x0001 | (1 << 16) | (0x140 << 20))
        cfg[0x140:0x144] = struct.pack("<I", 0x0003 | (1 << 16) | (0 << 20))
        cfg[0x144:0x14C] = bytes.fromhex("5634120f0f3ca4c0")
        cfg[0x14C] = 0xAB  # byte after the DSN capability must survive
        lines = ["02:00.0 Network controller [0280]: Intel Corporation Wi-Fi 6 AX200 [8086:2723] (rev 1a)"]
        for off in range(0, len(cfg), 16):
            lines.append(f"{off:02x}: " + " ".join(f"{b:02x}" for b in cfg[off:off + 16]))
        lines.append("")
        res = subprocess.run(["bash", os.path.join(HERE, "collect-linux.sh"), "--filter-config-space"],
                             input="\n".join(lines) + "\n", capture_output=True, text=True, check=True)
        out = {l.split(":")[0]: l for l in res.stdout.splitlines() if re.match(r"^[0-9a-f]+: ", l)}
        self.assertEqual(out["140"], "140: 03 00 01 00 00 00 00 00 00 00 00 00 ab 00 00 00")
        self.assertTrue(out["00"].startswith("00: 86 80 23 27"))
        self.assertTrue(res.stdout.startswith("02:00.0 Network controller"))
        self.assertNotIn("56 34 12 0f", res.stdout)

    @unittest.skipUnless(shutil.which("bash"), "bash not available")
    def test_linux_collector_survives_capability_cycle(self):
        cfg = bytearray(512)
        cfg[0x100:0x104] = struct.pack("<I", 0x0001 | (1 << 16) | (0x100 << 20))  # points to itself
        lines = ["00:00.0 Host bridge [0600]: x"]
        lines += [f"{o:02x}: " + " ".join(f"{b:02x}" for b in cfg[o:o + 16]) for o in range(0, 512, 16)]
        res = subprocess.run(["bash", os.path.join(HERE, "collect-linux.sh"), "--filter-config-space"],
                             input="\n".join(lines) + "\n", capture_output=True, text=True, timeout=30)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(len(res.stdout.splitlines()), len(lines))


class CommittedProfilesTest(unittest.TestCase):
    """docs/hardware must stay free of identifiers and consistent with its evidence."""

    def files(self):
        for base, _, names in os.walk(HW_DOCS):
            for n in names:
                yield os.path.join(base, n)

    def test_no_identifier_shaped_strings(self):
        found = False
        for path in self.files():
            found = True
            text = read_text(path)
            for rx in (profile.MAC_RE, profile.UUID_RE, profile.PRODUCT_KEY_RE):
                self.assertIsNone(rx.search(text), f"{path}: {rx.pattern}")
            self.assertNotRegex(text, r'(?i)"(serial|serial_?number|uuid|mac_?address|asset_?tag)"\s*:', path)
            if path.endswith(".json"):
                inv = json.loads(text)
                sigs = {t["signature"] for t in inv.get("acpi_tables", [])}
                self.assertTrue(sigs <= set(profile.ALLOWED_ACPI), f"{path}: {sigs}")
        self.assertTrue(found, "docs/hardware is empty")

    def test_profiles_match_their_inventories(self):
        checked = 0
        for path in self.files():
            if not path.endswith(".toml"):
                continue
            prof = profile.load_profile(path)
            ref = prof.get("source", {}).get("inventory")
            if not ref:
                continue
            inv_path = os.path.join(ROOT, ref)
            inv, sha = profile.load_inventory(inv_path)
            self.assertEqual(sha, prof["source"]["inventory_sha256"], path)
            diffs, _ = profile.compare(profile.profile_facts(prof), profile.facts(inv))
            self.assertEqual(diffs, [], path)
            doc, _ = profile.build(inv, sha, prof["id"], prof, ref)
            self.assertEqual(profile.render(doc, inv), read_text(path), path)
            checked += 1
        self.assertGreater(checked, 0)


if __name__ == "__main__":
    unittest.main()
