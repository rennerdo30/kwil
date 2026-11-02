#!/usr/bin/env python3
"""Hosted fail-first proof for real PTY navigation and stale compare text."""

import os
from pathlib import Path
import subprocess
import sys


def main():
    assert os.environ.get("GITHUB_ACTIONS") == "true", "build mutation only on a hosted CI runner"
    root = Path(__file__).resolve().parent.parent
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True, timeout=10).strip()
    print(f"Commit under test: {commit} (deliberately broken navigation copy)", flush=True)
    pending_source = root / "crates/ui-tui/src/binary_offer.rs"
    pending_original = pending_source.read_bytes()
    preview_gate = b"if !view.binary_preview_ready(view.active_id())"
    assert pending_original.count(preview_gate) == 1, "unique pending-palette mutation required"
    try:
        pending_source.write_bytes(pending_original.replace(preview_gate, b"if view.binary_preview_ready(view.active_id())"))
        result = subprocess.run(["cargo", "test", "-j", "4", "--locked", "-p", "kwil-ui-tui",
                                 "pending_preview_palette_opens_without_accepting_held_document_completion"],
                                cwd=root, capture_output=True, text=True, timeout=900)
        print(result.stdout, end="", flush=True)
        print(result.stderr, end="", flush=True)
        output = root / "target/tui-compare"
        output.mkdir(parents=True, exist_ok=True)
        (output / "mutation-palette-test.txt").write_text(
            f"Commit under test: {commit}\n" + result.stdout + result.stderr, encoding="utf-8")
        assert result.returncode != 0 and "pending preview admits palette chrome" in result.stdout + result.stderr, (
            "held-completion test must reject blocked palette dispatch, not fail on setup or compilation"
        )
        print("PASS fail-first: held-completion regression rejected pending-palette mutation", flush=True)
    finally:
        pending_source.write_bytes(pending_original)
    source = root / "crates/ui-tui/src/compare_ui.rs"
    original = source.read_bytes()
    navigation = b"Action::Previous } else { Action::Next }"
    clear = b"    frame.render_widget(ratatui::widgets::Clear, area);"
    assert original.count(navigation) == 1 and original.count(clear) == 1, "unique mutation anchors required"
    mutations = [
        ("previous-change", original.replace(navigation, b"Action::Next } else { Action::Next }"),
         "navigation", "previous-change: timeout"),
        ("stale-text", original.replace(clear, b""),
         "singular-saved", "compare row 2 must have exact text in both panes"),
    ]
    try:
        for name, mutated, case, failure in mutations:
            source.write_bytes(mutated)
            subprocess.run(["cargo", "build", "-j", "4", "--locked", "-p", "kwil", "--bin", "kwil"],
                           cwd=root, check=True, timeout=900)
            result = subprocess.run([sys.executable, str(root / "scripts/test-tui-compare.py"),
                                     str(root / "target/debug/kwil"), "--case", case,
                                     "--output", f"target/tui-compare/mutation-{name}"],
                                    cwd=root, capture_output=True, text=True, timeout=90)
            print(result.stdout, end="", flush=True)
            print(result.stderr, end="", flush=True)
            assert result.returncode != 0 and failure in result.stderr, (
                f"{name} mutation must fail on its visible comparison assertion, not setup or shutdown"
            )
            print(f"PASS fail-first: real PTY assertion rejected {name} mutation", flush=True)
    finally:
        source.write_bytes(original)


if __name__ == "__main__":
    main()
