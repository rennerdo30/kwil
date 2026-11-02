#!/usr/bin/env python3
"""Real kwil TUI compare acceptance in a private Unix PTY (no desktop/input).

Run after building kwil: python3 scripts/test-tui-compare.py target/debug/kwil.
Only the Python standard library is needed. Captures contain the reconstructed
screen as well as the original terminal bytes, never model/test-hook output.
"""

import argparse
import codecs
import errno
import itertools
import json
import os
from pathlib import Path
import re
import select
import signal
import struct
import subprocess
import tempfile
import time


class Screen:
    """Consume the cursor/erase subset emitted by ratatui's crossterm backend."""

    width, height = 140, 32

    def __init__(self):
        self.rows = [[" "] * self.width for _ in range(self.height)]
        self.x = self.y = 0
        self.pending = ""
        self.saved = (0, 0)
        self.completed = None

    def feed(self, text):
        self.pending += text
        while self.pending:
            char = self.pending[0]
            if char == "\x1b":
                if len(self.pending) < 2:
                    break
                if self.pending[1] == "[":
                    match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                    if not match:
                        break
                    self.pending = self.pending[match.end():]
                    self.csi(match[1], match[3])
                    continue
                if self.pending[1] == "]":
                    match = re.search(r"\x07|\x1b\\", self.pending)
                    if not match:
                        break
                    self.pending = self.pending[match.end():]
                    continue
                if self.pending[1] == "7":
                    self.saved = (self.x, self.y)
                elif self.pending[1] == "8":
                    self.x, self.y = self.saved
                self.pending = self.pending[2:]
                continue
            self.pending = self.pending[1:]
            if char == "\r":
                self.x = 0
            elif char == "\n":
                self.y = min(self.height - 1, self.y + 1)
            elif char == "\b":
                self.x = max(0, self.x - 1)
            elif char >= " ":
                # Fixtures and chrome use single-column characters only.
                self.rows[self.y][min(self.x, self.width - 1)] = char
                self.x = min(self.width - 1, self.x + 1)

    def csi(self, params, command):
        if params == "?25" and command in "hl":
            # Ratatui updates cursor visibility after writing a frame's cell
            # diff. Keep assertions/captures off partial PTY read fragments.
            self.completed = [row.copy() for row in self.rows]
        if params.startswith(("?", ">", "=")) or command in "mnhlqtu":
            return  # styles, terminal modes and device queries do not paint
        values = [int(part or 0) for part in params.split(";")]
        n = values[0] or 1
        if command in "Hf":
            self.y = min(self.height - 1, max(0, n - 1))
            self.x = min(self.width - 1, max(0, (values[1] if len(values) > 1 else 1) - 1))
        elif command == "G":
            self.x = min(self.width - 1, n - 1)
        elif command == "A":
            self.y = max(0, self.y - n)
        elif command == "B":
            self.y = min(self.height - 1, self.y + n)
        elif command == "C":
            self.x = min(self.width - 1, self.x + n)
        elif command == "D":
            self.x = max(0, self.x - n)
        elif command == "J":
            if values[0] in (2, 3):
                self.rows = [[" "] * self.width for _ in range(self.height)]
            elif values[0] == 0:
                self.rows[self.y][self.x:] = [" "] * (self.width - self.x)
                for row in range(self.y + 1, self.height):
                    self.rows[row] = [" "] * self.width
        elif command == "K":
            start, end = {0: (self.x, self.width), 1: (0, self.x + 1), 2: (0, self.width)}[values[0]]
            self.rows[self.y][start:end] = [" "] * (end - start)
        elif command not in "csu":
            raise AssertionError(f"unsupported painting sequence: CSI {params}{command}")

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.visible_rows())

    def visible_rows(self):
        return self.completed if self.completed is not None else self.rows

    def first_compare_line(self):
        for row in self.visible_rows():
            match = re.match(r"^\s{0,4}(\d+) ", "".join(row))
            if match:
                return int(match[1])
        return None


NEXT = itertools.count()


class Tui:
    def __init__(self, exe, root, path, output, name):
        # Import only here so parser checks also run on Windows.
        import fcntl
        import pty
        import termios

        self.output, self.name = output, name
        self.screen, self.raw = Screen(), bytearray()
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.status = None
        self.deadline = time.monotonic() + 60
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", Screen.height, Screen.width, 0, 0))
        env = dict(os.environ, TERM="xterm-256color", KWIL_NO_UPDATE="1",
                   KWIL_WORKSPACE_ROOT=str(root), HOME=str(root / "home"),
                   XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
                   XDG_STATE_HOME=str(root / "state"), TMPDIR=str(root / "tmp"))
        for folder in ("home", "config", "data", "state", "tmp"):
            (root / folder).mkdir()
        self.pid = os.fork()
        if self.pid == 0:
            try:
                os.close(master)
                os.setsid()
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
                for fd in (0, 1, 2):
                    os.dup2(slave, fd)
                if slave > 2:
                    os.close(slave)
                os.chdir(root)
                os.execve(exe, [str(exe), "--tui", "--hidden", str(path)], env)
            finally:
                os._exit(127)
        os.close(slave)
        self.fd = master
        os.set_blocking(master, False)
        print(f"{name}: started owned PID {self.pid}", flush=True)

    def send(self, data):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)

    def pump(self, timeout):
        if select.select([self.fd], [], [], timeout)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                data = b""
            self.raw.extend(data)
            self.screen.feed(self.decoder.decode(data))
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)

    def wait(self, predicate, label, timeout=10):
        deadline = min(self.deadline, time.monotonic() + timeout)
        while True:
            if predicate(self.screen):
                self.capture(label)
                return
            assert self.status is None, f"{label}: PID {self.pid} exited {self.status}\n{self.screen.text()}"
            remaining = deadline - time.monotonic()
            assert remaining > 0, f"{label}: timeout\n{self.screen.text()}"
            self.pump(min(0.02, remaining))

    def capture(self, label):
        (self.output / f"{self.name}-{label}.txt").write_text(self.screen.text(), encoding="utf-8")

    def command(self, name):
        # Ctrl+P is reachable even while compare owns editor input.
        self.send(b"\x10")
        self.wait(lambda s: "Enter: open" in s.text(), "palette-open")
        self.send(">" + name)
        self.wait(lambda s: ">" + name in s.text() and "Enter: open" in s.text(), "palette-query")
        self.send(b"\r")

    def compare_files(self, path):
        self.command("Compare Files")
        self.wait(lambda s: "Open by path" in s.text(), "file-prompt")
        self.send(str(path))
        self.wait(lambda s: str(path) in s.text(), "file-path")
        start = time.monotonic()
        self.send(b"\r")
        return start

    def finish(self):
        try:
            self.command("Close Comparison")
            self.command("Quit")
            self.wait(lambda _: self.status is not None, "exit", timeout=40)
            assert self.status == 0, f"normal TUI shutdown failed: {self.status}"
        finally:
            self.close()

    def close(self):
        try:
            self.capture("last")
            (self.output / f"{self.name}.ansi").write_bytes(self.raw)
            if self.status is None:
                os.kill(self.pid, signal.SIGKILL)
                _, status = os.waitpid(self.pid, 0)
                self.status = os.waitstatus_to_exitcode(status)
                print(f"{self.name}: stopped owned PID {self.pid} after test failure", flush=True)
            else:
                print(f"{self.name}: owned PID {self.pid} exited {self.status}", flush=True)
        finally:
            os.close(self.fd)
            self.fd = -1


def compare_ready(screen, count, left, right):
    text = screen.text()
    return (re.search(rf"\b{count} changes?\b", text) is not None
            and any(left in "".join(row[:screen.width // 2])
                    and right in "".join(row[screen.width // 2:]) for row in screen.visible_rows()))


def assert_compare_row(screen, line, left, right):
    expected = (f"{line:>5} {left}", f"{line:>5} {right}")
    for row in screen.visible_rows():
        halves = tuple("".join(part).rstrip() for part in (row[:screen.width // 2], row[screen.width // 2:]))
        if halves == expected:
            return
    raise AssertionError(f"compare row {line} must have exact text in both panes: {expected}\n{screen.text()}")


def run_case(exe, output, name, large=False):
    # Keep paths short enough to see the complete path in the 72-column prompt
    # on macOS too (its default temporary directory has a long /var prefix).
    with tempfile.TemporaryDirectory(prefix=f"kwil-tui-compare-{os.getpid()}-{next(NEXT)}-", dir="/tmp") as directory:
        root = Path(directory)
        left, right = root / "left.txt", root / "right.txt"
        if name == "singular-saved":
            lines = ["original first", "same one", "same two", "same three", "same four"]
        else:
            lines = [f"row {n:06}: unchanged payload for terminal compare" for n in range(100_000 if large else 40)]
        original = "\n".join(lines)
        other = lines.copy()
        other[0] = "different first"
        second = 90_000 if large else 30
        if name != "singular-saved":
            other[second] = "different second"
        left.write_text(original, encoding="utf-8")
        right.write_text("\n".join(other), encoding="utf-8")
        before = (left.read_bytes(), right.read_bytes())
        if large:
            assert 2 * 1024**2 < left.stat().st_size < 8 * 1024**2
        tui = Tui(exe, root, left, output, name)
        finished = False
        try:
            # The raw first-content preview precedes recovery/workspace
            # admission. Wait for real workspace chrome before sending input;
            # timing starts later, when the compare path is submitted.
            tui.wait(lambda s: lines[0] in s.text() and "[1/1]" in s.text(), "editor-ready")
            started = tui.compare_files(right)
            count = 1 if name == "singular-saved" else 2
            tui.wait(lambda s: compare_ready(s, count, lines[0], other[0]), "first-view", timeout=1 if large else 10)
            elapsed = time.monotonic() - started
            if large:
                assert elapsed < 1, f"large compare first view {elapsed:.3f}s exceeds 1 s"
                print(json.dumps({"case": name, "bytes_per_file": left.stat().st_size,
                                  "first_view_ms": round(elapsed * 1000, 3)}), flush=True)
            if name == "singular-saved":
                assert "1 changes" not in tui.screen.text(), tui.screen.text()
                tui.wait(lambda s: "1 unchanged line (" in s.text(), "one-unchanged-line")
                assert_compare_row(tui.screen, 2, "same one", "same one")
                tui.command("Close Comparison")
                tui.wait(lambda s: "Buffer |" not in s.text() and "original first" in s.text(), "editor-return")
                tui.send("buffer ")
                tui.wait(lambda s: "buffer original first" in s.text(), "dirty-buffer")
                tui.command("Compare with Saved File")
                tui.wait(lambda s: compare_ready(s, 1, "buffer original first", "original first"), "saved-view")
                assert "1 changes" not in tui.screen.text(), tui.screen.text()
                tui.wait(lambda s: "1 unchanged line (" in s.text(), "saved-unchanged-line")
                assert_compare_row(tui.screen, 2, "same one", "same one")
            else:
                assert tui.screen.first_compare_line() == 1, tui.screen.text()
                # Logical compare cursor/anchor is the first numbered row of
                # the side-by-side viewport, not the underlying editor cursor.
                tui.send(b"\x1b[18~")  # F7
                tui.wait(lambda s: s.first_compare_line() == second + 1 and "different second" in s.text(),
                         "next-change", timeout=1 if large else 10)
                tui.send(b"\x1b[18;2~")  # Shift+F7
                tui.wait(lambda s: s.first_compare_line() == 1 and "different first" in s.text(),
                         "previous-change", timeout=1 if large else 10)
            assert before == (left.read_bytes(), right.read_bytes()), "read-only compare changed its files"
            tui.finish()
            finished = True
        finally:
            if not finished:
                # finish() owns cleanup even when graceful exit fails.
                if tui.fd >= 0:
                    tui.close()
        print(f"PASS {name}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("exe", type=Path)
    parser.add_argument("--output", type=Path, default=Path("target/tui-compare"))
    parser.add_argument("--case", choices=["singular-saved", "navigation", "large"])
    args = parser.parse_args()
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"],
                                     cwd=Path(__file__).resolve().parent.parent, text=True, timeout=10).strip()
    print(f"Commit under test: {commit}", flush=True)
    args.output.mkdir(parents=True, exist_ok=True)
    for case in ([args.case] if args.case else ["singular-saved", "navigation", "large"]):
        run_case(args.exe.resolve(), args.output.resolve(), case, large=case == "large")


if __name__ == "__main__":
    main()
