"""Hosted-only A/A evidence. Completion never grants benchmark acceptance."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import statistics
import subprocess
import sys
import tempfile
import time

import run as production

ROUNDS = 9
OUTPUT_LIMIT = 2 << 20
RUSAGE_FIELDS = ("ru_utime", "ru_stime", "ru_maxrss", "ru_ixrss", "ru_idrss", "ru_isrss",
                 "ru_minflt", "ru_majflt", "ru_nswap", "ru_inblock", "ru_oublock", "ru_msgsnd",
                 "ru_msgrcv", "ru_nsignals", "ru_nvcsw", "ru_nivcsw")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def identities(a_dir, b_dir):
    suffix = ".exe" if os.name == "nt" else ""
    roles = {role: {name: {"path": str((directory / (name + suffix)).resolve()),
                           "sha256": digest(directory / (name + suffix))}
                    for name in ("kwil", "kwil-bench")}
             for role, directory in (("A", a_dir), ("B", b_dir))}
    for name in ("kwil", "kwil-bench"):
        if roles["A"][name]["sha256"] != roles["B"][name]["sha256"]:
            raise RuntimeError(f"A/A requires hash-identical {name} binaries")
    return roles


def paired_order():
    return [(iteration, role) for iteration in range(ROUNDS)
            for role in (("A", "B") if iteration % 2 == 0 else ("B", "A"))]


def linux_wait(child, pidfd, timeout):
    # A readable pidfd means exit, without reaping. wait4 returns this exact
    # child's counters; cumulative RUSAGE_CHILDREN/maxrss deltas would be wrong.
    timed_out = not select.select([pidfd], [], [], timeout)[0]
    if timed_out:
        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
    _, status, usage = os.wait4(child.pid, 0)
    child.returncode = os.waitstatus_to_exitcode(status)
    return timed_out, {"method": "wait4_exact_child", "maxrss_unit": "KiB",
                       **{name: getattr(usage, name) for name in RUSAGE_FIELDS},
                       "user_cpu_ms": usage.ru_utime * 1000,
                       "kernel_cpu_ms": usage.ru_stime * 1000}


class WindowsUsage:
    """Retain a handle to our child through exit; never identify it by name."""
    def __init__(self, pid):
        from ctypes import wintypes
        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        self.api.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.api.OpenProcess.restype = wintypes.HANDLE
        self.api.CloseHandle.argtypes = [wintypes.HANDLE]
        self.api.CloseHandle.restype = wintypes.BOOL
        self.handle = self.api.OpenProcess(0x1000, False, pid)  # QUERY_LIMITED_INFORMATION
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())

    def read(self):
        from ctypes import wintypes
        class IoCounters(ctypes.Structure):
            _fields_ = [(name, ctypes.c_ulonglong) for name in
                        ("read_operations", "write_operations", "other_operations",
                         "read_bytes", "write_bytes", "other_bytes")]
        self.api.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
        self.api.GetProcessTimes.restype = wintypes.BOOL
        self.api.GetProcessIoCounters.argtypes = [wintypes.HANDLE, ctypes.POINTER(IoCounters)]
        self.api.GetProcessIoCounters.restype = wintypes.BOOL
        created, exited, kernel, user = [wintypes.FILETIME() for _ in range(4)]
        io = IoCounters()
        if not self.api.GetProcessTimes(self.handle, ctypes.byref(created), ctypes.byref(exited),
                                        ctypes.byref(kernel), ctypes.byref(user)):
            raise ctypes.WinError(ctypes.get_last_error())
        if not self.api.GetProcessIoCounters(self.handle, ctypes.byref(io)):
            raise ctypes.WinError(ctypes.get_last_error())
        ticks = lambda value: (value.dwHighDateTime << 32) | value.dwLowDateTime
        return {"method": "GetProcessTimes+GetProcessIoCounters",
                "user_cpu_ms": ticks(user) / 10000, "kernel_cpu_ms": ticks(kernel) / 10000,
                "os_lifetime_ms": (ticks(exited) - ticks(created)) / 10000,
                "io": {name: getattr(io, name) for name, _ in IoCounters._fields_}}

    def close(self):
        self.api.CloseHandle(self.handle)


def read_output(stream):
    stream.seek(0)
    data = stream.read(OUTPUT_LIMIT + 1)
    if len(data) > OUTPUT_LIMIT:
        raise RuntimeError("diagnostic child output exceeded 2 MiB")
    return data.decode("utf-8", errors="replace")


def collect(command, timeout=60):
    """One exact child, bounded lifetime; no UI polling or descendant launches."""
    command = [str(arg) for arg in command]
    is_linux = sys.platform == "linux"
    if not is_linux and os.name != "nt":
        raise RuntimeError("A/A resource collector supports the two hosted runner OSes only")
    env = {**os.environ, "KWIL_NO_UPDATE": "1"}
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        wall_start = time.perf_counter_ns()
        launch_ns = time.clock_gettime_ns(time.CLOCK_BOOTTIME) if is_linux else None
        with subprocess.Popen(command, stdout=out, stderr=err, stdin=subprocess.DEVNULL, env=env) as child:
            returned_ns = time.clock_gettime_ns(time.CLOCK_BOOTTIME) if is_linux else None
            print(f"diagnostic started PID {child.pid}: {command[0]}", file=sys.stderr, flush=True)
            pidfd, windows = None, None
            try:
                clock = {"mode": "os_creation", "offset_ms": 0.0}
                if is_linux:
                    pidfd = os.pidfd_open(child.pid)
                    stat = Path(f"/proc/{child.pid}/stat").read_text()
                    ticks = int(stat.rsplit(")", 1)[1].split()[19])
                    origin_ns = ticks * 1_000_000_000 // os.sysconf("SC_CLK_TCK")
                    clock = {"mode": "parent_launch", "offset_ms": (launch_ns - origin_ns) / 1e6,
                             "spawn_span_ms": (returned_ns - launch_ns) / 1e6}
                    timed_out, usage = linux_wait(child, pidfd, timeout)
                else:
                    windows = WindowsUsage(child.pid)
                    timed_out = False
                    try:
                        child.wait(timeout=timeout)
                    except subprocess.TimeoutExpired:
                        timed_out = True
                        child.kill()
                        child.wait(timeout=10)
                    usage = windows.read()
                wall_ms = (time.perf_counter_ns() - wall_start) / 1e6
                return {"pid": child.pid, "command": command, "returncode": child.returncode,
                        "timed_out": timed_out, "parent_wall_ms": wall_ms,
                        "cpu_ms": usage["user_cpu_ms"] + usage["kernel_cpu_ms"],
                        "rusage": usage, "launch_clock": clock,
                        "stdout": read_output(out), "stderr": read_output(err)}
            finally:
                if child.returncode is None:
                    if pidfd is not None:
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        _, status, _ = os.wait4(child.pid, 0)
                        child.returncode = os.waitstatus_to_exitcode(status)
                    else:
                        child.kill()
                        child.wait(timeout=10)
                if pidfd is not None:
                    os.close(pidfd)
                if windows is not None:
                    windows.close()


def trace_marks(record):
    marks = {}
    for name in ("launch", "first_frame", "index_done"):
        match = re.search(rf"^t_{name}=([0-9.]+) ms$", record["stderr"], re.M)
        if not match:
            raise RuntimeError(f"diagnostic missing t_{name}")
        marks[name] = float(match[1])
    return production.correct_trace(marks, record["launch_clock"])


def save(output, report):
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


def sample(roles, role, iteration, log, report, output):
    result = {}
    app = roles[role]["kwil"]["path"]
    commands = [("text", [roles[role]["kwil-bench"]["path"], "measure", log]),
                ("file", [app, "--gui", "--hidden", "--trace-startup", log]),
                ("empty", [app, "--gui", "--hidden", "--trace-startup"])]
    for task, command in commands:
        record = collect(command)
        record.update(role=role, round=iteration, task=task,
                      executable_sha256=roles[role]["kwil-bench" if task == "text" else "kwil"]["sha256"])
        report["children"].append(record)
        save(output, report)  # Preserve even failed/partial rounds.
        if record["timed_out"] or record["returncode"]:
            raise RuntimeError(f"diagnostic {role}/{iteration}/{task} child failed")
        if task == "text":
            result.update(json.loads(record["stdout"]))
        else:
            trace = trace_marks(record)
            result.setdefault("startup_traces", {})[task] = trace
            result[f"startup_{task}_ms"] = trace["first_frame"]
            if task == "file":
                result["startup_index_ms"] = trace["index_done"]
    return result


def new_report(runner):
    return {"diagnostic_schema": 1, "diagnostic_only": True, "acceptance_eligible": False,
            "status": "incomplete", "runner": runner, "rounds_per_role": ROUNDS,
            "source_commit": os.environ.get("GITHUB_SHA"),
            "production_gate_outcome": os.environ.get("PRODUCTION_GATE_OUTCOME", "unknown"),
            "host": {"platform": sys.platform, "python": sys.version, "logical_cpus": os.cpu_count(),
                     "allowed_cpus": sorted(os.sched_getaffinity(0)) if sys.platform == "linux" else None,
                     "runner_arch": os.environ.get("RUNNER_ARCH"),
                     "image_version": os.environ.get("ImageVersion")},
            "children": [], "samples": {"A": [], "B": []}}


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("A/A diagnostic is hosted-CI-only; no local benchmark mode")
    report = new_report(args.runner)
    save(args.output, report)
    try:
        # Both roles execute the EXACT same files, not just equivalent sources.
        before = identities(args.bin_dir, args.bin_dir)
        report["executables"] = before
        log = args.fixtures / "log-1g.log"
        if log.stat().st_size < 1 << 30:
            raise RuntimeError("canonical 1 GiB fixture required")
        with log.open("rb") as stream:
            while stream.read(1 << 20):
                pass
        for iteration, role in paired_order():
            report["samples"][role].append(sample(before, role, iteration, log, report, args.output))
            save(args.output, report)
        if identities(args.bin_dir, args.bin_dir) != before:
            raise RuntimeError("A/A binaries changed during measurement")
        report["medians"] = {
            role: {metric: statistics.median(s[metric] for s in samples) for metric in production.LIMITS}
            for role, samples in report["samples"].items()}
        report["same_binary_cost_ratios_A_over_B"] = {
            metric: (report["medians"]["A"][metric] / report["medians"]["B"][metric]
                     if direction == "max" else report["medians"]["B"][metric] / report["medians"]["A"][metric])
            for metric, (direction, _) in production.LIMITS.items()}
        report["status"] = "complete"
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        save(args.output, report)
    print("DIAGNOSTIC ONLY: A/A evidence complete; no production acceptance decision")
    print(json.dumps(report["same_binary_cost_ratios_A_over_B"], indent=2))


if __name__ == "__main__":
    main()
