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

## PR #6 fix round 1 (2026-10-23 JST)

Started at requested head `0c6c622c77332f77093fc6782acddff574a7868c`.
The feature branch remained checked out in its original implementation worktree,
so this assigned worktree used detached HEAD and pushes directly to the requested
remote ref. No other checkout or branch was reset. No main merge or lockfile
change was needed. Scope remains `crates/core-commands/**`.

Implementation under test: `0104c6ae8e14ab9bb4dcf1efd7aad5a794787c47`.

- `cargo test -p kwil-core-commands -j 4 --locked`: **21 passed**, 0 failed.
- `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`: **pass**.
- `cargo fmt --all --check`: **pass**.
- Shared Cargo target cache reused; only changed/local crates compiled or checked.
  No dependency was added or updated. These were offline code checks, not native
  keyboard/frontend acceptance; `--offline` was used for the mutant runs.
- Final check receipts begin with the exact tested commit:
  `E:/Development/_cache/cargo-target/kwil-kw-25-commands-keymaps/fix-round-1-{test,clippy,fmt}.receipt`.
  Cargo PIDs: test 10380, clippy 35372, fmt 17436; each had a 180-second timeout
  and exited normally. No process termination was needed.

### Fail-first receipt

`scripts/fail-first.ps1 -FixRoundOne` ran the five new tests on compiling,
deliberately broken standalone copies before the final passing check. Each
required exit 101 and its named assertion failure; compile failures do not count.

| Mutation | Tests that failed |
|---|---|
| Notepad++ Save All changed back to Save As | reviewed_presets_match_windows_shortcuts |
| VS Code delete-line shortcut removed | reviewed_presets_match_windows_shortcuts |
| Drop Shift from punctuation (original defect) | punctuation_preserves_logical_key_and_explicit_shift; adapters_normalize_logical_keys_without_conflating_ctrl_and_cmd |
| Replace source line with 1 | settings_parse_errors_keep_locations; settings_validation_errors_keep_locations; settings_unknown_warnings_keep_key_spans |

Receipt (base `0c6c622` plus the implementation diff, subsequently committed as
`0104c6a`; formatting only followed the mutation run):
`E:/Development/_cache/cargo-target/kwil-kw-25-commands-keymaps/fail-first-c40f30ea8f344483bb584e4e9178a50e/summary.txt`.
Runner PID 65664; mutant Cargo PIDs 34492, 70392, 49536, 63500, each bounded to
120 seconds. All exited; the original worktree was never mutated by the script.

### Official Windows shortcut audit

Sources checked 2026-10-23 JST:

- VS Code [official Windows PDF](https://code.visualstudio.com/shortcuts/keyboard-shortcuts-windows.pdf),
  page 1, Basic editing, Navigation, File management and Display; the
  [official reference](https://code.visualstudio.com/docs/reference/default-keybindings)
  additionally confirms Undo/Redo omitted from the PDF.
- Notepad++ [Parameters.cpp at fd2f5c2](https://github.com/notepad-plus-plus/notepad-plus-plus/blob/fd2f5c27839e943518b5a53325ffd5e95bb3e0d9/PowerEditor/src/Parameters.cpp):
  `winKeyDefs` (Ctrl/Alt/Shift booleans in that order) and `scintKeyDefs`.
  Read as a factual shortcut reference only; no upstream implementation copied.

Reported keys, all now covered by `reviewed_presets_match_windows_shortcuts`:

| Preset | Shortcut | Command | Result |
|---|---|---|---|
| VS Code | Ctrl+Shift+K | edit.deleteLine | added |
| Both | Ctrl+Tab | file.nextTab | added |
| Both | Ctrl+Shift+Tab | file.previousTab | added |
| Notepad++ | Ctrl+Shift+S | file.saveAll | corrected |
| Notepad++ | Ctrl+Alt+S | file.saveAs | added |
| Notepad++ | Ctrl+L | edit.cutLine | corrected; registered Cut Line |
| Notepad++ | Ctrl+Shift+L | edit.deleteLine | added |
| All three | Ctrl+Shift+P | view.commandPalette | intentional palette policy |

Another 20 common bindings per desktop preset, excluding those reported keys.
Every row is checked in the same regression test. "Kept" means already correct
at `0c6c622`; "added" means a supported-v1 gap found in this audit.

| # | VS Code shortcut → command | Result | Notepad++ shortcut → command | Result |
|---|---|---|---|---|
| 1 | Ctrl+Shift+S → file.saveAs | kept | Ctrl+N → file.new | kept |
| 2 | Ctrl+D → edit.selectNextMatch | kept | Ctrl+O → file.open | kept |
| 3 | Alt+Shift+Down → edit.duplicateLine | kept | Ctrl+S → file.save | kept |
| 4 | Alt+Shift+Up → edit.duplicateLine | added | Ctrl+W → file.close | kept |
| 5 | Alt+Up → edit.moveLineUp | kept | Ctrl+Shift+T → file.reopenClosed | kept |
| 6 | Alt+Down → edit.moveLineDown | kept | Ctrl+Z → edit.undo | kept |
| 7 | Ctrl+K Ctrl+C → edit.comment | kept | Ctrl+Y → edit.redo | kept |
| 8 | Ctrl+K Ctrl+U → edit.uncomment | kept | Ctrl+X → edit.cut | kept |
| 9 | Ctrl+/ → edit.toggleComment | kept | Ctrl+C → edit.copy | kept |
| 10 | Alt+Z → view.toggleWordWrap | kept | Ctrl+V → edit.paste | kept |
| 11 | Alt+Shift+F → edit.formatDocument | kept | Ctrl+A → edit.selectAll | kept |
| 12 | Ctrl+] → edit.indent | added | Ctrl+D → edit.duplicateLine | kept |
| 13 | Ctrl+[ → edit.outdent | added | Ctrl+Q → edit.toggleComment | kept |
| 14 | Ctrl+Shift+Backslash → navigation.matchBracket | added | Ctrl+K → edit.comment | added |
| 15 | Ctrl+K S → file.saveAll | added | Ctrl+Shift+K → edit.uncomment | added |
| 16 | Ctrl+K Ctrl+W → file.closeAll | added | Ctrl+B → navigation.matchBracket | added |
| 17 | Ctrl+K Ctrl+F → edit.formatSelection | added | Ctrl+R → file.reload | added |
| 18 | Ctrl+K Ctrl+X → edit.trimTrailingWhitespace | added | Ctrl+Shift+W → file.closeAll | added |
| 19 | Ctrl+K M → file.setType | added | Ctrl+PageDown → file.nextTab | added |
| 20 | Ctrl+Shift+H → search.replaceInFiles | added | Ctrl+PageUp → file.previousTab | added |

Remaining existing desktop-preset bindings were checked too: file new/open/save/
close/reopen; undo/redo; cut/copy/paste/select-all; find/replace/find-in-files;
F3/Shift+F3 and Ctrl+G. Extra supported defaults added and regression-tested:
VS Code F1 (palette), Ctrl+, (settings), Ctrl+Alt+Up/Down (cursors); Notepad++
Ctrl+Shift+Z (redo), Tab/Shift+Tab (indent/outdent), Ctrl+Shift+Up/Down (move line).

Ctrl+Shift+P remains palette in every preset by coordinator decision, replacing
Notepad++ macro playback (outside v1). This is explicit in its preset header and
README. Palette-context Enter/Escape/Up/Down and diff-context F7/Shift+F7 are Kwil
extensions. MRU ordering remains a frontend concern. The Windows PDF's physical
Ctrl+Shift+Backslash also has a Ctrl+Shift+| logical-key alias for US adapters;
other layouts can override it. No native keyboard capture was performed.

Settings diagnostics retain parser-owned spans for parse/type failures and
unknown-key warnings; semantic validation uses the exact field path, including
indexed bindings and formatter names. Unknown-key path strings remain compatible;
`unknown_key_diagnostics` adds one-based line/column. No location-lookup dependency
was needed. Shift on punctuation/digits is preserved, with no inferred layout
translation; notation and adapter tests cover all US number/symbol pairs,
slash/question mark and bracket/brace pairs.

Not run: full-workspace tests, GUI/TUI/native execution, keyboard capture,
performance benchmarks, Linux/macOS local checks, publication or merge. Parent
owns PR CI monitoring and independent re-review.