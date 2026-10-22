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
import time

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
    command = [str(a) for a in args]
    env = {**os.environ, "KWIL_NO_UPDATE": "1"}
    linux_trace = sys.platform == "linux" and "--trace-startup" in command
    launched_ns = time.clock_gettime_ns(time.CLOCK_BOOTTIME) if linux_trace else None
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, encoding="utf-8", errors="replace", env=env, **kwargs) as child:
        clock = {"mode": "os_creation", "offset_ms": 0.0}
        if linux_trace:
            returned_ns = time.clock_gettime_ns(time.CLOCK_BOOTTIME)
            stat = Path(f"/proc/{child.pid}/stat").read_text()
            ticks = int(stat.rsplit(")", 1)[1].split()[19])
            os_start_ns = ticks * 1_000_000_000 // os.sysconf("SC_CLK_TCK")
            clock = {"mode": "parent_launch", "offset_ms": (launched_ns - os_start_ns) / 1e6,
                     "spawn_span_ms": (returned_ns - launched_ns) / 1e6}
        print(f"started PID {child.pid}: {args[0]}", file=sys.stderr)
        try:
            out, err = child.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate()
            raise
        if child.returncode:
            raise RuntimeError(f"{args}: exit {child.returncode}\n{out}\n{err}")
        completed = subprocess.CompletedProcess(args, child.returncode, out, err)
        completed.launch_clock = clock
        return completed


def correct_trace(marks, clock):
    # Linux stat floors creation to a 10 ms tick on hosted kernels. Subtract
    # that common offset to express all marks since the high-resolution parent
    # launch request. This includes spawn overhead, so it is conservative.
    corrected = {k: value - clock["offset_ms"] for k, value in marks.items()}
    if corrected["launch"] < 0 or corrected["first_frame"] < corrected["launch"] or corrected["index_done"] < corrected["launch"]:
        raise RuntimeError(f"invalid process clock: {corrected}")
    corrected["clock"] = {**clock, "raw_os_marks": marks}
    return corrected


def trace(app, file=None):
    args = [app, "--gui", "--hidden", "--trace-startup"]
    if file:
        args.append(file)
    completed = run(args)
    output = completed.stderr
    marks = {}
    for name in ("launch", "first_frame", "index_done"):
        match = re.search(rf"^t_{name}=([0-9.]+) ms$", output, re.M)
        if not match:
            raise RuntimeError(f"missing t_{name}: {output}")
        marks[name] = float(match[1])
    return correct_trace(marks, completed.launch_clock)


def check(current, baseline, reference=None):
    errors = []
    if baseline.get("schema") != SCHEMA or baseline.get("runner") != current["runner"] or baseline.get("fixture") != current["fixture"]:
        return ["baseline schema, runner OS/image or fixture mismatch"]
    if reference is not None and any(reference.get(k) != current.get(k) for k in ("schema", "runner", "fixture")):
        return ["paired reference schema, runner OS/image or fixture mismatch"]
    for metric, (direction, limit) in LIMITS.items():
        value = current["medians"][metric]
        # CPU/host speed calibrates timings; private bytes retain their stored
        # OS comparison. A timing calibration cannot waive memory growth.
        comparison = reference if reference is not None and metric != "private_mb" else baseline
        before = comparison["medians"].get(metric)
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


def sample(bin_dir, log, text_only):
    suffix = ".exe" if os.name == "nt" else ""
    result = json.loads(run([bin_dir / ("kwil-bench" + suffix), "measure", log]).stdout)
    if not text_only:
        app = bin_dir / ("kwil" + suffix)
        file_trace = trace(app, log)
        empty_trace = trace(app)
        result.update(startup_file_ms=file_trace["first_frame"],
                      startup_empty_ms=empty_trace["first_frame"],
                      startup_index_ms=file_trace["index_done"],
                      startup_traces={"file": file_trace, "empty": empty_trace})
    return result


def summarize(samples, runner, commit):
    return {"schema": SCHEMA, "runner": runner, "fixture": "v1", "commit": commit,
            "runs": len(samples), "samples": samples,
            "medians": {k: statistics.median(s[k] for s in samples) for k, v in samples[0].items()
                        if isinstance(v, (int, float))}}


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--fixtures", type=Path, required=True)
    p.add_argument("--runner", required=True)
    p.add_argument("--runs", type=int, default=5, choices=range(3, 10, 2))
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--baseline", type=Path)
    p.add_argument("--reference-bin-dir", type=Path)
    p.add_argument("--reference-source", type=Path)
    p.add_argument("--reference-manifest", type=Path)
    p.add_argument("--text-only", action="store_true")
    a = p.parse_args()
    suffix = ".exe" if os.name == "nt" else ""
    app = a.bin_dir / ("kwil" + suffix)
    samples = []
    reference_samples = []
    reference_commit = None
    if any((a.reference_bin_dir, a.reference_source, a.reference_manifest)):
        if not all((a.reference_bin_dir, a.reference_source, a.reference_manifest)):
            raise RuntimeError("paired measurement requires binary directory, source and trusted manifest")
        expected = json.loads(a.reference_manifest.read_text(encoding="utf-8-sig"))["source_head"]
        reference_commit = run(["git", "-C", a.reference_source, "rev-parse", "HEAD"]).stdout.strip()
        if not re.fullmatch("[0-9a-f]{40}", expected) or reference_commit != expected:
            raise RuntimeError("reference checkout does not match the trusted baseline source")
    log = a.fixtures / "log-1g.log"
    if log.stat().st_size < 1 << 30:
        raise RuntimeError("canonical 1 GiB fixture required")
    # Warm the OS file cache outside timed samples, without allocating a file-sized buffer.
    with log.open("rb") as f:
        while f.read(1 << 20):
            pass
    for iteration in range(a.runs):
        order = [(a.bin_dir, samples)]
        if a.reference_bin_dir:
            order.append((a.reference_bin_dir, reference_samples))
            if iteration % 2:
                order.reverse()
        for bin_dir, bucket in order:
            bucket.append(sample(bin_dir, log, a.text_only))
    result = summarize(samples, a.runner, run(["git", "rev-parse", "HEAD"]).stdout.strip())
    reference = None
    if reference_samples:
        reference = summarize(reference_samples, a.runner, reference_commit)
        result["reference"] = reference
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result["medians"], indent=2))
    if reference:
        print("Paired baseline-code medians:", json.dumps(reference["medians"], indent=2))
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
        baseline = json.loads(a.baseline.read_text(encoding="utf-8-sig"))
        result["stored_baseline"] = baseline
        a.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        if reference is None:
            raise RuntimeError("paired baseline-code measurements required; no literal-number fallback")
        errors = check(result, baseline, reference)
        if errors:
            raise RuntimeError("\n".join(errors))
        print("PASS: all §3 hard limits, <=10% timing regression against paired OS baseline code, and stored private-memory regression limit")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        print(f"BENCH GATE FAILED: {e}", file=sys.stderr)
        sys.exit(1)
