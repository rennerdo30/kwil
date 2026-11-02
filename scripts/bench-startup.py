"""One bounded hosted sample per startup case; targets are informational."""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import uuid

if os.name == "nt":
    import ctypes
    from ctypes import wintypes
    import _winapi

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    kernel32.GetProcessTimes.restype = wintypes.BOOL


class Editor:
    def __init__(self, binary, root, output, name, files=()):
        self.events = []
        self.inbox = queue.Queue()
        self.log = (output / f"{name}.log").open("w", encoding="utf-8")
        self.log.write(f"commit={os.environ.get('GITHUB_SHA', 'local')} case={name}\n")
        self.log.flush()
        worker_trace = output / f"{name}-worker.log"
        worker_trace.write_text(f"commit={os.environ.get('GITHUB_SHA', 'local')} case={name}\n", encoding="utf-8")
        env = dict(os.environ, KWIL_NO_UPDATE="1", KWIL_TRACE_STARTUP="1",
                   KWIL_WORKSPACE_ROOT=str(root), XDG_RUNTIME_DIR=str(root),
                   XDG_CONFIG_HOME=str(root / "config"), APPDATA=str(root / "config"),
                   HOME=str(root), KWIL_INSTANCE_NAMESPACE="kwil-test-" + root.parent.name[-8:] + "-" + root.name)
        env["KWIL_WORKER_TRACE"] = str(worker_trace)
        self.spawn = {}
        if os.name == "nt":
            original_create = _winapi.CreateProcess

            def timed_create(*args, **kwargs):
                self.spawn["create_process_enter_ns"] = time.time_ns()
                try:
                    return original_create(*args, **kwargs)
                finally:
                    self.spawn["create_process_return_ns"] = time.time_ns()

            # Only this harness's main thread spawns. Restore the native API
            # before starting its trace-reader thread, including on failure.
            _winapi.CreateProcess = timed_create
        try:
            self.started = time.time_ns()
            self.process = subprocess.Popen([str(binary), "--hidden", "--gui", "--renderer", "cpu", *map(str, files)],
                                        env=env, cwd=root, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                                        text=True, encoding="utf-8", errors="replace")
            self.spawn["popen_return_ns"] = time.time_ns()
        finally:
            if os.name == "nt":
                _winapi.CreateProcess = original_create
        self.spawn["popen_enter_ns"] = self.started
        if os.name == "nt":
            created, exited, kernel, user = (wintypes.FILETIME() for _ in range(4))
            if kernel32.GetProcessTimes(int(self.process._handle), ctypes.byref(created), ctypes.byref(exited),
                                       ctypes.byref(kernel), ctypes.byref(user)):
                ticks = (created.dwHighDateTime << 32) | created.dwLowDateTime
                self.spawn["native_process_created_ns"] = (ticks - 116444736000000000) * 100
        self.log.write("kwil-spawn " + json.dumps(self.spawn) + "\n")
        self.log.flush()
        print(f"case={name} pid={self.process.pid} timeout=30s", flush=True)
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        for line in self.process.stderr:
            self.log.write(line)
            self.log.flush()
            if line.startswith("kwil-speed\t"):
                _, pid, ns, mark = line.strip().split("\t", 3)
                self.inbox.put({"pid": int(pid), "ns": int(ns), "mark": mark})
        self.inbox.put(None)

    def wait(self, mark):
        for event in self.events:
            if event["mark"] == mark:
                return event["ns"]
        deadline = time.monotonic() + 30
        while True:
            try:
                event = self.inbox.get(timeout=max(0, deadline - time.monotonic()))
            except queue.Empty as error:
                raise RuntimeError(f"missing {mark}; see {self.log.name}") from error
            if event is None:
                raise RuntimeError(f"editor exited before {mark}; see {self.log.name}")
            self.events.append(event)
            if event["mark"] == mark:
                return event["ns"]

    def close(self):
        # Only this Popen's recorded PID; isolated files contain no edits.
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=10)
        self.reader.join(timeout=10)
        if self.reader.is_alive():
            raise RuntimeError("trace reader did not finish")
        self.process.stderr.close()
        self.log.close()


def measure(binary, output):
    output.mkdir(parents=True, exist_ok=True)
    # Deliberately retain the isolated profile with the raw logs. No recursive
    # cleanup, cache flushing, machine configuration or global input is needed.
    # macOS's default per-user temp path can exceed sockaddr_un's 104-byte limit.
    root = Path(tempfile.mkdtemp(prefix="kwil-speed-" + uuid.uuid4().hex[:8],
                                 dir=None if os.name == "nt" else "/tmp")).resolve()
    root.chmod(0o700)
    fixture = root / "file.txt"
    fixture.write_bytes(b"kwil-speed-file\n" + b"plain text content\n" * 4096)
    large = root / "large.log"
    with large.open("wb") as stream:
        stream.write(b"kwil-speed-file\n")
        chunk = b"plain text content\n" * 32768
        while stream.tell() < 1024**3:
            stream.write(chunk[:min(len(chunk), 1024**3 - stream.tell())])
    forwarded = root / "forward.txt"
    forwarded.write_bytes(b"kwil-speed-forward\nsecond process delivery\n")
    records = {}
    active = []
    failure = None
    try:
        for name, files in [("empty-first", ()), ("file-first", (fixture,)), ("empty-warm", ()),
                            ("file-warm", (fixture,)), ("file-large", (large,))]:
            profile = root / name
            profile.mkdir(mode=0o700)
            (profile / ".kwil").mkdir()
            (profile / ".kwil/welcome-v1.seen").write_bytes(b"kwil-welcome-v1\n")
            editor = Editor(binary, profile, output, name, files)
            active.append(editor)
            first = editor.wait("first frame presented")
            record = {"first_frame_ms": (first - editor.started) / 1e6,
                      "content_ms": None,
                      "file_bytes": files[0].stat().st_size if files else 0,
                      "spawn": editor.spawn,
                      "stages": editor.events}
            records[name] = record
            content = editor.wait("file content presented") if files else None
            record["content_ms"] = (content - editor.started) / 1e6 if content else None
            ready = editor.wait("IPC UI ready")
            if name == "file-warm":
                # The relay's readiness mark comes after installation, not after
                # a guessed delay. Wait for displayed row in the original PID.
                second = Editor(binary, profile, output, "second-launch", (forwarded,))
                active.append(second)
                acknowledged = second.wait("IPC acknowledged")
                shown = editor.wait("forward content presented")
                second.process.wait(timeout=10)
                if second.process.returncode:
                    raise RuntimeError("second launch failed")
                records["second-launch"] = {
                    "acknowledged_ms": (acknowledged - second.started) / 1e6,
                    "content_ms": (shown - second.started) / 1e6,
                    "spawn": second.spawn,
                    "stages": second.events,
                }
                second.close()
                active.remove(second)
            record["ready_ms"] = (ready - editor.started) / 1e6
            editor.close()
            active.remove(editor)
    except Exception as error:
        failure = str(error)
        raise
    finally:
        for editor in reversed(active):
            editor.close()
        report = {"sha": os.environ.get("GITHUB_SHA", "local"), "cases": records,
                  "error": failure,
                  "profile": "release CPU presenter; hidden native window; one sample per case",
                  "cache": "first invocation and repeat, OS disk cache uncontrolled; not cache-flushed cold boot",
                  "isolated_root": str(root)}
        (output / "startup.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        if "GITHUB_STEP_SUMMARY" in os.environ:
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
                summary.write("\n### Startup and second launch (informational)\n\n")
                if failure:
                    summary.write(f"Measurement unavailable/incomplete: {failure}\n\n")
                summary.write("| Case | First frame ms | File content ms | Target / hard ms |\n|---|---:|---:|---|\n")
                for name, record in records.items():
                    target = "20 / 50" if name == "second-launch" else "40 / 100" if "file" in name else "25 / 60"
                    summary.write(f"| {name} | {record.get('first_frame_ms', '-')} | {record['content_ms']} | {target} |\n")
                summary.write("\nOne sample; hidden release CPU presentation, not compositor scanout. "
                              "First invocation and repeat; OS disk cache uncontrolled. "
                              "Content requires the exact fixture's first row to be drawn and successfully presented. "
                              "Raw stage timestamps and second-process IPC acknowledgment are retained in startup.json.\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("startup measurements run only on hosted CI")
    measure(args.binary.resolve(), args.output.resolve())
