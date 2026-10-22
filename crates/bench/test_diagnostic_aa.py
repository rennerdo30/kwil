"""Hosted protocol checks use tiny child probes, never benchmark fixtures."""
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import diagnostic_aa as aa
import run as production


class DiagnosticTests(unittest.TestCase):
    def test_nine_paired_rounds_alternate_first_role(self):
        order = aa.paired_order()
        self.assertEqual(len(order), 18)
        for round_number in range(9):
            pair = order[2 * round_number:2 * round_number + 2]
            self.assertEqual({role for _, role in pair}, {"A", "B"})
            self.assertEqual([r for r, _ in pair], [round_number] * 2)
            if round_number:
                self.assertNotEqual(pair[0][1], order[2 * (round_number - 1)][1])

    def test_different_app_or_harness_cannot_be_an_aa_control(self):
        with tempfile.TemporaryDirectory() as directory:
            left, right = Path(directory) / "a", Path(directory) / "b"
            left.mkdir()
            right.mkdir()
            suffix = ".exe" if aa.os.name == "nt" else ""
            for name in ("kwil", "kwil-bench"):
                (left / (name + suffix)).write_bytes(b"identical control")
                (right / (name + suffix)).write_bytes(b"identical control")
            self.assertEqual(aa.identities(left, right)["A"]["kwil"]["sha256"], aa.digest(left / ("kwil" + suffix)))
            for name in ("kwil", "kwil-bench"):
                path = right / (name + suffix)
                path.write_bytes(b"different binary")
                with self.assertRaisesRegex(RuntimeError, "hash-identical"):
                    aa.identities(left, right)
                path.write_bytes(b"identical control")

    def test_diagnostic_report_is_not_an_acceptable_baseline(self):
        report = aa.new_report("ubuntu-24.04")
        self.assertTrue(report["diagnostic_only"])
        self.assertFalse(report["acceptance_eligible"])
        self.assertNotIn("schema", report)
        self.assertTrue(production.check(report, report))

    def test_exact_child_resource_probe_and_nonzero_exit(self):
        for code in (0, 7):
            result = aa.collect([sys.executable, "-c", f'print("probe"); raise SystemExit({code})'], timeout=10)
            self.assertEqual(result["returncode"], code)
            self.assertFalse(result["timed_out"])
            self.assertIn("probe", result["stdout"])
            self.assertGreater(result["pid"], 0)
            self.assertGreater(result["parent_wall_ms"], 0)
            self.assertGreaterEqual(result["cpu_ms"], 0)
            self.assertIn("user_cpu_ms", result["rusage"])
            self.assertIn("kernel_cpu_ms", result["rusage"])
            if sys.platform == "linux":
                self.assertEqual(result["rusage"]["method"], "wait4_exact_child")
                self.assertIn("ru_majflt", result["rusage"])
                self.assertIn("ru_nivcsw", result["rusage"])

    def test_timeout_reaps_only_the_started_child(self):
        result = aa.collect([sys.executable, "-c", "import time; time.sleep(30)"], timeout=0.05)
        self.assertTrue(result["timed_out"])
        self.assertNotEqual(result["returncode"], 0)
        self.assertLess(result["parent_wall_ms"], 10000)
        if sys.platform == "linux":
            with self.assertRaises(ChildProcessError):
                aa.os.wait4(result["pid"], aa.os.WNOHANG)

    def test_failed_child_is_retained_before_sample_rejection(self):
        report = aa.new_report("test")
        roles = {"A": {name: {"path": name, "sha256": "control"} for name in ("kwil", "kwil-bench")}}
        failed = {"timed_out": False, "returncode": 7, "pid": 123}
        with tempfile.TemporaryDirectory() as directory, patch.object(aa, "collect", return_value=failed):
            output = Path(directory) / "partial.json"
            with self.assertRaisesRegex(RuntimeError, "child failed"):
                aa.sample(roles, "A", 0, Path("unused"), report, output)
            self.assertTrue(output.is_file())
            self.assertEqual(report["children"][0]["returncode"], 7)
            self.assertEqual(report["children"][0]["role"], "A")

    def test_missing_trace_mark_and_output_overflow_fail(self):
        with self.assertRaisesRegex(RuntimeError, "missing t_"):
            aa.trace_marks({"stderr": "", "launch_clock": {"offset_ms": 0}})
        with tempfile.TemporaryFile() as stream:
            stream.write(b"x" * (aa.OUTPUT_LIMIT + 1))
            with self.assertRaisesRegex(RuntimeError, "exceeded"):
                aa.read_output(stream)


if __name__ == "__main__":
    unittest.main()
