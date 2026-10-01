import json
import os
import shutil
import subprocess
import unittest

def read(path):
    with open(path) as f:
        return f.read()


ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SPEC = os.path.join(ROOT, "user", "x86_64-unknown-nanox.json")
LINK = os.path.join(ROOT, "user", "link.ld")


class TargetSpec(unittest.TestCase):
    def test_fields_match_the_documented_contract(self):
        spec = json.loads(read(SPEC))
        self.assertEqual(spec["os"], "nanox")
        self.assertEqual(spec["arch"], "x86_64")
        self.assertEqual(spec["panic-strategy"], "abort")
        self.assertEqual(spec["relocation-model"], "static")
        self.assertEqual(spec["code-model"], "small")
        self.assertIs(spec["position-independent-executables"], False)
        self.assertIs(spec["dynamic-linking"], False)
        self.assertIs(spec["executables"], True)
        self.assertIs(spec["has-thread-local"], False)
        self.assertIn("+soft-float", spec["features"])
        self.assertEqual(spec["target-pointer-width"], "64")

    def test_it_differs_from_the_tier_2_target_only_where_documented(self):
        if not shutil.which("rustc"):
            self.skipTest("no rustc")
        env = dict(os.environ, RUSTC_BOOTSTRAP="1")
        out = subprocess.run(
            ["rustc", "-Zunstable-options", "--print", "target-spec-json",
             "--target", "x86_64-unknown-none"],
            capture_output=True, text=True, env=env)
        if out.returncode != 0:
            self.skipTest("rustc cannot print the tier 2 spec")
        base, ours = json.loads(out.stdout), json.loads(read(SPEC))
        ours_only = {"os", "vendor", "executables", "dynamic-linking", "has-thread-local"}
        changed = {k for k in set(base) | set(ours) if base.get(k) != ours.get(k)}
        changed -= {"metadata", "supported-sanitizers"}
        self.assertEqual(changed - ours_only,
                         {"code-model", "position-independent-executables",
                          "static-position-independent-executables", "relro-level",
                          "relocation-model"})

    def test_rustc_accepts_the_spec(self):
        if not shutil.which("rustc"):
            self.skipTest("no rustc")
        out = subprocess.run(["rustc", "--print", "cfg", "--target", SPEC],
                             capture_output=True, text=True)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn('target_os="nanox"', out.stdout)
        self.assertIn('panic="abort"', out.stdout)

    def test_the_linker_script_has_one_load_per_permission_and_a_stack(self):
        text = read(LINK)
        for needle in ("PT_LOAD FLAGS(5)", "PT_LOAD FLAGS(4)", "PT_LOAD FLAGS(6)",
                       "PT_GNU_STACK FLAGS(6)", ". = 0x400000", "ENTRY(_start)", "KEEP(*(.text.entry))"):
            self.assertIn(needle, text)
        self.assertNotIn("FLAGS(7)", text, "no writable and executable segment")


if __name__ == "__main__":
    unittest.main()
