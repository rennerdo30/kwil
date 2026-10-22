"""Bounded fresh-process measurements and fail-closed OS baseline gate (stdlib only)."""
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
    "startup_file_ms": ("max", 100),
    "startup_empty_ms": ("max", 60),
    # Index completion has a measured OS baseline but no independent §3 launch limit.
    "startup_index_ms": ("max", None),
}


def run(args, **kwargs):
    with subprocess.Popen([str(a) for a in args], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, env={**os.environ, "KWIL_NO_UPDATE": "1"}, **kwargs) as child:
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


def trace(app, file=None):
    args = [app, "--gui", "--hidden", "--trace-startup"]
    if file:
        args.append(file)
    output = run(args).stderr
    marks = {}
    for name in ("launch", "first_frame", "index_done"):
        match = re.search(rf"^t_{name}=([0-9.]+) ms$", output, re.M)
        if not match:
            raise RuntimeError(f"missing t_{name}: {output}")
        marks[name] = float(match[1])
    if marks["first_frame"] < marks["launch"] or marks["index_done"] < marks["launch"]:
        raise RuntimeError(f"invalid process clock: {marks}")
    return marks


def check(current, baseline):
    errors = []
    if baseline.get("schema") != SCHEMA or baseline.get("runner") != current["runner"] or baseline.get("fixture") != current["fixture"]:
        return ["baseline schema, runner OS/image or fixture mismatch"]
    for metric, (direction, limit) in LIMITS.items():
        value = current["medians"][metric]
        before = baseline["medians"].get(metric)
        if not isinstance(value, (float, int)) or not math.isfinite(value) or value <= 0:
            errors.append(f"invalid measurement: {metric}")
            continue
        if before is None or not isinstance(before, (float, int)) or not math.isfinite(before) or before <= 0:
            errors.append(f"missing/invalid baseline: {metric}")
            continue
        if limit is not None and ((direction == "max" and value > limit) or (direction == "min" and value < limit)):
            errors.append(f"hard limit: {metric}={value:.4f}, {direction} {limit}")
        # Throughput falls on regression; compare time per byte (inverse GB/s).
        ratio = value / before if direction == "max" else before / value
        if ratio > 1.10:
            errors.append(f"regression: {metric}={value:.4f}, baseline={before:.4f}, cost +{(ratio-1)*100:.1f}% >10%")
    return errors


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--fixtures", type=Path, required=True)
    p.add_argument("--runner", required=True)
    p.add_argument("--runs", type=int, default=5, choices=range(3, 10, 2))
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--baseline", type=Path)
    p.add_argument("--text-only", action="store_true")
    a = p.parse_args()
    suffix = ".exe" if os.name == "nt" else ""
    bench = a.bin_dir / ("kwil-bench" + suffix)
    app = a.bin_dir / ("kwil" + suffix)
    samples = []
    log = a.fixtures / "log-1g.log"
    if log.stat().st_size < 1 << 30:
        raise RuntimeError("canonical 1 GiB fixture required")
    # Warm the OS file cache outside timed samples, without allocating a file-sized buffer.
    with log.open("rb") as f:
        while f.read(1 << 20):
            pass
    for _ in range(a.runs):
        sample = json.loads(run([bench, "measure", log]).stdout)
        if not a.text_only:
            file_trace = trace(app, log)
            empty_trace = trace(app)
            sample.update(startup_file_ms=file_trace["first_frame"],
                          startup_empty_ms=empty_trace["first_frame"],
                          startup_index_ms=file_trace["index_done"])
        samples.append(sample)
    keys = samples[0].keys()
    result = {"schema": SCHEMA, "runner": a.runner, "fixture": "v1",
              "commit": run(["git", "rev-parse", "HEAD"]).stdout.strip(),
              "runs": a.runs, "samples": samples,
              "medians": {k: statistics.median(s[k] for s in samples) for k in keys}}
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result["medians"], indent=2))
    if not a.text_only:
        # Separate process, after startup samples; never pollutes launch timing.
        frames = run([app, "--gui", "--hidden", "--frame-script", log]).stdout
        print(frames)
        result["frames"] = json.loads(frames)
        a.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    if a.baseline:
        if a.text_only:
            raise RuntimeError("a gate cannot omit startup")
        errors = check(result, result)
        if errors:
            raise RuntimeError("\n".join(errors))
        if not a.baseline.is_file():
            raise RuntimeError("baseline missing; results saved, reviewed explicit seeding required")
        errors = check(result, json.loads(a.baseline.read_text(encoding="utf-8-sig")))
        if errors:
            raise RuntimeError("\n".join(errors))
        print("PASS: all §3 hard limits and <=10% cost regression against OS baseline")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        print(f"BENCH GATE FAILED: {e}", file=sys.stderr)
        sys.exit(1)
