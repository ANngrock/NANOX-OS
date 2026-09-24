"""M4 checks of the persistent store on the host side (docs/m4-store.md §7).

Pure functions over serial markers and the data-disk image (no QEMU), used
by the harness and unit-tested in tests/host/test_storecheck.py:

  - expectations on the data disk after a boot (expect.store);
  - the crash sweep: parsing of the reference run, the state model of the
    workload, and the verdict of one crash point.

The state of the store is read with tools/store/nxstore.py, the host-side
implementation of the format that shares no code with lib/store.c.
"""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "store"))
import nxstore  # noqa: E402

USER_RE = re.compile(r"^NANOX: USER core#\d+: (.*)$")
IO_RE = re.compile(r"^NANOX: m4 io (\d+) (write|flush)(?: blk=(\d+) n=(\d+))? pending=(\d+)")
CRASH_RE = re.compile(r"^NANOX: CRASH POINT io=(\d+) op=(\w+) pending=(\d+) policy=(\w+) "
                      r"persisted=(\d+) torn=(\d)")
SAVED_RE = re.compile(r"^store saved gen=(\d+) label=(\S+)")
BEGIN_RE = re.compile(r"^store commit begin gen=(\d+) label=(\S+)")
MOUNTED_RE = re.compile(r"^store mounted gen=(\d+) label=(\S+)")
STEP_RE = re.compile(r"^workload step \d+: (REQ .*)$")
FINAL_STATES = ("SUCCEEDED", "FAILED", "OUTCOME_UNKNOWN", "CANCELLED")


def user_lines(markers):
    out = []
    for m in markers:
        mm = USER_RE.match(m)
        if mm:
            out.append(mm.group(1))
    return out


# ---------------------------------------------------------------------------
# expect.store: the data disk after a boot

def store_state(image_bytes):
    """(dump, state or None) of a data-disk image."""
    img = nxstore.Image(image_bytes)
    d = nxstore.dump(img)
    return d, d.get("state")


def check_store(expect, image_bytes):
    """expect = {"mounted": bool, "check_ok": bool, "gen_min": n, "gen": n,
    "label": s, "config": {k: v or None}, "blobs": {name: size or None},
    "pins": {name: gen or None}, "tasks": {id: state}, "slot_states": [a, b]}.
    Returns a list of problems (empty: met)."""
    d, st = store_state(image_bytes)
    p = []
    if "mounted" in expect and d["mounted"] != expect["mounted"]:
        p.append("store: mounted=%s, expected %s (slots %s)" % (
            d["mounted"], expect["mounted"], [s["state"] for s in d["slots"]]))
    if "slot_states" in expect:
        have = [s["state"] for s in d["slots"]]
        if have != expect["slot_states"]:
            p.append("store: slot states %s, expected %s" % (have, expect["slot_states"]))
    if not d["mounted"]:
        return p
    if expect.get("check_ok") and d["check"]["problems"]:
        p.append("store: consistency check: %s" % "; ".join(d["check"]["problems"][:3]))
    if "gen_min" in expect and st["gen"] < expect["gen_min"]:
        p.append("store: generation %d < %d" % (st["gen"], expect["gen_min"]))
    if "gen" in expect and st["gen"] != expect["gen"]:
        p.append("store: generation %d, expected %d" % (st["gen"], expect["gen"]))
    if "label" in expect and st["label"] != expect["label"]:
        p.append("store: label %r, expected %r" % (st["label"], expect["label"]))
    for k, v in expect.get("config", {}).items():
        if st["config"].get(k) != v:
            p.append("store: cfg/%s = %r, expected %r" % (k, st["config"].get(k), v))
    for k, v in expect.get("blobs", {}).items():
        have = st["blobs"].get(k)
        if v is None and have is not None:
            p.append("store: blob/%s present, expected absent" % k)
        elif v is not None and (not have or have["size"] != v or not have["ok"]):
            p.append("store: blob/%s = %r, expected size %d with valid content" % (k, have, v))
    for k, v in expect.get("pins", {}).items():
        if st["pins"].get(k) != v:
            p.append("store: pin %s = %r, expected %r" % (k, st["pins"].get(k), v))
    tasks = {t["id"]: t["state"] for t in (st["tasks"] or [])}
    for k, v in expect.get("tasks", {}).items():
        if tasks.get(k) != v:
            p.append("store: task record %s = %r, expected %r" % (k, tasks.get(k), v))
    return p


# ---------------------------------------------------------------------------
# Crash sweep

def reached_store(markers):
    """True if the boot got as far as the M4 controller (a boot that failed
    earlier, e.g. in a self-test of M1, says nothing about the store)."""
    return any(m.startswith("NANOX: m4 boot_id=") for m in markers)


def parse_run(markers):
    """What a workload run printed: io operations (with the number of
    unflushed writes before each), commits begun and saved, the workload
    request lines, and the crash point if there was one."""
    ops, crash = [], None
    for m in markers:
        mm = IO_RE.match(m)
        if mm:
            ops.append({"n": int(mm.group(1)), "op": mm.group(2),
                        "blk": int(mm.group(3)) if mm.group(3) else None,
                        "count": int(mm.group(4)) if mm.group(4) else None,
                        "pending": int(mm.group(5)), "line": m})
            continue
        mm = CRASH_RE.match(m)
        if mm:
            crash = {"io": int(mm.group(1)), "op": mm.group(2), "pending": int(mm.group(3)),
                     "policy": mm.group(4), "persisted": int(mm.group(5)),
                     "torn": mm.group(6) == "1"}
    saved, begun, steps, mounted = [], [], [], None
    for u in user_lines(markers):
        for rx, dest in ((SAVED_RE, saved), (BEGIN_RE, begun)):
            mm = rx.match(u)
            if mm:
                dest.append((int(mm.group(1)), mm.group(2)))
        mm = STEP_RE.match(u)
        if mm:
            steps.append(mm.group(1))
        mm = MOUNTED_RE.match(u)
        if mm and mounted is None:
            mounted = (int(mm.group(1)), mm.group(2))
    return {"ops": ops, "crash": crash, "saved": saved, "begun": begun, "steps": steps,
            "mounted": mounted}


def policies_for(pending):
    """Persistence policies worth running at a crash point: with nothing
    unflushed they all give the same disk."""
    if pending == 0:
        return ["all"]
    if pending == 1:
        return ["all", "none", "torn"]
    return ["all", "none", "torn", "reorder"]


def empty_state():
    return {"config": {}, "blobs": {}, "pins": {}, "tasks": []}


def _copy(st):
    return {"config": dict(st["config"]), "blobs": dict(st["blobs"]), "pins": dict(st["pins"]),
            "tasks": [list(t) for t in st["tasks"]]}


def _args(req):
    parts = req.split()
    return parts[1], parts[2], dict(p.split("=", 1) for p in parts[3:])


def model_states(steps, saved, format_label="format"):
    """State after every saved generation of the reference run.

    steps: the workload request lines ("REQ <id> <op> k=v ..."), saved: the
    (gen, label) pairs in order.  A label is "<id>" (a store change and its
    task record in one commit), "<id>:s" (write-ahead record, RUNNING) or
    "<id>:d" (final record of a task.spawn/terminate).  Returns
    {label: state} including the empty store under `format_label`."""
    reqs = {}
    for line in steps:
        rid, op, args = _args(line)
        reqs[rid] = (op, args)
    states = {format_label: empty_state()}
    cur = empty_state()
    for gen, label in saved:
        rid, _, phase = label.partition(":")
        op, args = reqs[rid]
        cur = _copy(cur)
        if phase == "s":
            cur["tasks"].append([rid, op, "RUNNING"])
        elif phase == "d":
            for t in cur["tasks"]:
                if t[0] == rid:
                    t[2] = "SUCCEEDED"
        else:
            if op == "config.set":
                cur["config"][args["key"]] = args["value"]
            elif op == "config.delete":
                cur["config"].pop(args["key"], None)
            elif op == "blob.put":
                cur["blobs"][args["name"]] = int(args["size"])
            elif op == "blob.delete":
                cur["blobs"].pop(args["name"], None)
            elif op == "history.pin":
                cur["pins"][args["name"]] = int(args["gen"])
            elif op == "history.unpin":
                cur["pins"].pop(args["name"], None)
            cur["tasks"].append([rid, op, "SUCCEEDED"])
        states[label] = cur
    return states


def after_recovery(st):
    """What bin/core makes of a state at start: interrupted records become
    OUTCOME_UNKNOWN."""
    out = _copy(st)
    for t in out["tasks"]:
        if t[2] not in FINAL_STATES:
            t[2] = "OUTCOME_UNKNOWN"
    return out


def guest_state(markers):
    """The state bin/core printed in check mode ("m4 state ..." lines)."""
    st = empty_state()
    st.update({"gen": None, "label": None, "fsck": None, "blobs_ok": True, "unmountable": False})
    for u in user_lines(markers):
        if u.startswith("m4 state unmountable"):
            st["unmountable"] = True
        m = re.match(r"^m4 state gen=(\d+) label=(\S+)", u)
        if m:
            st["gen"], st["label"] = int(m.group(1)), m.group(2)
        m = re.match(r"^m4 state cfg ([^=\s]+)=(\S*) version=\d+$", u)
        if m:
            st["config"][m.group(1)] = m.group(2)
        m = re.match(r"^m4 state blob (\S+) size=(\d+) ok=(yes|no)$", u)
        if m:
            st["blobs"][m.group(1)] = int(m.group(2))
            st["blobs_ok"] &= m.group(3) == "yes"
        m = re.match(r"^m4 state pin ([^=\s]+)=(\d+)$", u)
        if m:
            st["pins"][m.group(1)] = int(m.group(2))
        m = re.match(r"^m4 state task (\S+) (\S+) (\S+)$", u)
        if m:
            st["tasks"].append([m.group(1), m.group(2), m.group(3)])
        m = re.match(r"^m4 fsck (ok|FAIL)", u)
        if m:
            st["fsck"] = m.group(1) == "ok"
    return st


def reader_state(image_bytes):
    """The same view from the host-side reader."""
    d, st = store_state(image_bytes)
    if not d["mounted"]:
        return None, d
    out = empty_state()
    out.update({"gen": st["gen"], "label": st["label"], "config": dict(st["config"]),
                "blobs": {k: v["size"] for k, v in st["blobs"].items()},
                "blobs_ok": all(v["ok"] for v in st["blobs"].values()),
                "pins": dict(st["pins"]),
                "tasks": [[t["id"], t["op"], t["state"]] for t in (st["tasks"] or [])],
                "fsck": not d["check"]["problems"]})
    return out, d


def content(st):
    return {k: st[k] for k in ("config", "blobs", "pins", "tasks")}


def evaluate_point(ref, crash_run, check_run, check_verdict, reader, point):
    """Verdict of one crash point.

    ref: parse_run of the reference run plus "models" (model_states);
    crash_run / check_run: parse_run of the two boots; check_verdict: the
    harness verdict of the check boot; reader: (state, dump) of the disk
    after the check boot; point: {"k", "policy"}.

    Returns {"violations": [...], "errors": [...], facts...}: a violation
    is a broken durability or consistency promise, an error means the
    point could not be evaluated (the test itself is broken)."""
    v, e = [], []
    k = point["k"]
    out = {"k": k, "policy": point["policy"]}
    crash = crash_run["crash"]
    if not crash or crash["io"] != k:
        e.append("crash boot did not stop at io=%d (%s)" % (k, crash))
        out.update(violations=v, errors=e)
        return out
    ref_prefix = [o["line"] for o in ref["ops"][:k - 1]]
    have_prefix = [o["line"] for o in crash_run["ops"]]
    if have_prefix != ref_prefix:
        e.append("io operations before the crash point differ from the reference run")
    acked = crash_run["saved"][-1] if crash_run["saved"] else (1, "format")
    attempted = crash_run["begun"][-1] if crash_run["begun"] else acked
    if attempted[0] < acked[0]:
        attempted = acked
    out.update(acked_gen=acked[0], acked_label=acked[1], attempted_gen=attempted[0],
               attempted_label=attempted[1], pending=crash["pending"],
               persisted=crash["persisted"], torn=crash["torn"])
    gstate = guest_state(check_run.get("markers", []))
    mounted = check_run.get("mounted")
    if check_verdict != "PASS" or mounted is None:
        if not reached_store(check_run.get("markers", [])):
            e.append("check boot ended (%s) before the M4 controller started" % check_verdict)
        elif gstate["unmountable"]:
            v.append("unmountable: the check boot found no valid generation")
        else:
            v.append("check boot verdict %s" % check_verdict)
        out.update(violations=v, errors=e)
        return out
    g, label = mounted
    out.update(recovered_gen=g, recovered_label=label)
    if g < acked[0]:
        v.append("saved_lost: recovered generation %d (%s) < reported saved %d (%s)" % (
            g, label, acked[0], acked[1]))
    if g > attempted[0]:
        v.append("generation %d beyond the last commit begun (%d)" % (g, attempted[0]))
    model = ref["models"].get(label)
    if model is None:
        e.append("no model for label %r" % label)
    else:
        want = after_recovery(model)
        if content(gstate) != want:
            v.append("content_mismatch (guest): %s != model of %s %s" % (
                content(gstate), label, want))
        rstate, _ = reader
        if rstate is None:
            v.append("unmountable (host reader) after the check boot")
        else:
            if content(rstate) != want:
                v.append("content_mismatch (host reader): %s != model of %s %s" % (
                    content(rstate), label, want))
            if not rstate["fsck"]:
                v.append("host reader consistency check failed")
            if not rstate["blobs_ok"]:
                v.append("blob content wrong (host reader)")
        if not gstate["fsck"]:
            v.append("guest consistency check failed")
        if not gstate["blobs_ok"]:
            v.append("blob content wrong (guest)")
    out.update(violations=v, errors=e)
    return out
