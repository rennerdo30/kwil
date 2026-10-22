"""Linux PTY smoke: real terminal rendering, trace, clean exit, 15-second deadline."""
import errno
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import termios
import time
import fcntl


def main():
    output = Path(sys.argv[3])
    output.parent.mkdir(parents=True, exist_ok=True)
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(TERM="xterm-256color", KWIL_NO_UPDATE="1")
        os.execv(sys.argv[1], [sys.argv[1], "--tui", "--trace-startup", sys.argv[2]])
    print(f"TUI PID {pid}")
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    deadline = time.monotonic() + 15
    data = bytearray()
    try:
        while time.monotonic() < deadline:
            if not select.select([fd], [], [], min(0.1, max(0, deadline-time.monotonic())))[0]:
                continue
            try:
                block = os.read(fd, 65536)
            except OSError as e:
                if e.errno == errno.EIO:
                    break
                raise
            if not block:
                break
            data.extend(block)
            if len(data) > 1 << 20:
                raise RuntimeError("TUI produced over 1 MiB for a viewport")
        else:
            raise RuntimeError("TUI startup exceeded 15 seconds")
        _, status = os.waitpid(pid, 0)
        pid = None
        output.write_bytes(data)
        if os.waitstatus_to_exitcode(status) != 0:
            raise RuntimeError(f"TUI failed: {data.decode(errors='replace')}")
        marks = dict(re.findall(rb"t_(launch|first_frame|index_done)=([0-9.]+) ms", data))
        if set(marks) != {b"launch", b"first_frame", b"index_done"}:
            raise RuntimeError(f"TUI trace incomplete: {marks}")
        print(marks)
    finally:
        os.close(fd)
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)


if __name__ == "__main__":
    main()
