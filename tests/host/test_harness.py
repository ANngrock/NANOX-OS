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
        for required in ("normal", "fail", "panic", "hang", "missing-kernel", "corrupt-kernel"):
            self.assertIn(required, names)


if __name__ == "__main__":
    unittest.main()
