#!/usr/bin/env python3
"""Build or check a NANOX physical hardware profile (docs/hardware/<id>.toml).

Input is the JSON written by collect-windows.ps1 or collect-linux.sh (schema
"nanox-hw-inventory" v1; a collect-linux output directory is accepted too).

  profile.py INVENTORY --id lenovo-82k8 --out docs/hardware/lenovo-82k8.toml
  profile.py INVENTORY --check docs/hardware/lenovo-82k8.toml

Generation copies an allowlist of facts, decodes the allowlisted ACPI tables
into short summaries (raw bytes are never written) and keeps the fields a
person edits (status, confirmed_by, confirmed_on, notes, risks, and per PCI
function nanox/note) from an existing profile. A profile whose status is
"confirmed" is not overwritten with different facts without --force.

--check compares the facts of an existing profile with a new collection:
exit 0 when nothing differs, 1 when devices, firmware, CPU, memory or ACPI
tables differ, 2 on invalid input. Fields the new collection could not
observe (e.g. ACPI without root, Secure Boot "unknown") are listed but are
not differences.

Privacy: identifying values never reach the output. Only allowlisted fields
are copied; MAC-, UUID- and product-key-shaped strings are refused; values
of input keys named like serial/uuid/mac/asset/hostname/user, and the payload
of an MSDM table, are searched for in the rendered text before writing.
Only the Python standard library is used.
"""

import argparse
import base64
import hashlib
import json
import os
import re
import struct
import sys
import tomllib

SCHEMA_VERSION = 1
ALLOWED_ACPI = ("APIC", "FACP", "HPET", "MCFG", "IVRS", "DMAR", "SRAT", "SLIT")
STATUSES = ("candidate", "confirmed")
NANOX_SUPPORT = ("none", "planned", "driver")

MAC_RE = re.compile(r"(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b")
UUID_RE = re.compile(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
PRODUCT_KEY_RE = re.compile(r"\b[A-Z0-9]{5}(?:-[A-Z0-9]{5}){4}\b")
SENSITIVE_KEY_RE = re.compile(r"(?i)serial|uuid|guid|mac|asset|host_?name|computer|user|product_?key|identifying")
GENERIC_VALUES = {"none", "default string", "to be filled by o.e.m.", "system serial number",
                  "not specified", "not applicable", "unknown"}


class InputError(Exception):
    pass


# --------------------------------------------------------------- helpers
def clean(value, limit=160):
    """Printable, trimmed text; identifier-shaped substrings are removed."""
    if value is None:
        return None
    s = re.sub(r"[\x00-\x1f\x7f]", " ", str(value)).strip()
    s = re.sub(r"\s+", " ", s)
    for rx in (MAC_RE, UUID_RE, PRODUCT_KEY_RE):
        s = rx.sub("<redacted>", s)
    s = s[:limit]
    return s or None


def hexid(value, width):
    s = clean(value)
    if s is None:
        return None
    s = s.lower().removeprefix("0x")
    if not re.fullmatch(r"[0-9a-f]{%d}" % width, s):
        raise InputError(f"bad hex id {value!r} (want {width} digits)")
    return s


def as_int(value):
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return None


def drop_none(d):
    return {k: v for k, v in d.items() if v is not None and v != [] and v != {}}


def section(inv, name):
    v = inv.get(name)
    return v if isinstance(v, dict) else {}


# ------------------------------------------------------------------ ACPI
class Table:
    def __init__(self, data):
        self.data = data

    def u(self, fmt, off):
        size = struct.calcsize(fmt)
        if off < 0 or off + size > len(self.data):
            raise ValueError(f"field at {off:#x} beyond table length {len(self.data)}")
        return struct.unpack_from("<" + fmt, self.data, off)[0]

    def entries(self, start, type_fmt, len_fmt, len_off, min_len):
        """Yield (offset, type, length) of variable-length subtables."""
        off = start
        while off < len(self.data):
            typ = self.u(type_fmt, off)
            length = self.u(len_fmt, off + len_off)
            if length < min_len or off + length > len(self.data):
                raise ValueError(f"subtable at {off:#x} has bad length {length}")
            yield off, typ, length
            off += length


def bdf16(v):
    return f"{v >> 8:02x}:{(v >> 3) & 0x1f:02x}.{v & 7:x}"


def summarize_apic(t):
    s = {"local_apic_address": f"{t.u('I', 36):#x}", "pcat_compat": bool(t.u("I", 40) & 1)}
    enabled = online_capable = disabled = x2apic = iso = nmi = 0
    ioapics = []
    for off, typ, _ in t.entries(44, "B", "B", 1, 2):
        if typ in (0, 9):
            flags = t.u("I", off + 4) if typ == 0 else t.u("I", off + 8)
            x2apic += typ == 9
            if flags & 1:
                enabled += 1
            elif flags & 2:
                online_capable += 1
            else:
                disabled += 1
        elif typ == 1:
            ioapics.append({"id": t.u("B", off + 2), "address": f"{t.u('I', off + 4):#x}",
                            "gsi_base": t.u("I", off + 8)})
        elif typ == 2:
            iso += 1
        elif typ in (3, 4, 0xA):
            nmi += 1
    s.update(cpus_enabled=enabled, cpus_online_capable=online_capable, cpus_disabled=disabled,
             x2apic_entries=x2apic, ioapics=ioapics, interrupt_overrides=iso, nmi_entries=nmi)
    return s


def summarize_facp(t):
    flags = t.u("I", 112)
    boot = t.u("H", 109)
    s = {"minor_revision": t.u("B", 131) if len(t.data) > 131 else 0,
         "sci_interrupt": t.u("H", 46),
         "hardware_reduced": bool(flags & (1 << 20)),
         "reset_register_supported": bool(flags & (1 << 10)),
         "pm_timer_32bit": bool(flags & (1 << 8)),
         "legacy_devices": bool(boot & 1), "has_8042": bool(boot & 2),
         "vga_not_present": bool(boot & 4), "msi_not_supported": bool(boot & 8),
         "cmos_rtc_not_present": bool(boot & 32)}
    return s


def summarize_hpet(t):
    block = t.u("I", 36)
    return {"base_address": f"{t.u('Q', 44):#x}", "comparators": ((block >> 8) & 0x1f) + 1,
            "counter_64bit": bool(block & (1 << 13)), "vendor": f"{block >> 16:04x}"}


def summarize_mcfg(t):
    segs = []
    off = 44
    while off + 16 <= len(t.data):
        segs.append({"base": f"{t.u('Q', off):#x}", "segment": t.u("H", off + 8),
                     "bus_start": t.u("B", off + 10), "bus_end": t.u("B", off + 11)})
        off += 16
    if off != len(t.data):
        raise ValueError("MCFG length is not 44 + 16*n")
    return {"segments": segs}


def summarize_ivrs(t):
    ivhd, ivmd = [], 0
    for off, typ, _ in t.entries(48, "B", "H", 2, 8):
        if typ in (0x10, 0x11, 0x40):
            ivhd.append({"type": f"{typ:#04x}", "iommu_bdf": bdf16(t.u("H", off + 4)),
                         "base_address": f"{t.u('Q', off + 8):#x}", "segment": t.u("H", off + 16)})
        elif typ in (0x20, 0x21, 0x22):
            ivmd += 1
    return {"ivinfo": f"{t.u('I', 36):#x}", "ivhd": ivhd, "ivmd_count": ivmd}


def summarize_dmar(t):
    drhd, rmrr, atsr = [], 0, 0
    for off, typ, _ in t.entries(48, "H", "H", 2, 4):
        if typ == 0:
            drhd.append({"segment": t.u("H", off + 6), "register_base": f"{t.u('Q', off + 8):#x}",
                         "include_pci_all": bool(t.u("B", off + 4) & 1)})
        elif typ == 1:
            rmrr += 1
        elif typ == 2:
            atsr += 1
    return {"host_address_width": t.u("B", 36) + 1, "flags": f"{t.u('B', 37):#x}",
            "drhd": drhd, "rmrr_count": rmrr, "atsr_count": atsr}


def summarize_srat(t):
    counts = {}
    for _, typ, _ in t.entries(48, "B", "B", 1, 2):
        counts[typ] = counts.get(typ, 0) + 1
    return {"processor_affinity": counts.get(0, 0) + counts.get(2, 0), "memory_affinity": counts.get(1, 0)}


def summarize_slit(t):
    return {"localities": t.u("Q", 36)}


SUMMARIZERS = {"APIC": summarize_apic, "FACP": summarize_facp, "HPET": summarize_hpet,
               "MCFG": summarize_mcfg, "IVRS": summarize_ivrs, "DMAR": summarize_dmar,
               "SRAT": summarize_srat, "SLIT": summarize_slit}


def acpi_fact(entry):
    sig = entry.get("signature")
    raw = entry.get("data_base64")
    if raw is None:
        data = None
    else:
        try:
            data = base64.b64decode(raw, validate=True)
        except ValueError as e:
            raise InputError(f"ACPI {sig}: bad base64: {e}") from None
    sha = clean(entry.get("sha256"))
    if data is not None:
        actual = hashlib.sha256(data).hexdigest()
        if sha is not None and sha.lower() != actual:
            raise InputError(f"ACPI {sig}: sha256 in inventory does not match the table bytes")
        sha = actual
        if data[:4] != sig.encode():
            raise InputError(f"ACPI {sig}: bytes carry another signature")
    if sha is None or not re.fullmatch(r"[0-9a-f]{64}", sha.lower()):
        raise InputError(f"ACPI {sig}: missing sha256")
    fact = {"signature": sig, "length": as_int(entry.get("length")), "revision": as_int(entry.get("revision")),
            "oem_id": clean(entry.get("oem_id")), "oem_table_id": clean(entry.get("oem_table_id")),
            "oem_revision": as_int(entry.get("oem_revision")), "sha256": sha.lower(),
            "checksum_ok": entry.get("checksum_ok") if isinstance(entry.get("checksum_ok"), bool) else None}
    if data is not None:
        fact["length"] = len(data)
        fact["checksum_ok"] = sum(data) & 0xFF == 0
        try:
            if len(data) < 36 or struct.unpack_from("<I", data, 4)[0] != len(data):
                raise ValueError("header length does not match the data")
            fact["summary"] = SUMMARIZERS[sig](Table(data))
        except (ValueError, struct.error) as e:
            fact["summary_error"] = str(e)
    return drop_none(fact)


# ------------------------------------------------------------ PCI roles
def pci_role(cls):
    base, sub = cls[:2], cls[:4]
    specific = {"0c03": "usb", "0c05": "smbus", "0806": "iommu", "0805": "sd-host", "0403": "audio",
                "0700": "serial", "0d00": "network", "0d80": "network"}
    by_base = {"01": "storage", "02": "network", "03": "display", "04": "multimedia", "06": "bridge",
               "07": "communication", "08": "system", "0c": "serial-bus", "0d": "network",
               "10": "crypto", "11": "signal-processing"}
    return specific.get(sub) or by_base.get(base, "other")


def default_support(cls, role):
    # Only the planned M9 path is marked; "driver" is set by a person once a
    # NANOX driver has been checked on this machine.
    if cls in ("010802", "010601", "0c0330") or role in ("bridge", "iommu"):
        return "planned"
    return "none"


# ---------------------------------------------------------------- facts
def load_inventory(path):
    if os.path.isdir(path):
        path = os.path.join(path, "inventory.json")
    with open(path, "rb") as f:
        raw = f.read()
    try:
        inv = json.loads(raw.decode("utf-8-sig"))
    except (UnicodeDecodeError, json.JSONDecodeError) as e:
        raise InputError(f"{path}: not JSON: {e}") from None
    if not isinstance(inv, dict) or inv.get("kind") != "nanox-hw-inventory":
        raise InputError(f"{path}: not a nanox-hw-inventory file")
    if inv.get("schema_version") != 1:
        raise InputError(f"{path}: unsupported schema_version {inv.get('schema_version')!r}")
    return inv, hashlib.sha256(raw).hexdigest()


def facts(inv):
    sysd, bios, fw, cpu, mem = (section(inv, k) for k in ("system", "bios", "firmware", "cpu", "memory"))
    out = {
        "system": drop_none({k: clean(sysd.get(k)) for k in (
            "manufacturer", "model", "family", "version", "sku",
            "board_manufacturer", "board_product", "board_version")}),
        "bios": drop_none({k: clean(bios.get(k)) for k in ("vendor", "version", "date", "release", "ec_release")}),
        "firmware": drop_none({
            "type": fw.get("type") if fw.get("type") in ("uefi", "legacy") else None,
            "secure_boot": fw.get("secure_boot") if fw.get("secure_boot") in ("enabled", "disabled") else None,
            "dma_protection_available": fw.get("dma_protection_available")
            if isinstance(fw.get("dma_protection_available"), bool) else None}),
        "cpu": drop_none({"name": clean(cpu.get("name")), "vendor": clean(cpu.get("vendor")),
                          **{k: as_int(cpu.get(k)) for k in ("family", "model", "stepping", "packages",
                                                              "cores", "threads")}}),
    }
    modules = []
    for m in mem.get("modules") or []:
        if isinstance(m, dict):
            modules.append(drop_none({"locator": clean(m.get("locator")), "bank": clean(m.get("bank")),
                                      "size_bytes": as_int(m.get("size_bytes")),
                                      "speed_mts": as_int(m.get("speed_mts")),
                                      "manufacturer": clean(m.get("manufacturer")),
                                      "part_number": clean(m.get("part_number"))}))
    out["memory"] = drop_none({"installed_bytes": as_int(mem.get("installed_bytes")),
                               "visible_bytes": as_int(mem.get("visible_bytes")), "modules": modules})

    pci = []
    for d in inv.get("pci") or []:
        bdf = clean(d.get("bdf"))
        if bdf is None or not re.fullmatch(r"[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]", bdf.lower()):
            raise InputError(f"PCI function with bad bdf {d.get('bdf')!r}")
        cls = hexid(d.get("class"), 6) or "000000"
        role = pci_role(cls)
        pci.append(drop_none({
            "bdf": bdf.lower(), "vendor": hexid(d.get("vendor"), 4), "device": hexid(d.get("device"), 4),
            "subsystem": f"{hexid(d.get('subsys_vendor'), 4)}:{hexid(d.get('subsys_device'), 4)}"
            if d.get("subsys_vendor") is not None and d.get("subsys_device") is not None else None,
            "revision": hexid(d.get("revision"), 2), "class": cls, "role": role, "name": clean(d.get("name"))}))
    pci.sort(key=lambda d: d["bdf"])
    out["pci"] = pci

    storage = []
    for s in inv.get("storage_drives") or []:
        bus = (clean(s.get("bus")) or "").lower()
        if bus == "usb":
            continue  # the boot stick or an external disk is not part of the machine
        storage.append(drop_none({"model": clean(s.get("model")), "bus": bus or None,
                                  "size_bytes": as_int(s.get("size_bytes")), "firmware": clean(s.get("firmware"))}))
    out["storage"] = storage
    out["network"] = [drop_none({"name": clean(n.get("name")), "bus": clean(n.get("bus")),
                                 "vendor": hexid(n.get("vendor"), 4), "device": hexid(n.get("device"), 4)})
                      for n in inv.get("network_adapters") or [] if isinstance(n, dict)]

    acpi = [acpi_fact(t) for t in inv.get("acpi_tables") or []
            if isinstance(t, dict) and t.get("signature") in ALLOWED_ACPI]
    acpi.sort(key=lambda t: t["signature"])
    out["acpi"] = acpi

    iommu = section(inv, "iommu")
    tables = sorted(t["signature"] for t in acpi if t["signature"] in ("IVRS", "DMAR"))
    # IVRS repeats one IOMMU in IVHD types 10h/11h/40h; list each unit once.
    ivhd_types = {}
    units = []
    for t in acpi:
        for u in t.get("summary", {}).get("ivhd", []):
            ivhd_types.setdefault((u["segment"], u["iommu_bdf"], u["base_address"]), []).append(u["type"])
        for u in t.get("summary", {}).get("drhd", []):
            units.append(f"vt-d segment {u['segment']} @ {u['register_base']}")
    for (seg, bdf, base), types in ivhd_types.items():
        units.append(f"amd-vi {seg:04x}:{bdf} @ {base} (IVHD {', '.join(types)})")
    kind = "amd-vi" if "IVRS" in tables else "vt-d" if "DMAR" in tables else None
    out["iommu"] = drop_none({"kind": kind, "firmware_tables": tables, "units": sorted(set(units)),
                              "groups": as_int(iommu.get("groups"))})
    return out


def detected_risks(inv, f):
    risks = []
    by_role = {}
    for d in f["pci"]:
        by_role.setdefault(d["role"], []).append(d)
    classes = {d["class"][:4] for d in f["pci"]}
    fw = f["firmware"]
    if f["pci"] and "0200" not in classes:
        wireless = [f"{d['vendor']}:{d['device']}" for d in by_role.get("network", [])]
        risks.append("No wired Ethernet controller; network only via " +
                     (", ".join(wireless) if wireless else "nothing") + " (Wi-Fi needs firmware and a driver)")
    if f["pci"] and "0700" not in classes:
        risks.append("No PCI UART; early diagnostics need the framebuffer or USB (a legacy COM port, if any, "
                     "is not visible to this inventory)")
    if f["pci"] and not any(d["vendor"] == "1af4" for d in f["pci"]):
        native = sorted({d["class"] for d in f["pci"] if d["class"] in ("010802", "010601", "0c0330")})
        risks.append("No virtio devices: M4/M5 virtio drivers do not apply; native drivers needed for "
                     + (", ".join({"010802": "NVMe", "010601": "AHCI", "0c0330": "xHCI"}[c] for c in native)
                        or "storage and USB"))
    displays = by_role.get("display", [])
    if len(displays) > 1:
        risks.append("Hybrid graphics (" + ", ".join(f"{d['vendor']}:{d['device']}" for d in displays)
                     + "); GPU support is separate work for one selected device")
    if fw.get("secure_boot") == "enabled":
        risks.append("Secure Boot is enabled: an unsigned NANOX loader will not start until the owner "
                     "disables Secure Boot or enrolls a key")
    if fw.get("type") == "legacy":
        risks.append("Firmware boots in legacy mode; the NANOX loader is UEFI-only")
    if not f["iommu"].get("firmware_tables") and f["acpi"]:
        risks.append("No IVRS/DMAR table: IOMMU disabled or absent in firmware")
    if section(inv, "system").get("hypervisor_present") is True and inv.get("host_os", {}).get("family") == "windows":
        risks.append("Collected under Hyper-V (Windows root partition): the hypervisor owns the IOMMU and hides "
                     "some functions (e.g. the AMD IOMMU PCI function); confirm with collect-linux.sh from a live Linux")
    for t in f["acpi"]:
        if t.get("checksum_ok") is False:
            risks.append(f"ACPI {t['signature']} has a bad checksum")
        if "summary_error" in t:
            risks.append(f"ACPI {t['signature']} did not decode: {t['summary_error']}")
    return risks


# ------------------------------------------------------------------ TOML
def toml_str(s):
    out = ['"']
    for ch in s:
        if ch in '"\\':
            out.append("\\" + ch)
        elif ord(ch) < 0x20 or ord(ch) == 0x7F:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def toml_key(k):
    return k if re.fullmatch(r"[A-Za-z0-9_-]+", k) else toml_str(k)


def toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, str):
        return toml_str(v)
    if isinstance(v, list):
        return "[" + ", ".join(toml_value(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{ " + ", ".join(f"{toml_key(k)} = {toml_value(x)}" for k, x in v.items()) + " }"
    raise TypeError(f"cannot write {type(v).__name__} to TOML")


def is_table_array(v):
    return isinstance(v, list) and v and all(isinstance(x, dict) for x in v)


def dump_table(path, d, lines, header):
    if header:
        lines.append("")
        lines.append(header)
    subtables = []
    for k, v in d.items():
        if isinstance(v, dict) and path is not None:
            subtables.append((k, v))
        elif isinstance(v, dict) or (path is None and is_table_array(v)):
            subtables.append((k, v))
        elif (isinstance(v, list) and v and all(isinstance(x, (str, dict)) for x in v)
              and len(toml_value(v)) > 88):
            lines.append(f"{toml_key(k)} = [")
            lines.extend(f"  {toml_value(x)}," for x in v)
            lines.append("]")
        else:
            lines.append(f"{toml_key(k)} = {toml_value(v)}")
    for k, v in subtables:
        name = toml_key(k) if path is None else f"{path}.{toml_key(k)}"
        if isinstance(v, dict):
            dump_table(name, v, lines, f"[{name}]")
        else:
            for item in v:
                dump_table(name, item, lines, f"[[{name}]]")


def dump_toml(doc, comment):
    lines = [f"# {c}" if c else "#" for c in comment]
    dump_table(None, doc, lines, None)
    return "\n".join(lines) + "\n"


# ------------------------------------------------------------ build/check
EDITABLE_TOP = ("status", "confirmed_by", "confirmed_on", "notes", "risks")
EDITABLE_PCI = ("nanox", "note")


def pci_key(d):
    return (d.get("bdf"), d.get("vendor"), d.get("device"))


def load_profile(path):
    with open(path, "rb") as f:
        try:
            prof = tomllib.load(f)
        except tomllib.TOMLDecodeError as e:
            raise InputError(f"{path}: invalid TOML: {e}") from None
    if prof.get("schema_version") != SCHEMA_VERSION:
        raise InputError(f"{path}: unsupported schema_version")
    if prof.get("status") not in STATUSES:
        raise InputError(f"{path}: status must be one of {', '.join(STATUSES)}")
    for d in prof.get("pci", []):
        if d.get("nanox") not in NANOX_SUPPORT:
            raise InputError(f"{path}: pci {d.get('bdf')}: nanox must be one of {', '.join(NANOX_SUPPORT)}")
    return prof


def profile_facts(prof):
    f = {k: prof.get(k, {}) for k in ("system", "bios", "firmware", "cpu", "memory", "iommu")}
    f["pci"] = [{k: v for k, v in d.items() if k not in EDITABLE_PCI} for d in prof.get("pci", [])]
    for k in ("storage", "network", "acpi"):
        f[k] = prof.get(k, [])
    return f


def build(inv, inv_sha, profile_id, existing=None, inventory_ref=None):
    f = facts(inv)
    old_pci = {pci_key(d): d for d in (existing or {}).get("pci", [])}
    for d in f["pci"]:
        old = old_pci.get(pci_key(d), {})
        d["nanox"] = old.get("nanox", default_support(d["class"], d["role"]))
        if old.get("note"):
            d["note"] = old["note"]
    ex = existing or {}
    doc = {
        "schema_version": SCHEMA_VERSION,
        "id": profile_id,
        "status": ex.get("status", "candidate"),
        "confirmed_by": ex.get("confirmed_by", ""),
        "confirmed_on": ex.get("confirmed_on", ""),
        "notes": ex.get("notes", []),
        "risks": ex.get("risks", []),
        "detected_risks": detected_risks(inv, f),
        "source": drop_none({
            "collector": clean(inv.get("collector")),
            "collector_version": as_int(inv.get("collector_version")),
            "collected_utc": clean(inv.get("collected_utc")),
            "host_os": clean(" ".join(str(v) for v in section(inv, "host_os").values() if v)),
            "privileged": inv.get("privileged") if isinstance(inv.get("privileged"), bool) else None,
            "inventory": inventory_ref,
            "inventory_sha256": inv_sha,
            "limitations": [clean(x, 300) for x in inv.get("limitations") or [] if clean(x, 300)],
        }),
    }
    for k in ("system", "bios", "firmware", "cpu", "memory", "iommu", "pci", "storage", "network", "acpi"):
        doc[k] = f[k]
    return doc, f


def render(doc, inv):
    text = dump_toml(doc, [
        f"NANOX physical hardware profile {doc['id']} (docs/hardware/README.md).",
        "Generated by tools/hw-inventory/profile.py. Edit only: status, confirmed_by,",
        "confirmed_on, notes, risks and pci.nanox / pci.note; regeneration keeps them.",
        "status = \"confirmed\" is set by the machine owner, never by a tool.",
    ])
    assert_no_leaks(text, inv)
    return text


def forbidden_values(inv):
    found = []

    def walk(v, key=""):
        if isinstance(v, dict):
            for k, x in v.items():
                walk(x, k)
        elif isinstance(v, list):
            for x in v:
                walk(x, key)
        elif isinstance(v, str) and SENSITIVE_KEY_RE.search(key):
            s = v.strip()
            if len(s) >= 4 and s.lower() not in GENERIC_VALUES and not re.fullmatch(r"0+|f+", s, re.I):
                found.append(s)

    walk(inv)
    for t in inv.get("acpi_tables") or []:
        if isinstance(t, dict) and t.get("signature") == "MSDM" and t.get("data_base64"):
            try:
                payload = base64.b64decode(t["data_base64"])[36:]
            except ValueError:
                continue
            found += [m.decode() for m in re.findall(rb"[\x21-\x7e]{5,}", payload)]
    return found


def assert_no_leaks(text, inv):
    low = text.lower()
    for v in forbidden_values(inv):
        if v.lower() in low:
            raise InputError("refusing to write: output would contain an identifying value "
                             "(serial/UUID/MAC/host or user name/product key)")
    for rx, what in ((MAC_RE, "MAC address"), (UUID_RE, "UUID"), (PRODUCT_KEY_RE, "product key")):
        if rx.search(text):
            raise InputError(f"refusing to write: output contains a {what}-shaped string")


def compare(old, new):
    """Return (differences, unobserved) between two fact dicts."""
    diffs, unobserved = [], []
    for sec, keys in (("system", ("manufacturer", "model", "board_product")),
                      ("bios", ("vendor", "version", "date", "release", "ec_release")),
                      ("firmware", ("type", "secure_boot")),
                      ("cpu", ("vendor", "family", "model", "stepping", "packages", "cores", "threads")),
                      ("memory", ("installed_bytes",))):
        for k in keys:
            a, b = old.get(sec, {}).get(k), new.get(sec, {}).get(k)
            if a is not None and b is not None and a != b:
                diffs.append(f"{sec}.{k}: {a!r} -> {b!r}")
            elif a is not None and b is None:
                unobserved.append(f"{sec}.{k}")

    def pci_id(d):
        return (d.get("vendor"), d.get("device"), d.get("subsystem"), d.get("revision"), d.get("class"))

    def pci_desc(d):
        return f"{d['bdf']} {d.get('vendor')}:{d.get('device')} class {d.get('class')} {d.get('name') or ''}".rstrip()

    old_pci = {d["bdf"]: d for d in old.get("pci", [])}
    new_pci = {d["bdf"]: d for d in new.get("pci", [])}
    if new_pci or not old_pci:
        for bdf in sorted(old_pci.keys() - new_pci.keys()):
            diffs.append(f"pci: device disappeared: {pci_desc(old_pci[bdf])}")
        for bdf in sorted(new_pci.keys() - old_pci.keys()):
            diffs.append(f"pci: new device: {pci_desc(new_pci[bdf])}")
        for bdf in sorted(old_pci.keys() & new_pci.keys()):
            if pci_id(old_pci[bdf]) != pci_id(new_pci[bdf]):
                diffs.append(f"pci: {bdf} changed: {pci_desc(old_pci[bdf])} -> {pci_desc(new_pci[bdf])}")
    else:
        unobserved.append("pci")

    old_fw = sorted(s.get("firmware", "?") for s in old.get("storage", []))
    new_fw = sorted(s.get("firmware", "?") for s in new.get("storage", []))
    if old_fw != new_fw:
        diffs.append(f"storage: drives/firmware {old_fw} -> {new_fw}")

    old_acpi = {t["signature"]: t for t in old.get("acpi", [])}
    new_acpi = {t["signature"]: t for t in new.get("acpi", [])}
    if new_acpi:
        for sig in sorted(old_acpi.keys() | new_acpi.keys()):
            a, b = old_acpi.get(sig), new_acpi.get(sig)
            if a is None:
                diffs.append(f"acpi: new table {sig}")
            elif b is None:
                diffs.append(f"acpi: table {sig} disappeared")
            elif a["sha256"] != b["sha256"]:
                diffs.append(f"acpi: {sig} sha256 {a['sha256'][:16]}... -> {b['sha256'][:16]}...")
    elif old_acpi:
        unobserved.append("acpi")
    return diffs, unobserved


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("inventory", help="inventory JSON, or a collect-linux.sh output directory")
    ap.add_argument("--id", help="profile id, e.g. lenovo-82k8")
    ap.add_argument("--out", help="profile TOML to write (default: stdout)")
    ap.add_argument("--inventory-ref", help="where the inventory is kept, recorded in [source]")
    ap.add_argument("--check", metavar="PROFILE", help="compare PROFILE with the inventory instead of writing")
    ap.add_argument("--force", action="store_true", help="overwrite a confirmed profile whose facts differ")
    args = ap.parse_args(argv)
    try:
        inv, inv_sha = load_inventory(args.inventory)
        if args.check:
            prof = load_profile(args.check)
            diffs, unobserved = compare(profile_facts(prof), facts(inv))
            for u in unobserved:
                print(f"not observed by this collection: {u}")
            for d in diffs:
                print(f"DIFF {d}")
            print(f"{args.check}: {'differs' if diffs else 'matches'} ({len(diffs)} differences)")
            return 1 if diffs else 0
        if not args.id or not re.fullmatch(r"[a-z0-9][a-z0-9-]*", args.id):
            raise InputError("--id is required: lowercase letters, digits and '-'")
        existing = None
        if args.out and os.path.exists(args.out):
            existing = load_profile(args.out)
            if existing.get("id") != args.id:
                raise InputError(f"{args.out} has id {existing.get('id')!r}, not {args.id!r}")
        doc, f = build(inv, inv_sha, args.id, existing, args.inventory_ref)
        if existing and existing.get("status") == "confirmed" and not args.force:
            diffs, _ = compare(profile_facts(existing), f)
            if diffs:
                raise InputError(f"{args.out} is confirmed and the facts differ; run --check, "
                                 "then use --force only with the owner's agreement")
        text = render(doc, inv)
        if args.out:
            tmp = args.out + ".tmp"
            with open(tmp, "w", encoding="utf-8", newline="\n") as fh:
                fh.write(text)
            os.replace(tmp, args.out)
            print(f"{args.out}: {len(f['pci'])} PCI functions, {len(f['acpi'])} ACPI tables, "
                  f"status {doc['status']}")
        else:
            sys.stdout.write(text)
        return 0
    except (InputError, OSError) as e:
        print(f"profile.py: error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
