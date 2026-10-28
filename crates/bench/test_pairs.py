"""Deterministic host-order and regression scenarios; no benchmarking or sleeps."""
import copy
import unittest
import run


class PairTests(unittest.TestCase):
    def measurements(self, slowdown=1.0, first_penalty=1.2):
        baseline = {"schema": run.SCHEMA, "runner": "windows-2025", "fixture": "v1",
                    "medians": {"open_first_ms": 1, "index_gb_s": 4,
                                "jump_ms": 1, "private_mb": 8}}
        current, reference = [], []
        for i in range(8):
            a = copy.deepcopy(baseline["medians"])
            b = copy.deepcopy(a)
            a_first = not run.reference_first(i)
            for metric, (direction, _) in run.LIMITS.items():
                if metric == "private_mb":
                    continue
                a_cost = slowdown * (first_penalty if a_first else 1)
                b_cost = first_penalty if not a_first else 1
                a[metric] *= a_cost if direction == "max" else 1 / a_cost
                b[metric] *= b_cost if direction == "max" else 1 / b_cost
            current.append(a)
            reference.append(b)
        return baseline, current, reference

    def compare(self, slowdown=1.0, first_penalty=1.2):
        baseline, current, reference = self.measurements(slowdown, first_penalty)
        result = run.summarize(current, baseline["runner"], "candidate")
        before = run.summarize(reference, baseline["runner"], "reference")
        result["paired_cost_ratios"] = run.paired_cost_ratios(current, reference)
        return result, baseline, before

    def test_identical_operations_with_first_process_penalty_do_not_regress(self):
        result, baseline, before = self.compare()
        self.assertEqual(run.check(result, baseline, before), [])
        for ratios in result["paired_cost_ratios"].values():
            for ratio in ratios:
                self.assertAlmostEqual(ratio, 1)

    def test_real_fifteen_percent_slowdown_fails_even_with_order_penalty(self):
        for penalty in (1, 1.2):
            result, baseline, before = self.compare(1.15, penalty)
            errors = run.check(result, baseline, before)
            for metric in ("open_first_ms", "index_gb_s", "jump_ms"):
                self.assertTrue(any(f"regression: {metric}" in e for e in errors), (metric, errors))

    def test_ten_percent_boundary_is_unchanged(self):
        result, baseline, before = self.compare(1.10)
        self.assertEqual(run.check(result, baseline, before), [])
        result, baseline, before = self.compare(1.10001)
        self.assertTrue(run.check(result, baseline, before))

    def test_spread_straddling_ten_percent_warns_without_waiving_hard_limits(self):
        result, baseline, before = self.compare(1.15)
        for ratios in result["paired_cost_ratios"].values():
            ratios[0] = 1.05
        self.assertEqual(run.check(result, baseline, before), [])
        warnings = run.uncertain_comparisons(result)
        self.assertEqual(len(warnings), 3)
        self.assertTrue(all("INCONCLUSIVE" in w for w in warnings))
        result["medians"]["open_first_ms"] = 101
        self.assertTrue(any("hard limit" in e for e in run.check(result, baseline, before)))

    def test_unbalanced_or_missing_pair_evidence_cannot_pass(self):
        baseline, current, reference = self.measurements()
        for a, b in ((current[:-1], reference[:-1]), (current, reference[:-1])):
            with self.assertRaises(ValueError):
                run.paired_cost_ratios(a, b)
        result, baseline, before = self.compare()
        del result["paired_cost_ratios"]
        self.assertTrue(run.check(result, baseline, before))


if __name__ == "__main__":
    unittest.main()
