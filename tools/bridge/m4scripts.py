"""Host-side scripts of the M4 scenarios (docs/m4-store.md §8).

Each scenario of several boots on one data disk runs one script per boot;
ctx["carry"] passes what the host learned in an earlier boot (boot id,
generations, references) to the next one.  Request ids carry a per-boot
prefix: with the store, request ids are deduplication keys across boots.

  persist-1 / persist-2      criterion 2: configuration, task records and
                             history survive a restart; a request of the
                             earlier boot is answered from its record
  corrupt-1 / corrupt-2      criterion 3: damaged current root -> fallback
                             to the previous generation, reported
  unmountable-1 / -2         criterion 3: both superblocks damaged -> the
                             store is refused, the executor still serves
  full-1 / full-2            criterion 3: full store -> NO_SPACE without
                             effects, release of space, consistent restart
  retention                  criterion 4: retention window, pins, prune
"""

import nci  # noqa: F401  (BridgeError is raised through the client)


class Checks:
    def __init__(self):
        self.problems = []

    def expect(self, cond, code, msg):
        if not cond:
            self.problems.append("%s: %s" % (code, msg))
        return cond


def describe(r):
    return "%s %s" % (r.state, " ".join("%s=%s" % kv for kv in sorted(r.fields.items())))


def alive(client, name, prefix):
    lst = client.call("task.list", request_id=prefix + "-list-%d" % client.counter)
    client.counter += 1
    return [i for i in lst.items if i.get("name") == name and
            i.get("state") in ("ready", "running", "blocked")]


class Caller:
    """client.call with request ids "<prefix>-<n>" for read-only steps."""

    def __init__(self, client, prefix):
        self.client, self.prefix, self.n = client, prefix, 0

    def __call__(self, op, request_id=None, **args):
        if request_id is None:
            self.n += 1
            request_id = "%s-%d" % (self.prefix, self.n)
        return self.client.call(op, request_id=request_id, **args)


# ---- persistence across a restart ----------------------------------------------

def run_persist_1(ctx):
    c, call, hello, carry = Checks(), Caller(ctx["client"], "p1"), ctx["hello"], ctx["carry"]
    carry["boot1"] = hello["boot"]
    st = call("store.status")
    c.expect(st.ok and st.get("state") == "mounted" and st.get("gen") == "1" and
             st.get("fallback") == "no", "status", "fresh store: %s" % describe(st))
    a = call("config.set", "p1-cfg-owner", key="owner", value="nanox")
    b = call("config.set", "p1-cfg-mode", key="mode", value="alpha")
    carry["gen_alpha"] = b.get("gen")
    pin = call("history.pin", "p1-pin", name="before", gen=carry["gen_alpha"] or "0")
    c.expect(pin.ok, "pin", describe(pin))
    b2 = call("config.set", "p1-cfg-mode2", key="mode", value="beta")
    for r in (a, b, b2):
        c.expect(r.ok and r.get("saved") == "yes" and r.get("verify") == "ok", "config_set",
                 describe(r))
    c.expect(b2.get("version") == "2", "config_version", "second set of mode: %s" % describe(b2))
    sp = call("task.spawn", "p1-spawn", program="load")
    c.expect(sp.ok and sp.get("saved") == "yes", "spawn", describe(sp))
    carry["ref1"] = sp.get("ref")
    tm = call("task.terminate", "p1-kill", target=sp.get("ref") or "task/0/0")
    c.expect(tm.ok and tm.get("saved") == "yes", "terminate", describe(tm))
    bl = call("blob.put", "p1-blob", name="data", size="20000")
    c.expect(bl.ok and bl.get("blocks") == "5", "blob_put", describe(bl))
    hist = call("history.list")
    labels = [i.get("label") for i in hist.items]
    for want in ("p1-cfg-owner", "p1-spawn:s", "p1-spawn:d", "p1-kill:d", "p1-pin"):
        c.expect(want in labels, "history", "commit %s missing from history %s" % (want, labels))
    chk = call("store.check")
    c.expect(chk.ok and chk.get("verify") == "ok", "store_check", describe(chk))
    st = call("store.status")
    carry["last_gen"] = st.get("gen")
    return c.problems


def run_persist_2(ctx):
    c, call, hello, carry = Checks(), Caller(ctx["client"], "p2"), ctx["hello"], ctx["carry"]
    client = ctx["client"]
    c.expect(hello["boot"] != carry.get("boot1"), "new_boot", "the boot id did not change")
    st = call("store.status")
    c.expect(st.ok and st.get("state") == "mounted" and st.get("gen") == carry.get("last_gen")
             and st.get("fallback") == "no" and st.get("outcome_unknown") == "0",
             "persist_status", "after the restart: %s (boot 1 ended at %s)" % (
                 describe(st), carry.get("last_gen")))
    c.expect(int(st.get("restored_tasks", "0")) >= 7, "persist_task",
             "restored task records: %s" % st.get("restored_tasks"))
    g = call("config.get", key="owner")
    c.expect(g.ok and g.get("value") == "nanox" and g.get("version") == "1", "persist_config",
             "owner after the restart: %s" % describe(g))
    g = call("config.get", key="mode")
    c.expect(g.ok and g.get("value") == "beta" and g.get("version") == "2", "persist_config",
             "mode after the restart: %s" % describe(g))
    g = call("config.get", key="mode", gen=carry.get("gen_alpha") or "0")
    c.expect(g.ok and g.get("value") == "alpha", "persist_history",
             "mode in the pinned generation %s: %s" % (carry.get("gen_alpha"), describe(g)))
    s = call("action.status", request="p1-spawn")
    c.expect(s.get("known") == "yes" and s.get("state") == "SUCCEEDED" and
             s.get("op") == "task.spawn" and s.get("boot") == carry.get("boot1") and
             s.get("persisted") == "yes" and s.get("restored") == "yes", "persist_task",
             "status of the spawn of boot 1: %s" % describe(s))
    # Repeating the boot-1 request: answered from the record, not executed.
    rep = call("task.spawn", "p1-spawn", program="load")
    c.expect(rep.ok and rep.replayed and rep.get("ref") == carry.get("ref1"), "persist_replay",
             "repeat of p1-spawn: %s" % describe(rep))
    c.expect(not alive(client, "load", "p2"), "persist_replay_executed",
             "a load task runs after repeating the boot-1 request")
    ins = call("task.inspect", target=carry.get("ref1") or "task/0/0")
    c.expect(ins.state == "FAILED" and ins.get("code") == "STALE_REF", "persist_stale",
             "boot-1 task reference: %s" % describe(ins))
    reuse = call("config.set", "p1-cfg-owner", key="owner", value="other")
    c.expect(reuse.state == "REJECTED" and reuse.get("code") == "ID_REUSED", "persist_dedup",
             "boot-1 request id with another request: %s" % describe(reuse))
    hist = call("history.list")
    boot1 = [i for i in hist.items if i.get("boot") == carry.get("boot1")]
    c.expect(len(boot1) >= 8, "persist_history",
             "commits of boot 1 in the history: %d" % len(boot1))
    pinned = [i for i in hist.items if i.get("pin") == "before"]
    c.expect(len(pinned) == 1 and pinned[0].get("gen") == carry.get("gen_alpha") and
             pinned[0].get("content") == "kept", "persist_pin", "pin: %s" % pinned)
    bl = call("blob.check", name="data")
    c.expect(bl.ok and bl.get("size") == "20000", "persist_blob", describe(bl))
    new = call("config.set", "p2-cfg-mode", key="mode", value="gamma")
    c.expect(new.ok and new.get("gen") == str(int(carry.get("last_gen") or 0) + 1),
             "persist_commit", "first commit of boot 2: %s" % describe(new))
    chk = call("store.check")
    c.expect(chk.ok, "store_check", describe(chk))
    return c.problems


# ---- damaged root, damaged superblocks ---------------------------------------------

def run_corrupt_1(ctx):
    c, call, carry = Checks(), Caller(ctx["client"], "c1"), ctx["carry"]
    one = call("config.set", "c1-x1", key="x", value="one")
    two = call("config.set", "c1-x2", key="x", value="two")
    c.expect(one.ok and two.ok, "config_set", "%s / %s" % (describe(one), describe(two)))
    carry["gen_one"], carry["gen_two"] = one.get("gen"), two.get("gen")
    return c.problems


def run_corrupt_2(ctx):
    c, call, carry = Checks(), Caller(ctx["client"], "c2"), ctx["carry"]
    st = call("store.status")
    c.expect(st.ok and st.get("state") == "mounted" and st.get("fallback") == "yes" and
             st.get("rejected_gen") == carry.get("gen_two") and
             st.get("rejected") == "bad_root" and st.get("gen") == carry.get("gen_one"),
             "fallback", "after damaging the root of generation %s: %s" % (
                 carry.get("gen_two"), describe(st)))
    g = call("config.get", key="x")
    c.expect(g.ok and g.get("value") == "one", "fallback_content", describe(g))
    s = call("action.status", request="c1-x2")
    c.expect(s.get("known") == "no", "fallback_record",
             "the record of the lost generation: %s" % describe(s))
    s = call("action.status", request="c1-x1")
    c.expect(s.get("known") == "yes" and s.get("state") == "SUCCEEDED", "fallback_record",
             "the record of the kept generation: %s" % describe(s))
    new = call("config.set", "c2-x3", key="x", value="three")
    c.expect(new.ok and new.get("gen") == str(int(carry.get("gen_two") or 0) + 1), "gen_monotonic",
             "commit after the fallback: %s" % describe(new))
    chk = call("store.check")
    c.expect(chk.ok, "store_check", describe(chk))
    return c.problems


def run_unmountable_1(ctx):
    c, call = Checks(), Caller(ctx["client"], "u1")
    r = call("config.set", "u1-x", key="x", value="one")
    c.expect(r.ok, "config_set", describe(r))
    return c.problems


def run_unmountable_2(ctx):
    c, call = Checks(), Caller(ctx["client"], "u2")
    st = call("store.status")
    c.expect(st.ok and st.get("state") == "unmountable" and st.get("error") == "no_valid_root"
             and st.get("slots") == "bad_crc,bad_crc", "unmountable",
             "both superblocks damaged: %s" % describe(st))
    g = call("config.get", key="x")
    c.expect(g.state == "FAILED" and g.get("code") == "NO_STORE", "no_store", describe(g))
    s = call("config.set", key="x", value="two")
    c.expect(s.state == "FAILED" and s.get("code") == "NO_STORE" and s.get("effects") == "none",
             "no_store", describe(s))
    d = call("system.describe")
    c.expect(d.ok and d.get("store") == "unmountable", "describe", describe(d))
    lst = call("task.list")
    c.expect(lst.ok and any(i.get("name") == "core" for i in lst.items), "serves",
             "the executor serves without the store: %s" % lst.state)
    return c.problems


# ---- full store -------------------------------------------------------------------------

def run_full_1(ctx):
    c, call, carry = Checks(), Caller(ctx["client"], "f1"), ctx["carry"]
    made, fail = [], None
    for i in range(40):
        r = call("blob.put", "f1-put-%d" % i, name="f%d" % i, size="32768")
        if not r.ok:
            fail = r
            break
        made.append("f%d" % i)
    c.expect(fail is not None and fail.state == "FAILED" and fail.get("code") == "NO_SPACE" and
             fail.get("effects") == "none", "full", "after %d blobs: %s" % (
                 len(made), describe(fail) if fail else "no failure"))
    c.expect(len(made) >= 20, "full_early", "only %d blobs of 32 KiB fit" % len(made))
    st = call("store.status")
    last = made[-1] if made else None
    s = call("action.status", request="f1-put-%d" % len(made))
    c.expect(s.get("state") == "FAILED" and s.get("persisted") == "no", "full_record",
             "the failed put is not in the store: %s" % describe(s))
    if last:
        b = call("blob.check", name=last)
        c.expect(b.ok, "full_intact", "last blob after the failure: %s" % describe(b))
    chk = call("store.check")
    c.expect(chk.ok, "full_check", describe(chk))
    gen_full = st.get("gen")
    # Release: delete four blobs (each commit may use the reserve), then
    # two prunes drop the generations that still reference them.
    for i in range(4):
        r = call("blob.delete", "f1-del-%d" % i, name=made[i] if i < len(made) else "none")
        c.expect(r.ok, "release_delete", describe(r))
    for i in range(2):
        r = call("store.prune", "f1-prune-%d" % i)
        c.expect(r.ok, "release_prune", describe(r))
    r = call("blob.put", "f1-again", name="again", size="32768")
    c.expect(r.ok and r.get("saved") == "yes", "release_put", "put after releasing: %s" %
             describe(r))
    chk = call("store.check")
    c.expect(chk.ok, "full_check", describe(chk))
    carry["full_gen"], carry["blobs"] = gen_full, made[4:] + ["again"]
    st = call("store.status")
    carry["last_gen"] = st.get("gen")
    return c.problems


def run_full_2(ctx):
    c, call, carry = Checks(), Caller(ctx["client"], "f2"), ctx["carry"]
    st = call("store.status")
    c.expect(st.ok and st.get("gen") == carry.get("last_gen") and st.get("fallback") == "no",
             "restart", describe(st))
    for name in carry.get("blobs", []):
        b = call("blob.check", name=name)
        c.expect(b.ok, "restart_blob", "%s: %s" % (name, describe(b)))
    chk = call("store.check")
    c.expect(chk.ok, "restart_check", describe(chk))
    return c.problems


# ---- retention policy ---------------------------------------------------------------------

def run_retention(ctx):
    c, call = Checks(), Caller(ctx["client"], "r")
    gens = []
    first = call("config.set", "r-set-1", key="v", value="1")
    gens.append(first.get("gen"))
    pin = call("history.pin", "r-pin", name="first", gen=first.get("gen") or "0")
    c.expect(pin.ok, "pin", describe(pin))
    for i in range(2, 7):
        r = call("config.set", "r-set-%d" % i, key="v", value=str(i))
        c.expect(r.ok, "config_set", describe(r))
        gens.append(r.get("gen"))
    st = call("store.status")
    cur = int(st.get("gen", "0"))
    c.expect(st.get("history") == "3" and st.get("pins") == "1", "window",
             "retain=4: %s" % describe(st))
    g = call("config.get", key="v", gen=first.get("gen") or "0")
    c.expect(g.ok and g.get("value") == "1", "pinned_kept",
             "pinned generation %s: %s" % (first.get("gen"), describe(g)))
    g = call("config.get", key="v", gen=pin.get("gen") or "0")
    c.expect(g.state == "FAILED" and g.get("code") == "PRUNED", "window_pruned",
             "generation %s (outside the window, not pinned): %s" % (pin.get("gen"), describe(g)))
    g = call("config.get", key="v", gen=str(cur - 3))
    c.expect(g.ok and g.get("value") == "3", "window_kept",
             "generation %d (oldest in the window): %s" % (cur - 3, describe(g)))
    hist = call("history.list")
    by_gen = {i.get("gen"): i for i in hist.items}
    c.expect(by_gen.get(first.get("gen"), {}).get("content") == "kept" and
             by_gen.get(first.get("gen"), {}).get("pin") == "first", "history_pin",
             "history of the pinned generation: %s" % by_gen.get(first.get("gen")))
    c.expect(by_gen.get(pin.get("gen"), {}).get("content") == "pruned", "history_pruned",
             "history of generation %s: %s" % (pin.get("gen"), by_gen.get(pin.get("gen"))))
    free_pinned = int(st.get("free", "0"))
    un = call("history.unpin", "r-unpin", name="first")
    c.expect(un.ok, "unpin", describe(un))
    g = call("config.get", key="v", gen=first.get("gen") or "0")
    c.expect(g.state == "FAILED" and g.get("code") == "PRUNED", "unpin_released",
             "generation %s after unpin: %s" % (first.get("gen"), describe(g)))
    st2 = call("store.status")
    c.expect(int(st2.get("free", "0")) > free_pinned, "unpin_space",
             "free blocks %s -> %s after unpin" % (free_pinned, st2.get("free")))
    pr = call("store.prune", "r-prune")
    c.expect(pr.ok, "prune", describe(pr))
    st3 = call("store.status")
    cur = int(st3.get("gen", "0"))
    c.expect(st3.get("history") == "1", "prune_history", describe(st3))
    g = call("config.get", key="v", gen=str(cur - 1))
    c.expect(g.ok, "prune_parent", "parent generation after prune: %s" % describe(g))
    g = call("config.get", key="v", gen=str(cur - 2))
    c.expect(g.state == "FAILED" and g.get("code") == "PRUNED", "prune_older",
             "generation %d after prune: %s" % (cur - 2, describe(g)))
    c.expect(int(st3.get("free", "0")) > int(st2.get("free", "0")), "prune_space",
             "free blocks %s -> %s after prune" % (st2.get("free"), st3.get("free")))
    chk = call("store.check")
    c.expect(chk.ok, "store_check", describe(chk))
    return c.problems


SCRIPTS = {
    "persist-1": run_persist_1, "persist-2": run_persist_2,
    "corrupt-1": run_corrupt_1, "corrupt-2": run_corrupt_2,
    "unmountable-1": run_unmountable_1, "unmountable-2": run_unmountable_2,
    "full-1": run_full_1, "full-2": run_full_2,
    "retention": run_retention,
}
