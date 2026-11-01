"""Hosted-only build measurements and GitHub-artifact weekly history (KW-35)."""
import argparse
import datetime as dt
import io
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import time
import zipfile


def command(args, **kwargs):
    return subprocess.check_output(args, text=True, timeout=120, **kwargs).strip()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def summary(text):
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as out:
        out.write(text + "\n")


def size_bytes(path):
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def timed_build(target, output, phase):
    args = ["cargo", "build", "--workspace", "--locked", "--offline", "--timings", "-j", "4"]
    started = time.perf_counter()
    with (output / f"{phase}.log").open("w", encoding="utf-8") as log:
        log.write(f"commit={command(['git', 'rev-parse', 'HEAD'])} phase={phase}\n")
        log.flush()
        process = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=os.name != "nt")
        print(f"phase={phase} cargo_pid={process.pid} timeout_seconds=1200", flush=True)
        try:
            result = process.wait(timeout=1200)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                               timeout=30, check=False)
            else:
                os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=30)
            raise
    elapsed = time.perf_counter() - started
    if result:
        raise RuntimeError(f"{phase} failed ({result}); see {phase}.log")
    shutil.copyfile(target / "cargo-timings/cargo-timing.html", output / f"{phase}.html")
    return {"seconds": round(elapsed, 3), "target_bytes": size_bytes(target)}


def measure(os_label):
    # Never accidentally turn a local invocation into a measurement build.
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("Build measurement is hosted CI only")
    target = Path(os.environ["CARGO_TARGET_DIR"]).resolve()
    if target.exists():
        raise RuntimeError(f"Measurement requires a fresh target, refusing to clean {target}")
    if os.environ.get("RUSTC_WRAPPER") or os.environ.get("CARGO_INCREMENTAL") != "1":
        raise RuntimeError("Measurement requires no compiler wrapper and incremental=1")
    output = Path("target/build-metrics-report")
    output.mkdir(parents=True, exist_ok=True)
    record = {
        "schema": 1, "os": os_label, "sha": command(["git", "rev-parse", "HEAD"]),
        "run_id": os.environ["GITHUB_RUN_ID"],
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "1"),
        "toolchain": command(["rustc", "--version"]),
        "runner_image": os.environ.get("ImageVersion", "unknown"),
        "profile": "dev; incremental=1; line-tables-only; jobs=4; no target or compiler cache",
        "measurements": {},
    }
    write_json(output / "metrics.json", record)
    # Network/registry work is deliberately outside the timed clean build.
    subprocess.run(["cargo", "fetch", "--locked"], timeout=300, check=True)
    phases = record["measurements"]
    try:
        phases["clean"] = timed_build(target, output, "clean")
        write_json(output / "metrics.json", record)
        for crate in ("core-text", "ui-gpu"):
            source = Path(f"crates/{crate}/src/lib.rs")
            original = source.read_bytes()
            try:
                # A deterministic source edit, not a no-op fresh build. This
                # invalidates the crate and its dependents without changing APIs.
                source.write_bytes(original + b"\nconst _: u8 = 1;\n")
                phases[crate] = timed_build(target, output, crate)
                write_json(output / "metrics.json", record)
            finally:
                source.write_bytes(original)
            if crate == "core-text":
                # Restore the baseline before editing UI; otherwise the UI
                # measurement would also include restoration of core-text.
                timed_build(target, output, "restore-core-text")
    finally:
        write_json(output / "metrics.json", record)
        text = f"## Build metrics: {os_label}\n\nCommit `{record['sha']}`\n\n"
        text += "| Build | Seconds | Target bytes |\n|---|---:|---:|\n"
        for phase, values in phases.items():
            text += f"| {phase} | {values['seconds']:.3f} | {values['target_bytes']} |\n"
        text += ("\nFresh target, no compiler cache, four compile jobs. Cargo fetch excluded. "
                 "Incremental samples add a private anonymous constant in core-text/ui-gpu, "
                 "then build the workspace. Target bytes are logical file sizes, including "
                 "incremental state. Hosted timings are observations, not owner-PC budget gates.\n")
        (output / "summary.md").write_text(text, encoding="utf-8")
        summary(text)


def api(path):
    return json.loads(command(["gh", "api", path]))


def job_times(jobs):
    rows = []
    for job in jobs:
        if job["completed_at"] is None or job["name"] == "CI wall time and weekly trend":
            continue
        start = dt.datetime.fromisoformat(job["started_at"].replace("Z", "+00:00"))
        end = dt.datetime.fromisoformat(job["completed_at"].replace("Z", "+00:00"))
        rows.append({"name": job["name"], "seconds": (end - start).total_seconds(),
                     "conclusion": job["conclusion"]})
    return rows


def wall_time(jobs, validation_only=False):
    completed = [job for job in jobs if job["completed_at"] is not None
                 and job["name"] != "CI wall time and weekly trend"
                 and (not validation_only or not job["name"].startswith("build metrics ("))]
    if not completed:
        return None
    def instant(value):
        return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    start = min(instant(job["started_at"]) for job in completed)
    end = max(instant(job["completed_at"]) for job in completed)
    return (end - start).total_seconds()


def weekly(records):
    buckets = {}
    for record in records:
        date = dt.datetime.fromisoformat(record["created_at"].replace("Z", "+00:00"))
        year, week, _ = date.isocalendar()
        key = f"{year}-W{week:02d}"
        for build in record["builds"]:
            samples = buckets.setdefault((key, build["os"]), {})
            for phase, values in build["measurements"].items():
                for unit, value in values.items():
                    samples.setdefault(f"{phase}_{unit}", []).append(value)
            for job in record["jobs"]:
                if job["name"] == f"test ({build['os']})":
                    samples.setdefault("test_job_seconds", []).append(job["seconds"])
    return [{"week": week, "os": os_label,
             "samples": {key: len(values) for key, values in samples.items()},
             "medians": {key: round(statistics.median(values), 3) for key, values in samples.items()}}
            for (week, os_label), samples in sorted(buckets.items())]


def report():
    repo = os.environ["GITHUB_REPOSITORY"]
    run_id = os.environ["GITHUB_RUN_ID"]
    root = f"repos/{repo}/actions"
    run = api(f"{root}/runs/{run_id}")
    jobs = api(f"{root}/runs/{run_id}/jobs?filter=latest&per_page=100")["jobs"]
    builds = [json.loads(p.read_text(encoding="utf-8"))
              for p in sorted(Path("target/metrics-input").rglob("metrics.json"))]
    record = {"schema": 1, "run_id": run_id,
              "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "1"),
              "sha": command(["git", "rev-parse", "HEAD"]), "head_sha": run["head_sha"],
              "created_at": run["created_at"], "event": run["event"],
              "jobs": job_times(jobs), "builds": builds,
              "validation_wall_seconds": wall_time(jobs, validation_only=True),
              "measurement_inclusive_wall_seconds": wall_time(jobs)}
    query = f"{root}/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=10"
    if os.environ.get("CI_BASE_SHA"):
        query += f"&head_sha={os.environ['CI_BASE_SHA']}"
    baselines = api(query)["workflow_runs"]
    baseline = next((item for item in baselines if str(item["id"]) != run_id), None)
    if baseline:
        old_jobs = api(f"{root}/runs/{baseline['id']}/jobs?filter=latest&per_page=100")["jobs"]
        record["baseline"] = {"run_id": baseline["id"], "sha": baseline["head_sha"],
                              "url": baseline["html_url"], "jobs": job_times(old_jobs),
                              "validation_wall_seconds": wall_time(old_jobs, validation_only=True)}
    # History comes only from completed successful main runs, never PR artifacts.
    # Keep at most one sample per ISO week (plus current) to bound API/downloads.
    history = []
    weeks = set()
    warnings = []
    cutoff = dt.datetime.now(dt.timezone.utc) - dt.timedelta(weeks=8)
    for page in range(1, 6):
        artifacts = api(f"{root}/artifacts?name=ci-metrics&per_page=100&page={page}")["artifacts"]
        if not artifacts:
            break
        for artifact in artifacts:
            if artifact["expired"] or str(artifact["workflow_run"]["id"]) == run_id:
                continue
            created = dt.datetime.fromisoformat(artifact["created_at"].replace("Z", "+00:00"))
            if created < cutoff:
                continue
            week = created.isocalendar()[:2]
            if week in weeks or artifact["workflow_run"]["head_branch"] != "main":
                continue
            previous = api(f"{root}/runs/{artifact['workflow_run']['id']}")
            if previous["conclusion"] != "success" or previous["event"] == "pull_request":
                continue
            try:
                data = subprocess.check_output(["gh", "api", f"{root}/artifacts/{artifact['id']}/zip"],
                                               timeout=60)
                with zipfile.ZipFile(io.BytesIO(data)) as archive:
                    sample = json.loads(archive.read("metrics.json"))
                if sample["schema"] != 1:
                    continue
                history.append(sample)
                weeks.add(week)
            except (subprocess.CalledProcessError, KeyError, ValueError, zipfile.BadZipFile) as error:
                warnings.append(f"Could not read historical artifact {artifact['id']}: {type(error).__name__}")
    record["weekly_trend"] = weekly(history + [record])
    record["history_warnings"] = warnings
    output = Path("target/ci-metrics-report")
    write_json(output / "metrics.json", record)
    text = f"## CI wall time\n\nCommit `{record['sha']}`; [run]({run['html_url']}).\n\n"
    text += (f"Validation wall time: **{record['validation_wall_seconds']} s**. "
             f"Including build measurement jobs: **{record['measurement_inclusive_wall_seconds']} s**. "
             "Spans first job start to last job completion; excludes this reporter and queue time.\n\n")
    text += "| Job (includes setup and cache post steps) | Seconds | Result |\n|---|---:|---|\n"
    for job in record["jobs"]:
        text += f"| {job['name']} | {job['seconds']:.0f} | {job['conclusion']} |\n"
    if "baseline" in record:
        baseline = record["baseline"]
        text += (f"\n### Before/after hosted test jobs\n\nBaseline `{baseline['sha']}` "
                 f"([run]({baseline['url']})); PR base commit when available, otherwise latest green main. "
                 "Single-run observations include cache/runner variation.\n\n"
                 "| Job | Before s | After s | Change s |\n|---|---:|---:|---:|\n")
        before = {job["name"]: job for job in baseline["jobs"]}
        for job in record["jobs"]:
            old = before.get(job["name"])
            if job["name"].startswith("test (") and old:
                text += (f"| {job['name']} | {old['seconds']:.0f} | {job['seconds']:.0f} | "
                         f"{job['seconds'] - old['seconds']:+.0f} |\n")
        text += (f"\nValidation wall time: {baseline['validation_wall_seconds']} s before; "
                 f"{record['validation_wall_seconds']} s after. Separate measurement-job cost is shown above.\n")
    text += ("\n## Weekly build trend\n\nLatest successful main sample per week plus this run; "
             "medians when both fall in the same week. First run seeds history. "
             "Artifacts retain 90 days; up to eight weeks / 500 recent artifacts are read.\n\n"
             "| ISO week | OS | Clean s | Core-text edit s | UI edit s | Clean target MiB | Test job s |\n"
             "|---|---|---:|---:|---:|---:|---:|\n")
    for row in record["weekly_trend"]:
        values = row["medians"]
        def cell(key, divisor=1):
            return f"{values[key] / divisor:.1f}" if key in values else "—"
        text += (f"| {row['week']} | {row['os']} | {cell('clean_seconds')} | "
                 f"{cell('core-text_seconds')} | {cell('ui-gpu_seconds')} | "
                 f"{cell('clean_target_bytes', 1024**2)} | {cell('test_job_seconds')} |\n")
    text += "\n" + "\n".join(warnings) + "\n"
    (output / "summary.md").write_text(text, encoding="utf-8")
    summary(text)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="mode", required=True)
    build_parser = subparsers.add_parser("build")
    build_parser.add_argument("--os", required=True)
    subparsers.add_parser("report")
    args = parser.parse_args()
    if args.mode == "build":
        measure(args.os)
    else:
        report()
