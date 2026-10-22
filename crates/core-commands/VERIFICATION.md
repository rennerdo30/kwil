# KW-25 / KW-3 verification receipt

Starting base: `b1b5c55658953a8572ea10662adf4f2aa4e7f730`.
Branch: `kw/25-commands-keymaps`. Run date: 2026-10-23 JST.
Checks below exercised the implementation working tree, not the empty base stub.

Environment for every cargo invocation:
`CARGO_TARGET_DIR=E:/Development/_cache/cargo-target/kwil-kw-25-commands-keymaps`;
compile/test jobs limited to 4. Every launched cargo PID was recorded with a
120-second (mutants) or 180-second (ordinary checks) timeout. All exited normally;
no process termination was needed. No persistent environment or Git config edits.

- Focused `cargo test -p kwil-core-commands -j 4 --locked`: **16 passed**.
- Initial focused run: **15 passed, 1 failed**. Uppercase shortcut notation
  `Ctrl+K Ctrl+C` incorrectly inferred Shift. Fixed parser case handling;
  bare Vim `G` retains Shift semantics.
- `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`: **pass**.
- `cargo fmt --all --check`: **pass**.

## Deliberate broken-copy evidence

`scripts/fail-first.ps1` created a standalone disposable crate copy, mutated only
production source/data there, and ran each named contract against a compiling
defect. Exit 101 and the named assertion failures were required; compilation
errors and accidentally passing tests would fail the verification script. Every
source was restored in `finally`. Original worktree files were never mutated.
The complete script run succeeded; every one of the 16 tests rejected a defect.

Raw local receipt:
`E:/Development/_cache/cargo-target/kwil-kw-25-commands-keymaps/fail-first-7f0d9366f60440d4830afc00e17e95b9/summary.txt`.
The per-mutant stdout/stderr are beside that receipt. This cache path is local
evidence, not a shipped artifact; the script makes the experiment reproducible.

| Deliberate defect | Failing contract(s) |
|---|---|
| Remove file.save from registry data | registry metadata/coverage; all preset loading |
| Ignore explicit-modifier letter normalization | chord parsing and aliases |
| Retain Shift on logical punctuation | frontend adapter normalization |
| Drop all override entries | layering; contexts/modes/sequences; settings overrides |
| Ignore user layer precedence over preset specificity | layering/rebinding |
| Parse h as Right | Vim motion/count table |
| Add rather than multiply operator counts | Vim operator/count/text-object table |
| Keep Normal when entering Insert | Vim mode/visual/cancel table |
| Parse dot as Undo | repeat/search/ex/substitution table |
| Select register z regardless of prefix | register/overflow state |
| Ignore explicit Vim unbind | shared dispatcher/rebinding/context cancellation |
| Discard unknown-key diagnostics | settings defaults/unknown keys |
| Suppress reload callback | last-valid reload and atomic-save fixture |
| Use wrong portable settings filename | portable and per-platform paths |

These are focused offline checks. The first dependency-compiling focused test
run took 3.51 s after a prior metadata/check compile; a later incremental run
took 0.59 s. These are observations, not a clean-build benchmark. New direct
dependencies (primary crates.io metadata checked 2026-10-23): serde 1.0.229,
released 2026-07-18; toml 1.1.8+spec-1.1.0, released 2026-10-09; both
MIT OR Apache-2.0. toml uses only parse/serde/std features. Serde derive is confined
to this settings crate; no notify/frontend/runtime dependency was added.

## Not run / integration boundary

- No local full-workspace test suite, GUI/TUI/native execution, benchmarks,
  platform keyboard captures, or release/publication.
- FileWatcher is supplied by the parallel core-workspace package. This crate
  delivers the requested callback hook; it does not start an active watcher.
- Frontends do not use the crate yet. Editor execution, document range semantics,
  register contents, completed-change replay and on-screen setting application
  remain integration work. Unit tests prove parser output and reload callbacks.
- Linux/macOS checks and the full suite belong to GitHub-hosted CI. The parent
  receives the PR URL, exact head and current CI state for review and monitoring.
  This implementation child does not merge or watch/poll the PR.
