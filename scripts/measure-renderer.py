"""Run the existing finite frame harness in an isolated, DWM-cloaked window.

Windows startup marks include process loader time. One sample is informational
against SPEC's owner-PC target, not a performance gate on shared hosted runners.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import uuid


def measure(executable, output, renderer, commit, warp=False):
    output.mkdir(parents=True, exist_ok=False)
    profile = output / "profile" / "kwil"
    profile.mkdir(parents=True)
    (profile / "settings.toml").write_text(
        '[files]\nrestore_session = false\n[editor]\nword_wrap = false\n',
        encoding="utf-8",
    )
    fixture = output / "renderer.txt"
    # Distinct lines without syntax tokens avoid token-color changes while
    # checking the edit worker's damage.
    fixture.write_text(
        "\n".join(f"line {chr(65 + i // 26)}{chr(65 + i % 26)} plain ASCII renderer fixture" for i in range(200)),
        encoding="utf-8",
    )
    (output / "startup.txt").write_text(f"commit {commit}\n", encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if not key.startswith("KWIL_")}
    env.update(
        APPDATA=str(output / "profile"),
        LOCALAPPDATA=str(output / "local"),
        XDG_CONFIG_HOME=str(output / "profile"),
        KWIL_WORKSPACE_ROOT=str(output / "workspace"),
        KWIL_NO_UPDATE="1",
        KWIL_FRAME_SCRIPT="1",
        KWIL_RENDERER_PROBE="1",
        KWIL_TRACE_STARTUP="1",
        KWIL_CAPTURE_DIR=str(output),
    )
    if renderer == "gpu":
        env["KWIL_REQUIRE_GPU"] = "1"
    if warp:
        env["KWIL_FRAME_WARP"] = "1"
        env["KWIL_REQUIRE_GPU"] = "1"
    with (output / "stdout.txt").open("wb") as stdout, (output / "stderr.txt").open("wb") as stderr:
        for stream in (stdout, stderr):
            stream.write(f"commit {commit}\n".encode())
            stream.flush()
        process = subprocess.Popen(
            [str(executable), "--hidden", "--new-window", "--renderer", renderer, str(fixture)],
            env=env,
            stdout=stdout,
            stderr=stderr,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
        print(f"Started owned PID {process.pid}: {renderer}", flush=True)
        try:
            code = process.wait(timeout=20)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            print(f"Closed owned PID {process.pid}", flush=True)
    failure = output / "harness-failure.txt"
    if failure.exists():
        step = output / "harness-step.txt"
        phase = step.read_text(encoding="utf-8") if step.exists() else "harness"
        raise RuntimeError(f"{phase}: {failure.read_text(encoding='utf-8')}")
    if code != 0:
        raise RuntimeError(f"renderer probe exited {code}; see {output}")
    native = (output / "native-window.txt").read_text(encoding="utf-8")
    if "cloaked=true" not in native:
        raise RuntimeError("probe did not verify its DWM cloak")
    counts = {}
    for phase, presents, full, damage, copied in re.findall(
        r"phase=(\w+) presents=(\d+) full=(\d+) damage_px=(\d+) copy_px=(\d+)",
        (output / "renderer-counts.txt").read_text(encoding="utf-8"),
    ):
        counts[phase] = dict(presents=int(presents), full=int(full), damage_px=int(damage), copy_px=int(copied))
    if set(counts) != {"settling", "idle", "scroll", "repaint", "status", "typing"}:
        raise RuntimeError("missing frame phases")
    marks = dict(re.findall(r"kwil-startup (.+?) ([0-9.]+) ms", (output / "startup.txt").read_text(encoding="utf-8")))
    frame = float(marks["first frame presented"])
    content = float(marks["first content presented"])
    backend = output / "scroll.bmp.backend.txt"
    result = dict(
        commit=commit,
        renderer=renderer,
        warp=warp,
        backend=backend.read_text(encoding="utf-8") if backend.exists() else "CPU window-DC capture",
        first_frame_ms=frame,
        first_content_ms=content,
        content_target_ms=40,
        within_content_target=content <= 40,
        counts=counts,
        startup_marks_ms={name: float(value) for name, value in marks.items()},
    )
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--renderer", choices=["cpu", "auto", "gpu"], default="cpu")
    parser.add_argument("--commit", required=True)
    parser.add_argument("--warp", action="store_true", help="exercise native DX12 with its software adapter")
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("this probe verifies DWM-cloaked Windows presentation only")
    if args.warp and args.renderer == "cpu":
        parser.error("--warp requires the auto or gpu presenter")
    output = args.output.resolve() / f"{args.renderer}-{os.getpid()}-{uuid.uuid4().hex}"
    print(f"commit {args.commit}\nEvidence: {output}", flush=True)
    print(json.dumps(measure(args.executable.resolve(strict=True), output, args.renderer, args.commit, args.warp), indent=2))


if __name__ == "__main__":
    main()
