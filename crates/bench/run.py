"""Fresh-process core-text budgets, paired with the stored baseline's source (stdlib only)."""
import argparse
import json
import math
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys

SCHEMA = 1
LIMITS = {
    "open_first_ms": ("max", 100),
    "index_gb_s": ("min", 1),
    "jump_ms": ("max", 16),
    "private_mb": ("max", 256),
}


def run(args, **kwargs):
    command = [str(a) for a in args]
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, encoding="utf-8", errors="replace", **kwargs) as child:
        print(f"started PID {child.pid}: {args[0]}", file=sys.stderr)
        try:
            out, err = child.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate()
            raise
        if child.returncode:
            raise RuntimeError(f"{args}: exit {child.returncode}\n{out}\n{err}")
        return subprocess.CompletedProcess(args, child.returncode, out, err)


def positive_number(value):
    return type(value) in (int, float) and math.isfinite(value) and value > 0


def reference_first(iteration):
    # Adjacent pairs form ABBA, then BAAB. Every four pairs have equal order.
    return iteration % 4 in (1, 2)


def paired_cost_ratios(current, reference):
    if len(current) != len(reference) or len(current) < 4 or len(current) % 4:
        raise ValueError("paired sampling requires equal counts in complete ABBA/BAAB cycles")
    result = {}
    for metric, (direction, _) in LIMITS.items():
        if metric == "private_mb":
            continue
        raw = []
        for a, b in zip(current, reference):
            value, before = a.get(metric), b.get(metric)
            if not positive_number(value) or not positive_number(before):
                raise ValueError(f"invalid paired measurement: {metric}")
            raw.append(value / before if direction == "max" else before / value)
        # Each ABBA/BAAB iteration contains two opposite-order pairs. Average
        # their log costs so a multiplicative first-process penalty cancels,
        # rather than biasing an arithmetic median of reciprocal raw ratios.
        result[metric] = [math.exp((math.log(a) + math.log(b)) / 2)
                          for a, b in zip(raw[::2], raw[1::2])]
    return result


def uncertain_comparisons(current):
    warnings = []
    for metric, ratios in current.get("paired_cost_ratios", {}).items():
        if (ratios and all(positive_number(r) for r in ratios) and
                min(ratios) <= 1.10 < max(ratios)):
            warnings.append(f"INCONCLUSIVE: {metric} paired cost spread "
                            f"{min(ratios):.4f}..{max(ratios):.4f} straddles 1.10")
    return warnings


def check(current, baseline, reference=None):
    errors = []
    identity = ("schema", "runner", "fixture")
    if (baseline.get("schema") != SCHEMA or
            any(baseline.get(k) != current.get(k) for k in identity)):
        return ["baseline schema, runner OS/image or fixture mismatch"]
    if reference is not None and any(reference.get(k) != current.get(k) for k in identity):
        return ["paired reference schema, runner OS/image or fixture mismatch"]
    for metric, (direction, limit) in LIMITS.items():
        value = current.get("medians", {}).get(metric)
        # Timings use baseline code on this same host. Memory always compares
        # against the stored OS result; host calibration cannot waive growth.
        comparison = reference if reference is not None and metric != "private_mb" else baseline
        before = comparison.get("medians", {}).get(metric)
        if not positive_number(value):
            errors.append(f"invalid measurement: {metric}")
            continue
        if not positive_number(before):
            errors.append(f"missing/invalid baseline: {metric}")
            continue
        if ((direction == "max" and value > limit) or
                (direction == "min" and value < limit)):
            errors.append(f"hard limit: {metric}={value:.4f}, {direction} {limit}")
        # Compare cost (time per byte for throughput), not a percentage drop in GB/s.
        if reference is not None and metric != "private_mb":
            ratios = current.get("paired_cost_ratios", {}).get(metric)
            if (not isinstance(ratios, list) or len(ratios) < 2 or
                    not all(positive_number(r) for r in ratios)):
                errors.append(f"missing/invalid paired cost ratios: {metric}")
                continue
            ratio = statistics.median(ratios)
            # Conservative full observed spread: crossing the unchanged 10%
            # boundary is inconclusive, never a noisy regression failure.
            conclusive = min(ratios) > 1.10
        else:
            ratio = value / before if direction == "max" else before / value
            conclusive = True
        if ratio > 1.10 and conclusive:
            errors.append(f"regression: {metric} paired/stored cost +{(ratio-1)*100:.1f}% >10%")
    return errors


def sample(bin_dir, log):
    suffix = ".exe" if os.name == "nt" else ""
    return json.loads(run([bin_dir / ("kwil-bench" + suffix), "measure", log]).stdout)


def summarize(samples, runner, commit):
    return {"schema": SCHEMA, "runner": runner, "fixture": "v1", "commit": commit,
            "runs": len(samples), "samples": samples,
            "medians": {metric: statistics.median(s[metric] for s in samples) for metric in LIMITS}}


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--fixtures", type=Path, required=True)
    p.add_argument("--runner", required=True)
    p.add_argument("--runs", type=int, default=8, choices=(4, 8, 12))
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--reference-bin-dir", type=Path, required=True)
    p.add_argument("--reference-source", type=Path, required=True)
    p.add_argument("--reference-manifest", type=Path, required=True)
    a = p.parse_args()
    # Missing baselines are errors, never permission to seed from the candidate.
    baseline = json.loads(a.baseline.read_text(encoding="utf-8-sig"))
    manifest = json.loads(a.reference_manifest.read_text(encoding="utf-8-sig"))
    expected = manifest["source_head"]
    reference_commit = run(["git", "-C", a.reference_source, "rev-parse", "HEAD"]).stdout.strip()
    if not re.fullmatch("[0-9a-f]{40}", expected) or reference_commit != expected:
        raise RuntimeError("reference checkout does not match the trusted baseline source")
    commit = run(["git", "rev-parse", "HEAD"]).stdout.strip()
    log = a.fixtures / "log-1g.log"
    if log.stat().st_size < 1 << 30:
        raise RuntimeError("canonical 1 GiB fixture required")
    with log.open("rb") as f:
        while f.read(1 << 20):
            pass
    samples, reference_samples = [], []
    for iteration in range(a.runs):
        order = [(a.bin_dir, samples), (a.reference_bin_dir, reference_samples)]
        if reference_first(iteration):
            order.reverse()
        for bin_dir, bucket in order:
            bucket.append(sample(bin_dir, log))
    result = summarize(samples, a.runner, commit)
    reference = summarize(reference_samples, a.runner, reference_commit)
    result.update(reference=reference, stored_baseline=baseline, reference_manifest=manifest)
    result["paired_cost_ratios"] = paired_cost_ratios(samples, reference_samples)
    result["paired_cost_medians"] = {k: statistics.median(v) for k, v in result["paired_cost_ratios"].items()}
    result["comparison"] = "balanced-paired-cost-v2"
    result["pair_order"] = ["BA" if reference_first(i) else "AB" for i in range(a.runs)]
    result["warnings"] = uncertain_comparisons(result)
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print("Candidate medians:", json.dumps(result["medians"], indent=2))
    print("Paired baseline-code medians:", json.dumps(reference["medians"], indent=2))
    print("Balanced paired cost ratios:", json.dumps(result["paired_cost_ratios"], indent=2))
    for warning in result["warnings"]:
        print(f"WARNING: {warning}")
    errors = check(result, baseline, reference)
    if errors:
        raise RuntimeError("\n".join(errors))
    if result["warnings"]:
        print("PASS hard limits and stored memory; timing comparisons marked INCONCLUSIVE are not acceptance")
    else:
        print("PASS: core-text hard limits, <=10% balanced paired timing regression and stored private-memory regression")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as e:
        print(f"BENCH GATE FAILED: {e}", file=sys.stderr)
        sys.exit(1)
