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


if __name__ == "__main__":
    unittest.main()
