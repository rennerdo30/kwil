"""Bounded real-binary hot-exit recovery after TerminateProcess / SIGKILL.

Build with kwil-ui-gpu/frame-harness. Linux needs X11 (CI uses xvfb-run).
Keeps isolated profiles, captures, PID receipts and logs in the output folder.
"""
import argparse
import itertools
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import threading
import time


NEXT = itertools.count()
TIMEOUT = 45
ORIGINAL = b"original on disk\n"
CHANGED = b"changed by another program after the kill\n"


class Editor:
    def __init__(self, binary, root, phase, files=()):
        self.output = root / phase
        self.output.mkdir()
        self.log = (self.output / "process.log").open("wb")
        self.log.write(f"commit={os.environ.get('GITHUB_SHA', 'local')} phase={phase}\n".encode())
        self.log.flush()
        # Inherited probe/instance/profile options must never steer this test
        # into the owner's session, settings or update store.
        env = {key: value for key, value in os.environ.items() if not key.startswith("KWIL_")}
        env.update(KWIL_NO_UPDATE="1", KWIL_FRAME_SCRIPT="1",
                   KWIL_SESSION_PROBE="kill-seed" if phase == "seed" else "kill-restore",
                   KWIL_CAPTURE_DIR=str(self.output), KWIL_WORKSPACE_ROOT=str(root / "workspace"),
                   HOME=str(root), USERPROFILE=str(root), APPDATA=str(root / "config"),
                   LOCALAPPDATA=str(root / "local"), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), XDG_STATE_HOME=str(root / "state"),
                   XDG_CACHE_HOME=str(root / "cache"), XDG_RUNTIME_DIR=str(root / "runtime"))
        self.process = subprocess.Popen(
            [str(binary), "--hidden", "--gui", "--renderer", "cpu", *map(str, files)],
            cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
        )
        # Also bounds work between readiness checks and exception cleanup.
        self.timer = threading.Timer(TIMEOUT, self.process.kill)
        self.timer.daemon = True
        self.timer.start()
        self.receipt = {"pid": self.process.pid, "phase": phase, "timeout_seconds": TIMEOUT}
        self.record()
        print(f"started own PID {self.process.pid}, phase={phase}, timeout={TIMEOUT}s", flush=True)

    def record(self):
        (self.output / "pid.json").write_text(json.dumps(self.receipt, indent=2) + "\n", encoding="utf-8")

    def wait(self, marker):
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            failure = self.output / "harness-failure.txt"
            if failure.exists():
                raise AssertionError(f"PID {self.process.pid}: {failure.read_text(encoding='utf-8')}")
            if self.process.poll() is not None:
                raise AssertionError(f"PID {self.process.pid} exited before {marker}; see {self.log.name}")
            if (self.output / marker).is_file():
                return
            time.sleep(0.05)
        raise AssertionError(f"PID {self.process.pid} timed out waiting for {marker}; see {self.log.name}")

    def kill(self):
        try:
            assert self.process.poll() is None, "process must still be alive at the forced kill"
            # Popen.kill uses TerminateProcess on Windows and SIGKILL on Unix.
            # It targets this Popen's handle/PID, never a name or window title.
            self.process.kill()
            code = self.process.wait(timeout=10)
            assert code != 0 if os.name == "nt" else code == -signal.SIGKILL, code
            self.receipt.update(kill="TerminateProcess" if os.name == "nt" else "SIGKILL", exit_code=code)
            self.record()
            print(f"hard-killed own PID {self.process.pid}, exit={code}", flush=True)
        finally:
            self.close()

    def close(self):
        self.timer.cancel()
        self.timer.join(timeout=1)
        if self.process.poll() is None:
            self.process.kill()
            self.receipt["cleanup_kill"] = "TerminateProcess" if os.name == "nt" else "SIGKILL"
        self.receipt["exit_code"] = self.process.wait(timeout=10)
        self.record()
        self.log.close()


def scenario(binary, output, changed, negative_control):
    case = "changed-disk" if changed else "matching-disk"
    root = output / f"{case}-{os.getpid()}-{next(NEXT)}"
    root.mkdir(mode=0o700)
    for folder in ["workspace/.kwil", "config/kwil", "local", "runtime"]:
        (root / folder).mkdir(parents=True, exist_ok=True, mode=0o700)
    (root / "workspace/.kwil/welcome-v1.seen").write_bytes(b"kwil-welcome-v1\n")
    (root / "config/kwil/settings.toml").write_text(
        "[files]\nrestore_session = true\n[update]\ncheck = false\n", encoding="utf-8")
    source = root / "notes.txt"
    source.write_bytes(ORIGINAL)
    seed = Editor(binary, root, "seed", (source,))
    try:
        seed.wait("durable.txt")
        assert source.read_bytes() == ORIGINAL, "unsaved edits must not save the source"
        seed.kill()
    finally:
        seed.close()
    # Capture exactly what survived the kill. Never create recovery data in
    # place of the app, call its close/flush path, or patch journal contents.
    shutil.copyfile(root / "workspace/session.kws", root / "expected-session.kws")
    journals = sorted((root / "workspace").glob("*.kwj"))
    assert len(journals) == 2, f"expected both durable journals, found {journals}"
    if changed:
        source.write_bytes(CHANGED)
    disk = CHANGED if changed else ORIGINAL
    if negative_control:
        # Deliberately broken isolated copy: no named journal can be recovered.
        # The E2E must reject missing edits rather than pass on metadata alone.
        journals[0].rename(journals[0].with_suffix(".disabled"))
    restored = Editor(binary, root, "restore")
    try:
        restored.wait("restored.txt")
        assert source.read_bytes() == disk, "recovery silently overwrote the disk version"
        evidence = (restored.output / "restored.txt").read_text(encoding="utf-8")
        assert "recovery decision=SaveAs" in evidence
        assert "Use Save As" in evidence
        restored.kill()
    finally:
        restored.close()
    assert source.read_bytes() == disk, "disk bytes must also survive cleanup"
    print(f"PASS {case}: exact recovered edits/session, warning, Save As, disk retained", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--negative-control", action="store_true",
                        help="remove the isolated named journal; this invocation must fail")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    assert not (binary.parent / "kwil.portable").exists(), "portable profile would bypass config isolation"
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    print(f"commit={os.environ.get('GITHUB_SHA', 'local')} binary={binary}", flush=True)
    for changed in ([True] if args.negative_control else [False, True]):
        scenario(binary, output, changed, args.negative_control)


if __name__ == "__main__":
    main()
