"""Hosted Linux only: measure received RFB updates, not inferred app damage.

Standard-library RFB 3.8 client, Raw + CopyRect only (RFC 6143 sections 7.5-7.7).
No authentication is permitted only on this disposable runner's loopback socket.
"""

import argparse
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import select
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import zlib


def run(command, env, **kwargs):
    capture = kwargs.pop("capture_output", False)
    if capture:
        kwargs.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    process = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL, **kwargs)
    print(f"Started owned PID {process.pid}: {command[0]}, timeout 10s", flush=True)
    try:
        stdout, stderr = process.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate(timeout=5)
        raise
    finally:
        print(f"Closed owned PID {process.pid}: {command[0]}", flush=True)
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, command, stdout, stderr)
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


@contextmanager
def owned(command, env, output, name, commit, cwd=None):
    with (output / f"{name}.log").open("wb") as log:
        log.write(f"commit {commit}\n".encode())
        log.flush()
        process = subprocess.Popen(command, env=env, cwd=cwd, stdout=log, stderr=log, stdin=subprocess.DEVNULL)
        print(f"Started owned PID {process.pid}: {name}, timeout 120s", flush=True)
        timer = threading.Timer(120, process.kill)
        timer.start()
        try:
            yield process
        finally:
            timer.cancel()
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            print(f"Closed owned PID {process.pid}: {name}, exit {process.returncode}", flush=True)


class RFB:
    def __init__(self, connection):
        self.socket = connection
        self.socket.settimeout(5)
        version = self.read(12)
        if version != b"RFB 003.008\n":
            raise RuntimeError(f"expected RFB 3.8, received {version!r}")
        self.socket.sendall(version)
        security = self.read(self.read(1)[0])
        if 1 not in security:
            raise RuntimeError("isolated Xvnc did not offer security type None")
        self.socket.sendall(b"\x01")
        if self.read(4) != b"\0\0\0\0":
            raise RuntimeError("RFB security handshake failed")
        self.socket.sendall(b"\x01")  # shared ClientInit
        self.width, self.height = struct.unpack("!HH", self.read(4))
        self.read(16)  # replace the server's default pixel format below
        self.desktop = self.read(struct.unpack("!I", self.read(4))[0]).decode("utf-8", "replace")
        # 32-bit little-endian true-colour B,G,R,pad pixels.
        pixel_format = struct.pack("!BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
        self.socket.sendall(b"\0\0\0\0" + pixel_format)
        self.socket.sendall(struct.pack("!BBHii", 2, 0, 2, 1, 0))
        self.frame = bytearray(self.width * self.height * 4)
        self.rectangles = []
        self.total = dict(update_bytes=0, updates=0, rectangles=0, raw_pixels=0,
                          copy_rectangles=0, copy_pixels=0, other_server_bytes=0)
        self.request(False)

    def read(self, count):
        data = bytearray()
        while len(data) < count:
            part = self.socket.recv(count - len(data))
            if not part:
                raise RuntimeError("RFB server disconnected")
            data.extend(part)
        return bytes(data)

    def request(self, incremental=True):
        self.socket.sendall(struct.pack("!BBHHHH", 3, int(incremental), 0, 0, self.width, self.height))

    def key(self, key, down):
        self.socket.sendall(struct.pack("!BB2xI", 4, int(down), key))

    def press(self, key, control=False):
        if control:
            self.key(0xFFE3, True)
        self.key(key, True)
        self.key(key, False)
        if control:
            self.key(0xFFE3, False)

    def wheel(self):
        # One downward wheel notch, inside the editor body (not its chrome).
        for buttons in (0, 16, 0):
            self.socket.sendall(struct.pack("!BBHH", 5, buttons, 500, 400))

    def message(self, phase):
        kind = self.read(1)[0]
        if kind == 2:  # Bell
            self.total["other_server_bytes"] += 1
            return
        if kind == 3:  # ServerCutText
            length = struct.unpack("!I", self.read(7)[3:])[0]
            self.read(length)
            self.total["other_server_bytes"] += 8 + length
            return
        if kind != 0:
            raise RuntimeError(f"unexpected server message {kind}")
        count = struct.unpack("!H", self.read(3)[1:])[0]
        update_bytes = 4
        for _ in range(count):
            x, y, width, height, encoding = struct.unpack("!HHHHi", self.read(12))
            if x + width > self.width or y + height > self.height:
                raise RuntimeError("rectangle outside framebuffer")
            if encoding == 0:
                pixels = self.read(width * height * 4)
                self.total["raw_pixels"] += width * height
            elif encoding == 1:
                sx, sy = struct.unpack("!HH", self.read(4))
                if sx + width > self.width or sy + height > self.height:
                    raise RuntimeError("CopyRect source outside framebuffer")
                # Snapshot before writing: CopyRect source/destination may overlap.
                pixels = b"".join(self.frame[((sy + row) * self.width + sx) * 4:
                                             ((sy + row) * self.width + sx + width) * 4]
                                  for row in range(height))
                self.total["copy_rectangles"] += 1
                self.total["copy_pixels"] += width * height
            else:
                raise RuntimeError(f"unrequested encoding {encoding}")
            payload = len(pixels) if encoding == 0 else 4
            update_bytes += 12 + payload
            for row in range(height):
                start = ((y + row) * self.width + x) * 4
                self.frame[start:start + width * 4] = pixels[row * width * 4:(row + 1) * width * 4]
            self.rectangles.append(dict(phase=phase, x=x, y=y, width=width, height=height,
                                        encoding="Raw" if encoding == 0 else "CopyRect", payload_bytes=payload))
        self.total["updates"] += 1
        self.total["rectangles"] += count
        self.total["update_bytes"] += update_bytes
        self.request()  # exactly one outstanding incremental request, including at idle

    def collect(self, phase, seconds):
        before = self.total.copy()
        started = time.monotonic()
        deadline = started + seconds
        while (remaining := deadline - time.monotonic()) > 0:
            if select.select([self.socket], [], [], remaining)[0]:
                self.message(phase)
        return dict(action=phase, seconds=round(time.monotonic() - started, 3),
                    **{key: value - before[key] for key, value in self.total.items()})

    def capture(self, path):
        # Standalone lossless PNG from the framebuffer actually received by the client.
        def chunk(kind, data):
            return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))
        rows = bytearray()
        for y in range(self.height):
            rows.append(0)
            row = self.frame[y * self.width * 4:(y + 1) * self.width * 4]
            for x in range(0, len(row), 4):
                rows.extend((row[x + 2], row[x + 1], row[x]))
        header = struct.pack("!IIBBBBB", self.width, self.height, 8, 2, 0, 0, 0)
        path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
                         + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def wait_for(predicate, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {description}")


def file_ready(log, mark):
    # The fixture-specific *presented* mark is a startup-benchmark paint fence,
    # not an acknowledgement of a normal interactive document. Use the known
    # row's worker acknowledgement, received pixels, and exact-save checks here.
    text = log.read_text()
    return f"kwil-startup {mark} " in text and "kwil-startup first content presented " in text


def measure(binary, output, commit):
    output.mkdir(parents=True, exist_ok=False)
    profile = output / "profile"
    settings = profile / "config/kwil"
    settings.mkdir(parents=True)
    (settings / "settings.toml").write_text(
        '[files]\nrestore_session = false\n[editor]\nword_wrap = false\n'
        '[render]\nremote = "auto"\n', encoding="utf-8")
    (settings / "welcome-v1.seen").write_bytes(b"kwil-welcome-v1\n")
    fixture = profile / "typing.txt"
    original = b"kwil-speed-file\n" + b"".join(f"row {i:04d} plain text for scroll measurement\n".encode() for i in range(300))
    fixture.write_bytes(original)
    opened = profile / "opened.txt"
    opened.write_bytes(b"kwil-speed-forward\nOpened through the running instance.\n")
    # Standalone Xvnc does not create an app environment. Supply the same
    # session hint a VNC desktop launcher supplies; do not force render.remote.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("KWIL_") and key not in ("WAYLAND_DISPLAY", "DISPLAY")}
    env.update(DISPLAY=":99", VNCDESKTOP="kwil-hosted-vnc", HOME=str(profile),
               XDG_CONFIG_HOME=str(profile / "config"), XDG_STATE_HOME=str(profile / "state"),
               XDG_CACHE_HOME=str(profile / "cache"), XDG_RUNTIME_DIR=str(profile),
               KWIL_WORKSPACE_ROOT=str(profile), KWIL_NO_UPDATE="1", KWIL_TRACE_STARTUP="1",
               WINIT_UNIX_BACKEND="x11")
    profile.chmod(0o700)
    server = shutil.which("Xvnc") or shutil.which("Xtigervnc")
    if not server:
        raise RuntimeError("TigerVNC Xvnc/Xtigervnc executable missing")
    with ExitStack() as stack:
        stack.enter_context(owned([server, ":99", "-geometry", "1400x1000", "-depth", "24",
                                   "-rfbport", "5999", "-localhost=1", "-SecurityTypes", "None",
                                   "-nolisten", "tcp", "-ac", "-desktop", "kwil-hosted-vnc"],
                                  env, output, "xvnc", commit))
        wait_for(lambda: Path("/tmp/.X11-unix/X99").exists(), "Xvnc display socket")
        run(["xdotool", "getdisplaygeometry"], env, stdout=subprocess.DEVNULL)
        connection = stack.enter_context(socket.create_connection(("127.0.0.1", 5999), timeout=5))
        client = RFB(connection)
        stack.callback(client.capture, output / "last-frame.png")
        stack.callback(lambda: (output / "rectangles.json").write_text(json.dumps(client.rectangles, indent=2) + "\n"))
        client.collect("desktop_setup", 1)
        desktop_frame = bytes(client.frame)
        app = stack.enter_context(owned([str(binary), "--gui", str(fixture)], env, output, "kwil", commit,
                                       cwd=profile))
        log = output / "kwil.log"
        # Drain the real connection and preserve its pixels BEFORE readiness.
        # The workflow uploads these even if any later acknowledgement fails.
        client.collect("app_setup", 3)
        client.capture(output / "setup-window.png")
        wait_for(lambda: "first frame presented" in log.read_text(), "initial window frame")
        windows = run(["xdotool", "search", "--onlyvisible", "--pid", str(app.pid), "--name", "Kwil"],
                      env, capture_output=True, text=True).stdout.splitlines()
        if not windows:
            raise RuntimeError("no visible Kwil window on isolated display")
        # Direct focus to this owned window; no window manager or unrelated apps.
        run(["xdotool", "windowfocus", windows[0]], env)
        wait_for(lambda: "session_detected=true remote=true blink=false" in log.read_text(),
                 "automatic remote policy diagnostic")
        client.socket.sendall(struct.pack("!BBHH", 5, 0, 500, 400))
        # Debug builds intentionally show onboarding even with a seen marker.
        # Escape advances About, then explicitly skips file registration. Both
        # inputs travel over RFB and their traffic belongs only to setup.
        for _ in range(2):
            client.press(0xFF1B)
            client.collect("dismiss_welcome", 0.5)
        client.capture(output / "setup.png")
        if client.frame == desktop_frame:
            raise RuntimeError("setup framebuffer has not changed from the empty desktop; see setup.png")
        wait_for(lambda: file_ready(log, "file content ready"), "known initial file ready with received pixels")
        client.capture(output / "initial.png")
        records = []

        def sample(action, seconds):
            record = client.collect(action, seconds)
            records.append(record)
            (output / "actions.json").write_text(json.dumps(dict(commit=commit, actions=records), indent=2) + "\n")
            print(json.dumps(record), flush=True)
            return record

        sample("idle", 3)
        client.press(ord("x"))
        sample("type_one_character", 3)
        client.capture(output / "typing.png")
        # Save is outside the typing sample; exact bytes prove input reached the document.
        client.press(ord("s"), control=True)
        client.collect("save_verification", 2)
        wait_for(lambda: fixture.read_bytes() == b"x" + original, "exact typed/saved bytes")
        client.wheel()
        sample("scroll_one_wheel_notch", 3)
        client.capture(output / "scroll.png")
        sample("idle_after_scroll", 2)
        before_open = bytes(client.frame)
        run([str(binary), "--gui", str(opened)], env, cwd=profile, stdout=subprocess.DEVNULL)
        sample("open_file_via_ipc", 4)
        client.capture(output / "opened.png")
        if client.frame == before_open:
            raise RuntimeError("IPC open did not change the received framebuffer; see opened.png")
        wait_for(lambda: file_ready(log, "forward content ready"), "known IPC file ready with received pixels")
        sample("idle_after_open", 2)
        # Outside all action samples, prove IPC selected the requested document.
        client.press(ord("y"))
        client.press(ord("s"), control=True)
        client.collect("open_save_verification", 2)
        wait_for(lambda: opened.read_bytes() == b"ykwil-speed-forward\nOpened through the running instance.\n",
                 "exact IPC document edit/save bytes")
        result = dict(commit=commit, desktop=client.desktop, geometry=[client.width, client.height],
                      encoding="Raw + CopyRect (uncompressed, 32bpp)",
                      remote_detected=True, remote_override="auto", session_hint={"VNCDESKTOP": env["VNCDESKTOP"]},
                      exact_typing_save_verified=True, forwarded_file_save_verified=True, actions=records,
                      measurement="Received RFB FramebufferUpdate bytes including headers; excludes TCP/IP, handshake and client requests. Pixel areas include overlap across rectangles/updates, not unique damage or app presents.")
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        summary = [f"## TigerVNC informational measurement\n\nCommit `{commit}`; desktop 1400x1000; Raw + CopyRect, 32bpp.",
                   "\nAuto remote detection: **true** (`VNCDESKTOP` supplied by the hosted VNC session launcher; `render.remote=auto`, blink disabled).",
                   "\n| Action | Seconds | Update bytes | Updates | Rectangles | Raw pixels | Copy rects | Copy pixels |",
                   "|---|---:|---:|---:|---:|---:|---:|---:|"]
        for row in records:
            summary.append("| {action} | {seconds} | {update_bytes} | {updates} | {rectangles} | {raw_pixels} | {copy_rectangles} | {copy_pixels} |".format(**row))
        summary.extend(["\nTyping and IPC file selection verified by exact edited/saved bytes. Readiness requires known-file ready diagnostics and changed received framebuffer pixels; setup/open PNGs retain the actual pixels.",
                        "\nBytes are server-to-client RFB updates including headers, excluding TCP/IP, handshake, input and request messages. Raw is intentionally uncompressed. Pixel sums can overlap; updates are not app presents.",
                        "\nNo performance gates. Xvnc may compare/coalesce damage; zero wire updates do not prove zero app presents. One wheel notch uses the editor's configured line increment.",
                        "\nStill needed: a separate real RDP host/client with per-action transport counters, live local/RDP switching, compressed VNC codecs, physical machines and WAN latency/bandwidth. Windows stretch skipped: no validated headless FreeRDP byte/rectangle instrument or disposable RDP logon setup in this package; feasibility was not proven. No owner-desktop connection."])
        (output / "summary.md").write_text("\n".join(summary) + "\n")
        print(json.dumps(result, indent=2), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    # Never create an X server or send input on the owner's machine.
    if sys.platform != "linux" or os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("this measurement runs only on a disposable Linux GitHub Actions runner")
    print(f"commit {args.commit}", flush=True)
    measure(args.executable.resolve(strict=True), args.output.resolve(), args.commit)


if __name__ == "__main__":
    main()
