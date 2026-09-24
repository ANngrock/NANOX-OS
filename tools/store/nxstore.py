#!/usr/bin/env python3
"""nxstore v1 -- independent host-side implementation (M4).

The store itself is lib/store.c; this module re-implements, from the format
description in docs/m4-store.md and without sharing code, what the host
needs: writing an empty store (the data disk image of the build), reading
and choosing the current generation the way the recovery rules prescribe,
a consistency check, a JSON dump, decoding of the objects bin/core keeps
(configuration, blobs, task-engine records), and deliberate damage for the
recovery scenarios.  Only the Python standard library.

  nxstore.py format --out IMG [--blocks N] [--retain K] [--store-id HEX]
  nxstore.py dump IMG               JSON: slots, current generation, objects, history
  nxstore.py check IMG              consistency check; exit 1 on problems
  nxstore.py corrupt IMG --what current-root|current-super|both-supers|current-data
"""

import argparse
import json
import struct
import sys
import zlib

BLOCK = 4096
VERSION = 1
SUPER_MAGIC = b"NXSTSUP1"
ROOT_MAGIC = b"NXSTROOT"
FIRST_DATA = 2
MIN_BLOCKS = 16
MAX_BLOCKS = 32768
HIST_MAX, PIN_MAX, LOG_MAX, OBJ_MAX = 8, 4, 16, 49
RETAIN_MIN, RETAIN_MAX = 2, HIST_MAX + 1
OBJ_MAX_BYTES = 8 * BLOCK
NAME_CHARS = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-/")
PIN_CHARS = set("abcdefghijklmnopqrstuvwxyz0123456789_-")

# Layout (little endian), docs/m4-store.md §2.
SUPER_FMT = "<8sIIQQQIIIIQ"          # 64 bytes, then zeros, CRC at 4092
ROOT_HDR_FMT = "<8sQQQQQIIIIII16s40x"  # 128 bytes
REF_FMT = "<QII"                      # 16
PIN_FMT = "<16sQII"                   # 32
LOG_FMT = "<QQ16s"                    # 32
OBJ_FMT = "<QQIIIIHH28s"              # 64
assert struct.calcsize(SUPER_FMT) == 64
assert struct.calcsize(ROOT_HDR_FMT) == 128
assert struct.calcsize(OBJ_FMT) == 64
OFF_HIST, OFF_PIN, OFF_LOG, OFF_OBJ = 128, 256, 384, 896

KIND_CONFIG, KIND_BLOB, KIND_TASKS = 1, 2, 3
TASKS_MAGIC = b"NXTASKS1"
TASK_REC_FMT = "<33s33sBBIQQ232s"     # struct nx_m4_task_rec, 320 bytes
assert struct.calcsize(TASK_REC_FMT) == 320
# Engine states (user/core/engine.h).
ACT_STATES = ["NONE", "CREATED", "OBSERVING", "PLANNED", "RUNNING", "VERIFYING", "SUCCEEDED",
              "FAILED", "OUTCOME_UNKNOWN", "CANCELLED"]


def crc32(data):
    return zlib.crc32(data) & 0xFFFFFFFF


def block_crc(blk):
    return crc32(blk[:BLOCK - 4])


def cstr(b):
    return b.split(b"\0", 1)[0].decode("ascii", "replace")


def cstr_ok(b):
    """NUL-terminated within the field."""
    return b"\0" in b


# ---------------------------------------------------------------------------
# Writing (the empty store of the build)

def pack_super(gen, root_blk, root_crc, blocks, store_id, retain, boot_id):
    head = struct.pack(SUPER_FMT, SUPER_MAGIC, VERSION, BLOCK, blocks, store_id, gen, root_blk,
                       root_crc, retain, 0, boot_id)
    blk = head + bytes(BLOCK - 64 - 4)
    return blk + struct.pack("<I", crc32(blk))


def pack_empty_root(store_id, retain, boot_id):
    hdr = struct.pack(ROOT_HDR_FMT, ROOT_MAGIC, 1, 0, store_id, 1, boot_id, 0, 0, 0, 1, retain, 0,
                      b"format")
    body = bytearray(BLOCK)
    body[:128] = hdr
    body[OFF_LOG:OFF_LOG + 32] = struct.pack(LOG_FMT, 1, boot_id, b"format")
    body[BLOCK - 4:] = struct.pack("<I", block_crc(bytes(body)))
    return bytes(body)


def format_image(blocks=256, retain=4, store_id=0x4E414E4F58303034, boot_id=0):
    """Bytes of a freshly formatted store: generation 1, no objects, in slot A."""
    if not MIN_BLOCKS <= blocks <= MAX_BLOCKS or not RETAIN_MIN <= retain <= RETAIN_MAX:
        raise ValueError("bad size or retention")
    root = pack_empty_root(store_id, retain, boot_id)
    img = bytearray(blocks * BLOCK)
    img[FIRST_DATA * BLOCK:(FIRST_DATA + 1) * BLOCK] = root
    img[0:BLOCK] = pack_super(1, FIRST_DATA, struct.unpack_from("<I", root, BLOCK - 4)[0],
                              blocks, store_id, retain, boot_id)
    return bytes(img)


# ---------------------------------------------------------------------------
# Reading

class Image:
    def __init__(self, data):
        self.data = bytes(data)
        self.blocks = len(self.data) // BLOCK

    def block(self, i):
        if not 0 <= i < self.blocks:
            raise IndexError("block %d outside the image" % i)
        return self.data[i * BLOCK:(i + 1) * BLOCK]


def parse_super(blk, dev_blocks):
    if blk[:8] != SUPER_MAGIC:
        return {"state": "empty"}
    if block_crc(blk) != struct.unpack_from("<I", blk, BLOCK - 4)[0]:
        return {"state": "bad_crc"}
    (_, version, bsize, blocks, store_id, gen, root_blk, root_crc, retain, _r,
     boot_id) = struct.unpack_from(SUPER_FMT, blk)
    sb = {"state": "candidate", "version": version, "block_size": bsize, "blocks": blocks,
          "store_id": store_id, "gen": gen, "root_blk": root_blk, "root_crc": root_crc,
          "retain": retain, "boot_id": boot_id}
    if version != VERSION or bsize != BLOCK or gen == 0 or not MIN_BLOCKS <= blocks <= dev_blocks:
        sb["state"] = "bad_header"
    return sb


def parse_root(blk):
    (magic, gen, parent, store_id, next_oid, boot_id, nobj, nhist, npin, nlog, retain, flags,
     label) = struct.unpack_from(ROOT_HDR_FMT, blk)
    r = {"magic": magic, "gen": gen, "parent_gen": parent, "store_id": store_id,
         "next_oid": next_oid, "boot_id": boot_id, "nobj": nobj, "nhist": nhist, "npin": npin,
         "nlog": nlog, "retain": retain, "flags": flags, "label_raw": label,
         "label": cstr(label), "crc": struct.unpack_from("<I", blk, BLOCK - 4)[0],
         "crc_ok": block_crc(blk) == struct.unpack_from("<I", blk, BLOCK - 4)[0]}
    r["hist"] = [dict(zip(("gen", "blk", "crc"), struct.unpack_from(REF_FMT, blk, OFF_HIST + 16 * i)))
                 for i in range(min(nhist, HIST_MAX))]
    r["pins"] = []
    for i in range(min(npin, PIN_MAX)):
        name, g, b, c = struct.unpack_from(PIN_FMT, blk, OFF_PIN + 32 * i)
        r["pins"].append({"name": cstr(name), "name_raw": name, "gen": g, "blk": b, "crc": c})
    r["log"] = []
    for i in range(min(nlog, LOG_MAX)):
        g, boot, lab = struct.unpack_from(LOG_FMT, blk, OFF_LOG + 32 * i)
        r["log"].append({"gen": g, "boot_id": boot, "label": cstr(lab)})
    r["objects"] = []
    for i in range(min(nobj, OBJ_MAX)):
        oid, mod_gen, version, b, length, c, kind, nblk, name = struct.unpack_from(
            OBJ_FMT, blk, OFF_OBJ + 64 * i)
        r["objects"].append({"oid": oid, "mod_gen": mod_gen, "version": version, "blk": b,
                             "len": length, "crc": c, "kind": kind, "nblk": nblk,
                             "name": cstr(name), "name_raw": name})
    return r


def root_problems(r, dev_blocks):
    """Structural rules of docs/m4-store.md §2 (not the checksum)."""
    p = []
    if r["magic"] != ROOT_MAGIC:
        p.append("magic")
    if r["gen"] == 0 or r["parent_gen"] >= r["gen"]:
        p.append("generation numbers")
    if r["nobj"] > OBJ_MAX or r["nhist"] > HIST_MAX or r["npin"] > PIN_MAX:
        p.append("table counts")
    if not 1 <= r["nlog"] <= LOG_MAX or not r["log"] or r["log"][0]["gen"] != r["gen"]:
        p.append("commit log")
    if not RETAIN_MIN <= r["retain"] <= RETAIN_MAX or r["next_oid"] == 0:
        p.append("retain/next_oid")
    if not cstr_ok(r["label_raw"]):
        p.append("label")
    prev = r["gen"]
    for h in r["hist"]:
        if not 0 < h["gen"] < prev or not FIRST_DATA <= h["blk"] < dev_blocks:
            p.append("history ref gen %d" % h["gen"])
        prev = h["gen"]
    names = set()
    for pin in r["pins"]:
        if (not cstr_ok(pin["name_raw"]) or not pin["name"] or len(pin["name"]) > 15 or
                set(pin["name"]) - PIN_CHARS or pin["name"] in names or
                not 0 < pin["gen"] < r["gen"] or not FIRST_DATA <= pin["blk"] < dev_blocks):
            p.append("pin %r" % pin["name"])
        names.add(pin["name"])
    names, oids, extents = set(), set(), []
    for o in r["objects"]:
        ok = (cstr_ok(o["name_raw"]) and o["name"] and len(o["name"]) <= 27 and
              not set(o["name"]) - NAME_CHARS and o["name"] not in names and
              0 < o["oid"] < r["next_oid"] and o["oid"] not in oids and o["version"] > 0 and
              0 < o["mod_gen"] <= r["gen"] and o["len"] <= OBJ_MAX_BYTES and
              o["nblk"] == (o["len"] + BLOCK - 1) // BLOCK)
        if ok and o["nblk"]:
            ok = FIRST_DATA <= o["blk"] and o["blk"] + o["nblk"] <= dev_blocks
            for (b, n) in extents:
                if o["blk"] < b + n and b < o["blk"] + o["nblk"]:
                    ok = False
            extents.append((o["blk"], o["nblk"]))
        elif ok:
            ok = o["blk"] == 0
        if not ok:
            p.append("object %r" % o["name"])
        names.add(o["name"])
        oids.add(o["oid"])
    return p


def object_data(img, o):
    data = b"".join(img.block(o["blk"] + i) for i in range(o["nblk"]))[:o["len"]]
    return data, crc32(data) == o["crc"]


def load_root(img, blk, gen=None, crc=None, store_blocks=None):
    """Returns (root, None) or (None, reason)."""
    store_blocks = store_blocks or img.blocks
    if not FIRST_DATA <= blk < store_blocks:
        return None, "root block out of range"
    raw = img.block(blk)
    r = parse_root(raw)
    if not r["crc_ok"]:
        return None, "root checksum"
    if crc is not None and r["crc"] != crc:
        return None, "root checksum differs from the reference"
    if gen is not None and r["gen"] != gen:
        return None, "root generation differs from the reference"
    probs = root_problems(r, store_blocks)
    if probs:
        return None, "root structure: " + ", ".join(probs)
    r["blk"] = blk
    return r, None


def mount(img):
    """Recovery rules of docs/m4-store.md §5: the newest slot whose
    superblock, root and every data extent verify."""
    slots = [parse_super(img.block(i), img.blocks) for i in range(2)]
    max_gen = max([s["gen"] for s in slots if s["state"] in ("candidate", "bad_header")] or [0])
    order = sorted([i for i in range(2) if slots[i]["state"] == "candidate"],
                   key=lambda i: (-slots[i]["gen"], i))
    result = {"slots": slots, "max_gen": max_gen, "root": None, "slot": None, "rejected": []}
    for i in order:
        sb = slots[i]
        if result["root"] is not None:
            sb["state"] = "older"
            continue
        r, why = load_root(img, sb["root_blk"], sb["gen"], sb["root_crc"], sb["blocks"])
        if r and r["store_id"] != sb["store_id"]:
            r, why = None, "store id"
        if r is None:
            sb["state"] = "bad_root"
            sb["reason"] = why
            result["rejected"].append({"slot": i, "gen": sb["gen"], "state": "bad_root",
                                       "reason": why})
            continue
        bad = [o["name"] for o in r["objects"] if not object_data(img, o)[1]]
        if bad:
            sb["state"] = "bad_data"
            sb["reason"] = "extent checksum: %s" % ",".join(bad)
            result["rejected"].append({"slot": i, "gen": sb["gen"], "state": "bad_data",
                                       "reason": sb["reason"]})
            continue
        sb["state"] = "current"
        result["root"], result["slot"] = r, i
        result["store_blocks"] = sb["blocks"]
    return result


def retained_refs(root):
    refs = [{"gen": root["gen"], "blk": root["blk"], "crc": root["crc"], "why": "current"}]
    for h in root["hist"]:
        refs.append(dict(h, why="history"))
    for p in root["pins"]:
        refs.append({"gen": p["gen"], "blk": p["blk"], "crc": p["crc"], "why": "pin:" + p["name"]})
    return refs


def check(img):
    """Consistency check of every retained generation.  Returns (report, problems)."""
    m = mount(img)
    problems = []
    if m["root"] is None:
        return {"mounted": False, "slots": m["slots"]}, ["no valid generation"]
    blocks = m["store_blocks"]
    extents = {}   # (blk, nblk, crc, oid) -> owners
    seen = set()
    roots = 0
    for ref in retained_refs(m["root"]):
        if (ref["gen"], ref["blk"]) in seen:
            continue
        seen.add((ref["gen"], ref["blk"]))
        r, why = load_root(img, ref["blk"], ref["gen"], ref["crc"], blocks)
        if r is None:
            problems.append("retained generation %d: %s" % (ref["gen"], why))
            continue
        roots += 1
        extents.setdefault((ref["blk"], 1, ref["crc"], 0), []).append(ref["gen"])
        for o in r["objects"]:
            if not object_data(img, o)[1]:
                problems.append("generation %d object %s: checksum" % (ref["gen"], o["name"]))
            if o["nblk"]:
                extents.setdefault((o["blk"], o["nblk"], o["crc"], o["oid"]), []).append(ref["gen"])
    keys = sorted(extents)
    used = set()
    for i, a in enumerate(keys):
        for b in keys[i + 1:]:
            if a[0] < b[0] + b[1] and b[0] < a[0] + a[1]:
                problems.append("extents overlap: blocks %d+%d and %d+%d" % (a[0], a[1], b[0], b[1]))
        used.update(range(a[0], a[0] + a[1]))
    report = {"mounted": True, "gen": m["root"]["gen"], "slot": m["slot"], "roots": roots,
              "extents": len(keys), "used_blocks": len(used),
              "free_blocks": blocks - FIRST_DATA - len(used), "blocks": blocks}
    return report, problems


def decode_tasks(data):
    if len(data) < 16 or data[:8] != TASKS_MAGIC:
        return None
    count, rec_size = struct.unpack_from("<II", data, 8)
    if rec_size != 320 or 16 + count * rec_size > len(data):
        return None
    recs = []
    for i in range(count):
        rid, op, state, flags, res_len, fp, boot, res = struct.unpack_from(
            TASK_REC_FMT, data, 16 + i * rec_size)
        recs.append({"id": cstr(rid), "op": cstr(op),
                     "state": ACT_STATES[state] if state < len(ACT_STATES) else str(state),
                     "flags": flags, "fp": "%016x" % fp, "boot": "%016x" % boot,
                     "res": res[:min(res_len, 232)].decode("ascii", "replace")})
    return recs


def blob_byte(name_hash, size, i):
    x = (name_hash ^ ((size * 2654435761) & 0xFFFFFFFF) ^ ((i * 40503) & 0xFFFFFFFF)) & 0xFFFFFFFF
    x ^= x >> 15
    x = (x * 0x2C1B3C6D) & 0xFFFFFFFF
    x ^= x >> 12
    return x & 0xFF


def name_hash(s):
    h = 2166136261
    for c in s.encode():
        h = ((h ^ c) * 16777619) & 0xFFFFFFFF
    return h


def blob_ok(name, data):
    h = name_hash(name)
    return all(data[i] == blob_byte(h, len(data), i) for i in range(len(data)))


def state_of(img, root):
    """What bin/core keeps, decoded: config, blobs, task records."""
    st = {"gen": root["gen"], "label": root["label"], "config": {}, "blobs": {}, "tasks": None,
          "pins": {p["name"]: p["gen"] for p in root["pins"]},
          "history": [h["gen"] for h in root["hist"]]}
    for o in root["objects"]:
        data, ok = object_data(img, o)
        if o["name"].startswith("cfg/") and o["kind"] == KIND_CONFIG:
            st["config"][o["name"][4:]] = data.decode("ascii", "replace") if ok else None
        elif o["name"].startswith("blob/") and o["kind"] == KIND_BLOB:
            st["blobs"][o["name"][5:]] = {"size": len(data), "ok": ok and blob_ok(o["name"][5:], data)}
        elif o["name"] == "core/tasks" and o["kind"] == KIND_TASKS:
            st["tasks"] = decode_tasks(data) if ok else None
    return st


def dump(img):
    m = mount(img)
    out = {"schema": "nanox.store-dump.v1", "image_blocks": img.blocks, "max_gen": m["max_gen"],
           "slots": [{k: v for k, v in s.items()} for s in m["slots"]],
           "rejected": m["rejected"], "mounted": m["root"] is not None}
    if m["root"] is not None:
        r = m["root"]
        out["slot"] = m["slot"]
        out["root"] = {k: v for k, v in r.items() if not k.endswith("_raw") and k != "magic"
                       and k != "objects" and k != "pins"}
        out["root"]["pins"] = [{k: v for k, v in p.items() if k != "name_raw"} for p in r["pins"]]
        out["objects"] = [{k: v for k, v in o.items() if k != "name_raw"} for o in r["objects"]]
        out["state"] = state_of(img, r)
        rep, problems = check(img)
        out["check"] = dict(rep, problems=problems)
    return out


# ---------------------------------------------------------------------------
# Damage for the recovery scenarios

def corrupt(data, what):
    """Returns (new bytes, description)."""
    img = Image(data)
    m = mount(img)
    buf = bytearray(data)

    def flip(blk, off):
        buf[blk * BLOCK + off] ^= 0x40

    if what == "both-supers":
        flip(0, 100)
        flip(1, 100)
        return bytes(buf), "flipped a byte in both superblocks"
    if m["root"] is None:
        raise ValueError("no current generation to damage")
    r = m["root"]
    if what == "current-root":
        flip(r["blk"], 1000)
        return bytes(buf), "flipped a byte of the root block %d of generation %d" % (r["blk"], r["gen"])
    if what == "current-super":
        flip(m["slot"], 100)
        return bytes(buf), "flipped a byte of superblock slot %d (generation %d)" % (m["slot"], r["gen"])
    if what == "current-data":
        objs = [o for o in r["objects"] if o["nblk"]]
        if not objs:
            raise ValueError("the current generation has no data extent")
        flip(objs[0]["blk"], 5)
        return bytes(buf), "flipped a byte of object %s (block %d)" % (objs[0]["name"], objs[0]["blk"])
    raise ValueError("unknown damage %r" % what)


# ---------------------------------------------------------------------------

def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("format")
    f.add_argument("--out", required=True)
    f.add_argument("--blocks", type=int, default=256)
    f.add_argument("--retain", type=int, default=4)
    f.add_argument("--store-id", type=lambda s: int(s, 16), default=0x4E414E4F58303034)
    d = sub.add_parser("dump")
    d.add_argument("image")
    c = sub.add_parser("check")
    c.add_argument("image")
    k = sub.add_parser("corrupt")
    k.add_argument("image")
    k.add_argument("--what", required=True,
                   choices=["current-root", "current-super", "both-supers", "current-data"])
    args = ap.parse_args(argv)
    if args.cmd == "format":
        data = format_image(args.blocks, args.retain, args.store_id)
        tmp = args.out + ".tmp"
        with open(tmp, "wb") as fh:
            fh.write(data)
        import os
        os.replace(tmp, args.out)
        return 0
    with open(args.image, "rb") as fh:
        data = fh.read()
    if args.cmd == "dump":
        print(json.dumps(dump(Image(data)), indent=2, default=lambda b: b.hex()))
        return 0
    if args.cmd == "check":
        rep, problems = check(Image(data))
        print(json.dumps(dict(rep, problems=problems), indent=2, default=str))
        return 1 if problems else 0
    new, what = corrupt(data, args.what)
    with open(args.image, "wb") as fh:
        fh.write(new)
    print(what)
    return 0


if __name__ == "__main__":
    sys.exit(main())
