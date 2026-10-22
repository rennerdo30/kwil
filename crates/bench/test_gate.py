"""Focused contract tests. Synthetic times make the negative cases deterministic."""
import copy
import unittest
import run


class GateTests(unittest.TestCase):
    def setUp(self):
        self.base = {"schema": run.SCHEMA, "runner": "windows-2025", "fixture": "v1",
                     "medians": {"open_first_ms": 1, "index_gb_s": 4, "jump_ms": 1,
                                 "private_mb": 8, "startup_file_ms": 40,
                                 "startup_empty_ms": 25, "startup_index_ms": 200}}

    def test_same_passes_and_cost_over_ten_percent_fails(self):
        self.assertEqual(run.check(self.base, self.base), [])
        for metric, (direction, _) in run.LIMITS.items():
            changed = copy.deepcopy(self.base)
            changed["medians"][metric] *= 1.101 if direction == "max" else 1 / 1.101
            self.assertTrue(any("regression" in e for e in run.check(changed, self.base)), metric)

    def test_hard_limit_cannot_be_blessed_by_slow_baseline(self):
        for metric, (direction, limit) in run.LIMITS.items():
            if limit is None:
                continue
            bad = copy.deepcopy(self.base)
            bad["medians"][metric] = limit * (1.01 if direction == "max" else 0.99)
            self.assertTrue(any("hard limit" in e for e in run.check(bad, bad)), metric)

    def test_os_mismatch_and_missing_metric_fail_closed(self):
        bad = copy.deepcopy(self.base)
        bad["runner"] = "ubuntu-24.04"
        self.assertTrue(run.check(self.base, bad))
        bad = copy.deepcopy(self.base)
        del bad["medians"]["jump_ms"]
        self.assertTrue(run.check(self.base, bad))

    def test_nonfinite_and_zero_cannot_bypass_gate(self):
        for value in (float("nan"), float("inf"), 0):
            bad = copy.deepcopy(self.base)
            bad["medians"]["index_gb_s"] = value
            self.assertTrue(run.check(bad, self.base))
            self.assertTrue(run.check(self.base, bad))

    def test_paired_timing_compensates_host_speed_but_keeps_ten_percent_gate(self):
        reference = copy.deepcopy(self.base)
        current = copy.deepcopy(self.base)
        # Both binaries run on a host twice as slow as the stored sample.
        for metric, (direction, _) in run.LIMITS.items():
            if metric == "private_mb":
                continue
            reference["medians"][metric] *= 2 if direction == "max" else 0.5
            current["medians"][metric] = reference["medians"][metric]
        self.assertEqual(run.check(current, self.base, reference), [])
        self.assertTrue(run.check(current, self.base))
        for metric, (direction, _) in run.LIMITS.items():
            if metric == "private_mb":
                continue
            slow = copy.deepcopy(current)
            slow["medians"][metric] *= 1.101 if direction == "max" else 1 / 1.101
            self.assertTrue(any("regression" in e for e in run.check(slow, self.base, reference)), metric)

    def test_paired_reference_cannot_waive_hard_or_stored_memory_limits(self):
        reference = copy.deepcopy(self.base)
        reference["medians"]["private_mb"] = 200
        current = copy.deepcopy(self.base)
        current["medians"]["private_mb"] *= 1.101
        self.assertTrue(any("regression" in e for e in run.check(current, self.base, reference)))
        for metric, (direction, limit) in run.LIMITS.items():
            if limit is None:
                continue
            bad = copy.deepcopy(self.base)
            bad["medians"][metric] = limit * (1.01 if direction == "max" else 0.99)
            self.assertTrue(any("hard limit" in e for e in run.check(bad, bad, bad)), metric)

    def test_paired_reference_must_match_runner_and_fixture(self):
        wrong = copy.deepcopy(self.base)
        wrong["runner"] = "ubuntu-24.04"
        self.assertTrue(run.check(self.base, self.base, wrong))

    def test_linux_tick_phase_cannot_create_a_startup_regression(self):
        for phase in (0.1, 3.5, 9.8):
            raw = {"launch": 2 + phase, "first_frame": 40 + phase, "index_done": 90 + phase}
            corrected = run.correct_trace(raw, {"mode": "parent_launch", "offset_ms": phase})
            self.assertAlmostEqual(corrected["first_frame"], 40)
            self.assertAlmostEqual(corrected["index_done"], 90)
            self.assertEqual(corrected["clock"]["raw_os_marks"], raw)
        with self.assertRaises(RuntimeError):
            run.correct_trace({"launch": 1, "first_frame": 2, "index_done": 3}, {"offset_ms": 5})


if __name__ == "__main__":
    unittest.main()
