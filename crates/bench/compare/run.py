"""One fresh capture/diff process per size; multi-GB fixtures are hosted-only."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def run(command, log, timeout, commit):
    with log.open("w", encoding="utf-8") as output:
        output.write(json.dumps({"event": "commit", "commit": commit}) + "\n")
        output.flush()
        process = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT)
        print(f"PID {process.pid}: {command[1]}", flush=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=30)
            raise
    if code:
        raise RuntimeError(f"exit {code}; see {log}")
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sizes-mib", type=int, nargs="+", default=[1024, 2048])
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--require-complete", action="store_true")
    args = parser.parse_args()
    if any(size > 8 for size in args.sizes_mib) and os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("sizes above 8 MiB are hosted-only")
    if min(args.sizes_mib) < 1 or args.timeout < 1:
        parser.error("sizes and timeout must be positive")
    args.output.mkdir(parents=True, exist_ok=True)
    binary = str(args.binary.resolve())
    results = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "runner": os.environ.get("RUNNER_OS", os.name),
        "samples": [],
        "gated": False,
    }
    # Save even when an OS error, incorrect result or outer timeout aborts later.
    destination = args.output / "compare.json"
    destination.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    for size in args.sizes_mib:
        with tempfile.TemporaryDirectory(prefix="kwil-compare-", dir=os.environ.get("RUNNER_TEMP")) as temp:
            root = str(Path(temp) / "fixture")
            generation = time.monotonic()
            run([binary, "generate", root, str(size)], args.output / f"generate-{size}.jsonl", 180, results["commit"])
            generation = time.monotonic() - generation
            records = run([binary, "measure", root, str(args.timeout)],
                          args.output / f"measure-{size}.jsonl", args.timeout + 60, results["commit"])
            result = records[-1]
            if result.get("event") != "result":
                raise RuntimeError("missing terminal measurement")
            result["generation_seconds"] = generation
            results["samples"].append(result)
            destination.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
            print(json.dumps(result), flush=True)
            if args.require_complete and not result["complete"]:
                raise RuntimeError("small-fixture correctness smoke did not finish")
    summary = ["## External compare (informational)", "",
               "Fixture generation excluded; one fresh process per size, uncontrolled OS disk cache.", "",
               "| Bytes per file | Complete | Wall seconds | Peak RSS MiB | Peak private MiB (Windows) | Stop phase |",
               "|---:|:---:|---:|---:|---:|---|"]
    for sample in results["samples"]:
        memory = sample["memory"]
        private = memory["peak_private_bytes"]
        private = f"{private / 1048576:.2f}" if private is not None else "n/a"
        summary.append(f'| {sample["bytes_per_file"]} | {sample["complete"]} | '
                       f'{sample["wall_seconds"]:.3f} | {memory["peak_rss_bytes"] / 1048576:.2f} | '
                       f'{private} | {sample["stop_phase"]} |')
    summary.append("\nIncomplete means the cooperative time limit was reached; it is not a completed diff timing.")
    rendered = "\n".join(summary) + "\n"
    (args.output / "summary.md").write_text(rendered, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
            output.write(rendered)


if __name__ == "__main__":
    main()
