"""Unit tests of the M4 host-side checks (tools/bench/storecheck.py) and of
the crash-point verdict class of the harness."""

import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "bench"))
sys.path.insert(0, str(REPO / "tools" / "store"))

import harness  # noqa: E402
import nxstore  # noqa: E402
import storecheck as sc  # noqa: E402

U = "NANOX: USER core#4: "
STEPS = [U + "workload step 1: REQ w1 config.set key=mode value=alpha",
         U + "workload step 2: REQ w2 task.spawn program=load",
         U + "workload step 3: REQ w3 blob.put name=b1 size=100",
         U + "workload step 4: REQ w4 history.pin name=keep gen=2",
         U + "workload step 5: REQ w5 config.delete key=mode"]
SAVED = [(2, "w1"), (3, "w2:s"), (4, "w2:d"), (5, "w3"), (6, "w4"), (7, "w5")]
BOOT = ["NANOX: loader start", "NANOX: loader exit_boot_services ok", "NANOX: kernel_main x",
        "NANOX: bootinfo ok x"]


def run_markers(ops, saved, begun=None, crash=None, extra=()):
    m = ["NANOX: m4 boot_id=0123 bridge=none"] + list(extra)
    for n, (op, pending) in enumerate(ops, 1):
        m.append("NANOX: m4 io %d %s%s pending=%d" % (n, op, " blk=5 n=1" if op == "write" else "",
                                                      pending))
    for g, label in begun or []:
        m.append(U + "store commit begin gen=%d label=%s" % (g, label))
    for g, label in saved:
        m.append(U + "store saved gen=%d label=%s free=10" % (g, label))
    if crash:
        m.append("NANOX: CRASH POINT io=%d op=write pending=%d policy=all persisted=0 torn=0" % crash)
    return m


class ModelTest(unittest.TestCase):
    def models(self):
        steps = sc.parse_run(STEPS)["steps"]
        return sc.model_states(steps, SAVED)

    def test_model_follows_the_commits(self):
        m = self.models()
        self.assertEqual(m["format"], sc.empty_state())
        self.assertEqual(m["w1"]["config"], {"mode": "alpha"})
        self.assertEqual(m["w2:s"]["tasks"][-1], ["w2", "task.spawn", "RUNNING"])
        self.assertEqual(m["w2:d"]["tasks"][-1], ["w2", "task.spawn", "SUCCEEDED"])
        self.assertEqual(m["w3"]["blobs"], {"b1": 100})
        self.assertEqual(m["w4"]["pins"], {"keep": 2})
        self.assertEqual(m["w5"]["config"], {})
        self.assertEqual([t[0] for t in m["w5"]["tasks"]], ["w1", "w2", "w3", "w4", "w5"])
        # earlier states are not modified by later commits
        self.assertEqual(m["w1"]["tasks"], [["w1", "config.set", "SUCCEEDED"]])

    def test_recovery_marks_interrupted_records(self):
        st = sc.after_recovery(self.models()["w2:s"])
        self.assertEqual(st["tasks"][-1][2], "OUTCOME_UNKNOWN")
        self.assertEqual(self.models()["w2:s"]["tasks"][-1][2], "RUNNING")

    def test_policies(self):
        self.assertEqual(sc.policies_for(0), ["all"])
        self.assertEqual(sc.policies_for(1), ["all", "none", "torn"])
        self.assertEqual(sc.policies_for(5), ["all", "none", "torn", "reorder"])


class EvaluateTest(unittest.TestCase):
    def setUp(self):
        ref_markers = STEPS + run_markers([("write", 0), ("write", 1), ("flush", 2),
                                           ("write", 0), ("flush", 1)], SAVED)
        self.ref = sc.parse_run(ref_markers)
        self.ref["models"] = sc.model_states(self.ref["steps"], self.ref["saved"])

    def crash(self, k, saved, begun):
        ops = [("write", 0), ("write", 1), ("flush", 2), ("write", 0), ("flush", 1)][:k - 1]
        return sc.parse_run(run_markers(ops, saved, begun, crash=(k, 0)))

    def check(self, gen, label, state_lines, fsck="ok"):
        m = BOOT + ["NANOX: m4 boot_id=4567 bridge=none",
                    U + "store mounted gen=%d label=%s slot=0" % (gen, label)]
        m += [U + l for l in state_lines] + [U + "m4 fsck %s roots=1" % fsck]
        run = sc.parse_run(m)
        run["markers"] = m
        return run

    def reader(self, **st):
        base = sc.empty_state()
        base.update({"gen": 9, "label": "x", "fsck": True, "blobs_ok": True})
        base.update(st)
        return base, {}

    def test_good_point(self):
        crash = self.crash(4, [(2, "w1")], [(2, "w1"), (3, "w2:s")])
        chk = self.check(3, "w2:s", ["m4 state gen=4 label=recover objects=2 pins=0 history=2",
                                      "m4 state cfg mode=alpha version=1",
                                      "m4 state task w1 config.set SUCCEEDED",
                                      "m4 state task w2 task.spawn OUTCOME_UNKNOWN"])
        rd = self.reader(config={"mode": "alpha"},
                         tasks=[["w1", "config.set", "SUCCEEDED"],
                                ["w2", "task.spawn", "OUTCOME_UNKNOWN"]])
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", rd, {"k": 4, "policy": "all"})
        self.assertEqual((r["violations"], r["errors"]), ([], []))
        self.assertEqual((r["acked_gen"], r["attempted_gen"], r["recovered_gen"]), (2, 3, 3))

    def test_saved_generation_lost(self):
        crash = self.crash(4, [(2, "w1"), (3, "w2:s")], [(3, "w2:s")])
        chk = self.check(2, "w1", ["m4 state cfg mode=alpha version=1",
                                   "m4 state task w1 config.set SUCCEEDED"])
        rd = self.reader(config={"mode": "alpha"}, tasks=[["w1", "config.set", "SUCCEEDED"]])
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", rd, {"k": 4, "policy": "all"})
        self.assertTrue(any(v.startswith("saved_lost") for v in r["violations"]))

    def test_content_mismatch(self):
        crash = self.crash(2, [], [(2, "w1")])
        chk = self.check(2, "w1", ["m4 state cfg mode=beta version=1",
                                   "m4 state task w1 config.set SUCCEEDED"])
        rd = self.reader(config={"mode": "beta"}, tasks=[["w1", "config.set", "SUCCEEDED"]])
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", rd, {"k": 2, "policy": "none"})
        self.assertTrue(any("content_mismatch (guest)" in v for v in r["violations"]))
        self.assertTrue(any("content_mismatch (host reader)" in v for v in r["violations"]))

    def test_guest_and_reader_must_both_agree(self):
        crash = self.crash(2, [], [(2, "w1")])
        chk = self.check(1, "format", [])
        rd = self.reader(config={"mode": "alpha"})
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", rd, {"k": 2, "policy": "all"})
        self.assertEqual(len(r["violations"]), 1)
        self.assertIn("host reader", r["violations"][0])

    def test_fsck_failure_is_a_violation(self):
        crash = self.crash(2, [], [(2, "w1")])
        chk = self.check(1, "format", [], fsck="FAIL")
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", self.reader(), {"k": 2, "policy": "all"})
        self.assertIn("guest consistency check failed", r["violations"])

    def test_unmountable(self):
        crash = self.crash(2, [], [(2, "w1")])
        m = BOOT + ["NANOX: m4 boot_id=4567", U + "m4 state unmountable: no_valid_root"]
        chk = sc.parse_run(m)
        chk["markers"] = m
        r = sc.evaluate_point(self.ref, crash, chk, "FAIL", (None, {}), {"k": 2, "policy": "all"})
        self.assertTrue(r["violations"][0].startswith("unmountable"))

    def test_infrastructure_failure_is_an_error(self):
        crash = self.crash(2, [], [(2, "w1")])
        m = BOOT + ["NANOX: TEST FAIL timer: rate"]
        chk = sc.parse_run(m)
        chk["markers"] = m
        r = sc.evaluate_point(self.ref, crash, chk, "FAIL", (None, {}), {"k": 2, "policy": "all"})
        self.assertEqual(r["violations"], [])
        self.assertEqual(len(r["errors"]), 1)

    def test_wrong_crash_point_is_an_error(self):
        crash = self.crash(3, [], [])
        chk = self.check(1, "format", [])
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", self.reader(), {"k": 2, "policy": "all"})
        self.assertTrue(r["errors"])

    def test_prefix_must_match_the_reference(self):
        crash = sc.parse_run(run_markers([("flush", 0)], [], [], crash=(2, 0)))
        chk = self.check(1, "format", [])
        r = sc.evaluate_point(self.ref, crash, chk, "PASS", self.reader(), {"k": 2, "policy": "all"})
        self.assertIn("io operations before the crash point differ from the reference run",
                      r["errors"])


class StoreExpectTest(unittest.TestCase):
    def test_empty_store(self):
        img = nxstore.format_image(blocks=64)
        self.assertEqual(sc.check_store({"mounted": True, "gen": 1, "check_ok": True,
                                         "config": {"x": None}}, img), [])
        p = sc.check_store({"gen": 2, "config": {"x": "1"}, "tasks": {"a": "SUCCEEDED"}}, img)
        self.assertEqual(len(p), 3)

    def test_unmountable(self):
        img = bytearray(nxstore.format_image(blocks=64))
        img[100] ^= 1
        p = sc.check_store({"mounted": False, "slot_states": ["bad_crc", "empty"]}, bytes(img))
        self.assertEqual(p, [])
        self.assertEqual(len(sc.check_store({"mounted": True}, bytes(img))), 1)


class CrashPointClassTest(unittest.TestCase):
    SERIAL = "\n".join(BOOT) + "\n"

    def test_crash_point(self):
        r = harness.classify(self.SERIAL + "NANOX: CRASH POINT io=3 op=write pending=0 "
                             "policy=all persisted=0 torn=0\n", 43, False)
        self.assertEqual((r["verdict"], r["failure_class"]), ("FAIL", "crash_point"))

    def test_crash_exit_without_marker(self):
        r = harness.classify(self.SERIAL, 43, False)
        self.assertEqual(r["failure_class"], "inconsistent")

    def test_crash_marker_with_pass(self):
        r = harness.classify(self.SERIAL + "NANOX: CRASH POINT io=3\nNANOX: TEST PASS\n", 43,
                             False)
        self.assertEqual(r["failure_class"], "inconsistent")


if __name__ == "__main__":
    unittest.main()
