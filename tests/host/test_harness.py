"""Unit tests for the harness verdict rules (tools/bench/harness.py)."""

import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "bench"))

import harness  # noqa: E402

BOOT = ("\x1b[2J\x1b[01;01HBdsDxe: starting Boot0001\r\n\r\n"
        "NANOX: loader start\r\n"
        "NANOX: loader exit_boot_services ok regions=34\r\n"
        "NANOX: kernel_main bootinfo=0x000000000e75c000\r\n"
        "NANOX: bootinfo ok version=1.0 size=192 regions=34 usable_bytes=1\r\n")


class ClassifyTest(unittest.TestCase):
    def verdict(self, serial, status, timed_out=False):
        r = harness.classify(serial, status, timed_out)
        return r["verdict"], r["failure_class"]

    def test_pass_requires_marker_and_exit(self):
        self.assertEqual(self.verdict(BOOT + "NANOX: TEST PASS\r\n", 33), ("PASS", None))

    def test_exit_33_without_marker_is_fail(self):
        self.assertEqual(self.verdict(BOOT, 33), ("FAIL", "inconsistent"))

    def test_marker_without_exit_is_fail(self):
        self.assertEqual(self.verdict(BOOT + "NANOX: TEST PASS\r\n", 0),
                         ("FAIL", "unexpected_exit"))
        self.assertEqual(self.verdict(BOOT + "NANOX: TEST PASS\r\n", 35),
                         ("FAIL", "inconsistent"))

    def test_pass_marker_with_timeout_is_fail(self):
        self.assertEqual(self.verdict(BOOT + "NANOX: TEST PASS\r\n", None, True),
                         ("FAIL", "timeout"))

    def test_pass_with_panic_line_is_fail(self):
        serial = BOOT + "NANOX: PANIC x\r\nNANOX: TEST PASS\r\n"
        self.assertEqual(self.verdict(serial, 33), ("FAIL", "inconsistent"))

    def test_duplicate_pass_is_fail(self):
        serial = BOOT + "NANOX: TEST PASS\r\nNANOX: TEST PASS\r\n"
        self.assertEqual(self.verdict(serial, 33), ("FAIL", "inconsistent"))

    def test_out_of_order_sequence_is_fail(self):
        serial = ("NANOX: kernel_main x\nNANOX: loader start\n"
                  "NANOX: loader exit_boot_services ok\nNANOX: bootinfo ok\n"
                  "NANOX: TEST PASS\n")
        self.assertEqual(self.verdict(serial, 33), ("FAIL", "inconsistent"))

    def test_pass_before_bootinfo_is_fail(self):
        serial = ("NANOX: loader start\nNANOX: loader exit_boot_services ok\n"
                  "NANOX: kernel_main\nNANOX: TEST PASS\nNANOX: bootinfo ok\n")
        self.assertEqual(self.verdict(serial, 33), ("FAIL", "inconsistent"))

    def test_marker_must_start_line(self):
        serial = BOOT.replace("NANOX: bootinfo ok", "x NANOX: bootinfo ok") + "NANOX: TEST PASS\n"
        self.assertEqual(self.verdict(serial, 33), ("FAIL", "inconsistent"))

    def test_fail_panic_loader(self):
        self.assertEqual(self.verdict(BOOT + "NANOX: TEST FAIL why\n", 35), ("FAIL", "test_fail"))
        self.assertEqual(self.verdict(BOOT + "NANOX: PANIC boom\n", 37), ("FAIL", "panic"))
        self.assertEqual(self.verdict(BOOT, 37), ("FAIL", "inconsistent"))
        r = harness.classify("NANOX: loader start\nNANOX: LOADER ERROR E_KERNEL_HASH (7): x\n",
                             39, False)
        self.assertEqual((r["verdict"], r["failure_class"], r["loader_error"]),
                         ("FAIL", "loader_error", "E_KERNEL_HASH"))
        self.assertEqual(self.verdict("NANOX: loader start\n", 39), ("FAIL", "inconsistent"))

    def test_other_exit_codes(self):
        for status in (0, 1, 3, -15, 255):
            self.assertEqual(self.verdict(BOOT + "NANOX: TEST PASS\n", status),
                             ("FAIL", "unexpected_exit"))


EXC_REPORT = ("NANOX: EXCEPTION #PF vector=14 error=0x2 rip=0x0000000000201000 "
              "cr2=0x00000000dead0000\n"
              "NANOX: BACKTRACE 0 0x0000000000201000\n"
              "NANOX: BACKTRACE 1 0x0000000000200105\n"
              "NANOX: PANIC unhandled #PF at rip=0x0000000000201000\n")


class FakeSymbolizer:
    def describe(self, addr):
        if 0x201000 <= addr < 0x201100:
            return "nx_fault_pagefault+0x%x" % (addr - 0x201000)
        if 0x200100 <= addr < 0x200200:
            return "kernel_main+0x%x" % (addr - 0x200100)
        return None


class ExceptionClassTest(unittest.TestCase):
    def test_exception_exit_code(self):
        r = harness.classify(BOOT + EXC_REPORT, 41, False)
        self.assertEqual((r["verdict"], r["failure_class"]), ("FAIL", "exception"))
        r = harness.classify(BOOT + "NANOX: PANIC x\n", 41, False)
        self.assertEqual(r["failure_class"], "inconsistent")
        r = harness.classify(BOOT + EXC_REPORT + "NANOX: TEST PASS\n", 33, False)
        self.assertEqual(r["failure_class"], "inconsistent")

    def test_report_parsing_and_symbols(self):
        rep = harness.analyze_report(BOOT + EXC_REPORT, FakeSymbolizer())
        exc = rep["exception"]
        self.assertEqual((exc["mnemonic"], exc["vector"], exc["error"]), ("#PF", 14, 2))
        self.assertEqual(exc["rip_symbol"], "nx_fault_pagefault+0x0")
        # frame 0 is the faulting RIP; return addresses are looked up at addr - 1
        self.assertEqual([f["symbol"] for f in rep["backtrace"]],
                         ["nx_fault_pagefault+0x0", "kernel_main+0x4"])

    def test_exception_expectations(self):
        outcome = harness.classify(BOOT + EXC_REPORT, 41, False)
        rep = harness.analyze_report(BOOT + EXC_REPORT, FakeSymbolizer())
        good = {"verdict": "FAIL", "failure_class": "exception",
                "exception": {"mnemonic": "#PF", "vector": 14, "error": 2, "cr2": "0xdead0000",
                              "rip_function": "nx_fault_pagefault"},
                "backtrace_functions": ["kernel_main"]}
        self.assertEqual(harness.check_expectation(outcome, good, EXC_REPORT, {}, rep), [])
        for key, bad in (("error", 3), ("cr2", "0xdead1000"), ("rip_function", "other"),
                         ("cr2_is_rip", True)):
            exp = dict(good, exception=dict(good["exception"], **{key: bad}))
            self.assertEqual(len(harness.check_expectation(outcome, exp, EXC_REPORT, {}, rep)), 1,
                             key)
        exp = dict(good, backtrace_functions=["run_mode"])
        self.assertEqual(len(harness.check_expectation(outcome, exp, EXC_REPORT, {}, rep)), 1)
        self.assertEqual(len(harness.check_expectation(outcome, good, "", {}, None)), 2)


class SymbolizerTest(unittest.TestCase):
    def test_kernel_symbols(self):
        import elfsym
        kernel = REPO / "out" / "kernel.elf"
        self.assertTrue(kernel.is_file(), "run make first")
        sym = elfsym.Symbolizer(kernel)
        start = sym.lookup(0x200000)
        self.assertEqual(start, ("_start", 0))
        names = {sym.describe(a) for a in range(0x200000, 0x200400, 4)}
        self.assertTrue(any(n and n.startswith("kernel_main+") for n in names))
        self.assertIsNone(sym.lookup(0x100))  # below the image; absolute symbols ignored


class RepeatTest(unittest.TestCase):
    def test_only_timing_fields_are_masked(self):
        a = ["NANOX: timer ok ticks=29 tsc_delta=123 lapic_per_10ms=5 hz=100", "NANOX: x=1"]
        b = ["NANOX: timer ok ticks=30 tsc_delta=999 lapic_per_10ms=6 hz=100", "NANOX: x=1"]
        self.assertEqual(harness.normalized_markers(a), harness.normalized_markers(b))
        c = ["NANOX: timer ok ticks=29 tsc_delta=123 lapic_per_10ms=5 hz=200", "NANOX: x=1"]
        self.assertNotEqual(harness.normalized_markers(a), harness.normalized_markers(c))
        self.assertNotEqual(harness.normalized_markers(["NANOX: rip=0x1"]),
                            harness.normalized_markers(["NANOX: rip=0x2"]))


class M2RepeatTest(unittest.TestCase):
    def test_scheduling_statistics_are_masked(self):
        a = ["NANOX: task spin-a#3 exited code=0 ticks=57 preempted=28 syscalls=6",
             "NANOX: m2-sched progress order latest_first=3 earliest_last=11 preempted=28,27,27",
             "NANOX: m2-sched ok tasks=3 switches=86 preemptions=82"]
        b = ["NANOX: task spin-a#3 exited code=0 ticks=55 preempted=26 syscalls=6",
             "NANOX: m2-sched progress order latest_first=2 earliest_last=12 preempted=1,2,30",
             "NANOX: m2-sched ok tasks=3 switches=90 preemptions=80"]
        self.assertEqual(harness.normalized_markers(a), harness.normalized_markers(b))
        c = [a[0].replace("syscalls=6", "syscalls=7")] + a[1:]
        self.assertNotEqual(harness.normalized_markers(a), harness.normalized_markers(c))

    def test_unordered_lines_compared_as_multiset(self):
        run1 = ["NANOX: sched ok", "NANOX: USER a#3: progress 1/2", "NANOX: USER b#4: progress 1/2",
                "NANOX: USER a#3: done", "NANOX: USER b#4: done", "NANOX: TEST PASS"]
        run2 = ["NANOX: sched ok", "NANOX: USER b#4: progress 1/2", "NANOX: USER a#3: progress 1/2",
                "NANOX: USER b#4: done", "NANOX: USER a#3: done", "NANOX: TEST PASS"]
        self.assertNotEqual(harness.repeat_view(run1), harness.repeat_view(run2))
        v1 = harness.repeat_view(run1, "^NANOX: USER ")
        self.assertEqual(v1, harness.repeat_view(run2, "^NANOX: USER "))
        self.assertEqual(v1[0], ["NANOX: sched ok", "NANOX: TEST PASS"])
        self.assertEqual(len(v1[1]), 4)
        # A missing, extra or changed line is still a difference.
        self.assertNotEqual(v1, harness.repeat_view(run2[:-2] + run2[-1:], "^NANOX: USER "))
        self.assertNotEqual(v1, harness.repeat_view(run2 + ["NANOX: USER a#3: done"],
                                                    "^NANOX: USER "))
        changed = [l.replace("b#4: done", "b#4: FAILED") for l in run2]
        self.assertNotEqual(v1, harness.repeat_view(changed, "^NANOX: USER "))
        # Ordered lines keep their order.
        swapped = [run1[-1]] + run1[1:-1] + [run1[0]]
        self.assertNotEqual(v1, harness.repeat_view(swapped, "^NANOX: USER "))


class InterleaveTest(unittest.TestCase):
    SPEC = {"pattern": "^NANOX: USER (t[0-9]): progress", "groups": 2}

    def serial(self, order):
        return BOOT + "".join("NANOX: USER %s: progress\n" % t for t in order)

    def test_interleaved(self):
        self.assertEqual(harness.check_interleave(self.serial(["t1", "t2", "t1", "t2"]),
                                                  self.SPEC), [])
        self.assertEqual(harness.check_interleave(self.serial(["t1", "t2", "t2", "t1"]),
                                                  self.SPEC), [])

    def test_sequential_is_rejected(self):
        problems = harness.check_interleave(self.serial(["t1", "t1", "t2", "t2"]), self.SPEC)
        self.assertEqual(len(problems), 1)
        self.assertIn("t1 reported its last line", problems[0])
        # One line each is not interleaving either.
        self.assertEqual(len(harness.check_interleave(self.serial(["t1", "t2"]), self.SPEC)), 1)

    def test_producer_count(self):
        self.assertEqual(len(harness.check_interleave(self.serial(["t1", "t1"]), self.SPEC)), 1)
        three = self.serial(["t1", "t2", "t3", "t1", "t2", "t3"])
        self.assertEqual(len(harness.check_interleave(three, self.SPEC)), 1)
        self.assertEqual(harness.check_interleave(three, dict(self.SPEC, groups=3)), [])

    def test_expectation_hook(self):
        outcome = {"verdict": "PASS"}
        exp = {"verdict": "PASS", "interleave": self.SPEC}
        good, bad = self.serial(["t1", "t2", "t1", "t2"]), self.serial(["t1", "t1", "t2", "t2"])
        self.assertEqual(harness.check_expectation(outcome, exp, good, {}), [])
        self.assertEqual(len(harness.check_expectation(outcome, exp, bad, {})), 1)


class ExpectationTest(unittest.TestCase):
    def test_expectation_mismatch_reported(self):
        outcome = harness.classify(BOOT + "NANOX: TEST PASS\n", 33, False)
        self.assertEqual(harness.check_expectation(outcome, {"verdict": "PASS"}, BOOT, {}), [])
        problems = harness.check_expectation(outcome, {"verdict": "FAIL"}, BOOT, {})
        self.assertEqual(len(problems), 1)

    def test_pattern_substitution(self):
        serial = BOOT + "NANOX: kernel sha256 abc123\n"
        outcome = {"verdict": "PASS"}
        exp = {"verdict": "PASS", "patterns": ["^NANOX: kernel sha256 {kernel_sha256}$"]}
        self.assertEqual(harness.check_expectation(outcome, exp, serial,
                                                   {"kernel_sha256": "abc123"}), [])
        self.assertEqual(len(harness.check_expectation(outcome, exp, serial,
                                                       {"kernel_sha256": "def456"})), 1)


class ScenarioFileTest(unittest.TestCase):
    def test_required_scenarios_present(self):
        names = {s["name"] for s in harness.load_scenarios()}
        for required in ("normal", "fail", "panic", "hang", "missing-kernel", "corrupt-kernel",
                         "missing-initrd", "corrupt-initrd", "bad-initrd", "pagefault",
                         "nullderef", "wprotect", "nxexec", "stackoverflow", "ud", "gp",
                         "divzero", "doublefree", "timer-masked", "m2-user", "m2-sched",
                         "m2-sched-nopreempt", "m2-ipc", "m2-ipc-overgrant"):
            self.assertIn(required, names)

    def test_scenario_patterns_are_valid(self):
        """Every pattern survives str.format substitution and compiles."""
        import re
        subs = {"kernel_sha256": "0" * 64, "initrd_sha256": "0" * 64}
        for sc in harness.load_scenarios():
            for pattern in sc["expect"].get("patterns", []):
                re.compile(pattern.format(**subs))
            if "interleave" in sc["expect"]:
                self.assertEqual(re.compile(sc["expect"]["interleave"]["pattern"]).groups, 1)
            if "repeat" in sc:
                re.compile(sc["repeat"]["unordered"])


if __name__ == "__main__":
    unittest.main()
