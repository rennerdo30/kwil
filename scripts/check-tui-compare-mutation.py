#!/usr/bin/env python3
"""Hosted fail-first proof: the real PTY test must reject broken Shift+F7."""

import os
from pathlib import Path
import subprocess
import sys


def main():
    assert os.environ.get("GITHUB_ACTIONS") == "true", "build mutation only on a hosted CI runner"
    root = Path(__file__).resolve().parent.parent
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True, timeout=10).strip()
    print(f"Commit under test: {commit} (deliberately broken navigation copy)", flush=True)
    source = root / "crates/ui-tui/src/compare_ui.rs"
    original = source.read_bytes()
    old = b"Action::Previous } else { Action::Next }"
    assert original.count(old) == 1, "navigation mutation anchor must be unique"
    try:
        source.write_bytes(original.replace(old, b"Action::Next } else { Action::Next }"))
        subprocess.run(["cargo", "build", "-j", "4", "--locked", "-p", "kwil", "--bin", "kwil"],
                       cwd=root, check=True, timeout=900)
        result = subprocess.run([sys.executable, str(root / "scripts/test-tui-compare.py"),
                                 str(root / "target/debug/kwil"), "--case", "navigation",
                                 "--output", "target/tui-compare/mutation"],
                                cwd=root, capture_output=True, text=True, timeout=90)
        print(result.stdout, end="", flush=True)
        print(result.stderr, end="", flush=True)
        assert result.returncode != 0 and "previous-change: timeout" in result.stderr, (
            "broken Shift+F7 must fail on the visible previous-change position, not setup or shutdown"
        )
        print("PASS fail-first: real PTY assertion rejected Shift+F7 routed to Next", flush=True)
    finally:
        source.write_bytes(original)


if __name__ == "__main__":
    main()
