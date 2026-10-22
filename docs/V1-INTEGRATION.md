# v1 integration checkpoint — 2026-10-23

This is intermediate evidence, not v1 acceptance or a release.

## Verified source

Production source tested at `34dbdcf3edf9076e1aa1a96f55636e71e6c8964c`.
CLI PID logging added in `9f4146cf569171fc613ba384e7ab19774d26d611`.
Formatting-only module ordering produced `0b2800b1d226361a09dd93f72c8b941ed7664668`.

Focused Windows checks used the configured shared Cargo target, offline/locked,
`-j4`, one admitted process at a time, and bounded child timeouts:

- Shared palette lifecycle/input/query viewport: 4 passed.
- Session horizontal-origin codec, including KWS1 migration: 2 passed.
- Zoom shortcuts and explicit unbinds across all three presets: 2 passed.
- GUI zoom/DPI sizing and dispatch contracts: 3 passed.
- IME destination/epoch/prompt/cursor/mode contracts: 4 passed.
- GUI and console CLI help/version integration: 4 passed.
- All-target/all-feature Clippy with warnings denied passed for view-model,
  GUI, TUI and app, including their dependencies.
- Workspace formatting check passed after module-order correction.

Three independent compiling mutations were rejected by named assertions:
query-caret scrolling, horizontal-origin decoding, and DPI multiplication.
Original file hashes were restored in `finally`; palette/codec/zoom checks and
Clippy/formatting passed again afterwards. The reusable harness is
`scripts/check-integrated-ui-mutations.ps1`.

Logs: `E:/Development/_cache/kwil-root-v1-*.{identity,stdout,stderr}`.
Mutation proof: `E:/Development/_cache/kwil-integrated-ui-proof-2406f9a9867346f3bdcb579be628bfbd/summary.log`.
CLI children 30152, 14840, 70496 and 27324 completed normally; the four commands
were `--help`/`--version`, with no GUI window. All owned Cargo processes exited.

## Integration limits

- Palette overlay modules are prepared but not registered or wired into the
  frontend event loops. Their drawing tests have not executed. The shared host
  is compiled/tested; this does not complete the palette feature or 100k-file gate.
- Tabs/hot-exit source has six independently reviewed save/recovery defects.
  Follow-up `5a148d9` is awaiting its focused validation/re-review. No native
  owner build is accepted from the affected checkpoint.
- Theme backend has three review corrections; formatter has four preservation
  corrections under preparation. Neither package is accepted as a complete UI feature.
- Native DPI/monitor movement, zoom captures, session/crash recovery, RTL visual
  caret mapping, remote transport, full platform and speed gates remain open.
- Actual app startup remains above the 40 ms target. GPU onscreen takeover and
  macOS partial-presentation/copy are incomplete under the documented CPU policy.
- Log/compressed autodetection is queued on KW-41/KW-49, not implemented here.

Main advanced separately through reviewed launcher PR #10, merged as
`f37e23f9d93544e0cf5fe8142645afbb848f5665`; that merge does not imply approval of
this complete integration branch. Only the owner publishes releases.

## GUI palette/native checkpoint

GUI event wiring was added in `9618120` and corrected through `6d660f2624cbe0def180a72b8f8b3ebe4b0c31bb`.
The earlier statement that the GUI overlay is unregistered is superseded here;
the TUI overlay remains unwired. Native execution proves display/query filtering,
not full palette action or performance acceptance.

- Palette overlay geometry test passed after correcting an initial IME fallback
  return-type compilation error. No compiler failure was counted as a pass.
- Integrated session R2 checks at `db16771`: workspace 21, document worker 13,
  IME 6 passed. Native destination helper at `ad90390`: 15 Windows tests passed;
  symlink-dependent cases are conditional on OS permission.
- Targeted three-package all-feature/all-target Clippy passed at `01c187a` after
  fixing two palette lint errors. Workspace formatting passed at `6d660f2`.
- Debug/frame-harness build at `6d660f2` completed in 12.39 seconds (Cargo PID 73324).
- Copied executable SHA256:
  `FBA8E871C38ECD12A1386E4FE206471FCA4517CD81A405D77341B1C774CCDAE6`.
- Hidden capture runner PID 63684 and GUI PID 31952 both exited 0. Native caption,
  controls, small/big icons and DWM cloak were verified before interaction.
- Captures show two tabs, typed document text, worker-filtered `>save` results,
  and visibly larger text after real zoom dispatch. Scoped hidden/debug function
  keys exercised commands; no global keyboard, clipboard, display or owner
  profile manipulation occurred. Actual Ctrl shortcuts and monitor movement
  were not exercised natively.

Evidence: `E:/Development/_cache/kwil-palette-native-20261023/` contains the copied
binary, identity, build/check logs, probe script, isolated profiles and captures.
No agent-launched window appeared on the owner's desktop.

### Terminal palette routing and input identity (2026-10-23)

Product commit `f0bb765` adds the terminal palette controller and `KeymapEngine::invoke_named`; GUI palette commands use the same Vim transition. Query typing/paste stays in the palette even in Vim Normal mode. Named commands cancel earlier operators/counts. Worker matching/discovery remains in `PaletteSession`.

Validation: TUI library 19 passed before adding the two controller regressions (PID73776); all three palette tests passed afterward (PID79016). Strict GUI/TUI all-target/all-feature Clippy passed (PID72872). Initial syntax and needless-return failures were corrected, not counted as passing checks. The latest GUI prefix helper from PR21 was merged at `9d43cfd`; its two integrated tests pass (PID27388). Formatting and diff checks pass. Every recorded process exited.

No terminal/native palette-action capture, 100k-file latency measurement, full suite or full-v1 acceptance ran for these commits. Read-only palette and final tabs/session reviews are active. Existing native screenshot evidence remains pinned to `6d660f2`.

### Palette frontend review R1 corrections

`dc0aca6` / `bbd60a1` correct all five R1 source findings: TUI mouse dismissal restores Editor context; a previously displayed overlay blocks native scroll-copy through accepted actions; frontend-owned Go to Line dispatch precedes Vim model execution; palette-context Ctrl+V/Cmd+V bindings preserve tombstones/chords; open palette tab metadata refreshes after addressed workspace events without restarting unchanged queries.

Proof: mouse-context compiled/assertion-failed PID67988 in `E:/Development/_cache/kwil-palette-frontends-proof-c6281d21d48446c2ae9d1ef089490f21`; overlay-copy, paste and tab-publication independently compiled/assertion-failed PIDs45100/49576/11140 in `E:/Development/_cache/kwil-palette-frontends-proof-57185a6fbba94c3b80c8b181f6a8fdf2`. Source restore hashes are checked. The first overlay proof was rejected by the harness because its custom panic text omitted the word assertion; the retained log still shows compilation and the intended failure. A later empty selection executed zero cases and is not evidence; the script now refuses zero selected cases.

Restored frontend palette tests: 6 passed PID38512. Restored tab publication: 1 passed PID18620. Strict three-crate all-target/all-feature Clippy PID18824 and formatting/diff checks pass. All owned processes exited. Go-to dispatch is source corrected; no accepted GUI action/native artifact or 100k-file latency result is claimed.

### Viewport highlighting integration

`684de1e` connects a shared `HighlightSession` to GUI/TUI worker events. Cached absolute row windows avoid extra source reads for styling. Disjoint visible windows remain bounded across giant-line gaps. The syntax worker also retires frontend source handles through a fixed eight-slot queue; there is no per-edit thread. GUI highlight updates damage only rows whose local styles changed. ASCII colouring draws runs; the TUI retains selection backgrounds and horizontal byte positions.

Four independent compiling faults were detected: oversized merged windows, all-row damage, source destruction on the caller, and missing terminal foreground styling. Proof: `E:/Development/_cache/kwil-highlight-integration-proof-92d7b5dad0514595a33c54e289520f88`, PIDs79132/50836/38616/57384, each exit101 with intended assertion; source hashes restored. An earlier malformed let-chain mutant did not compile and is not evidence. Restored session tests3 PID74700, source retirement1 PID9536, terminal rendering1 PID73636 pass. Strict three-crate Clippy PID70780 and formatting pass; the final ASCII run adjustment is a later source change awaiting native inspection.

Limits remain explicit: initial wiring uses automatic detection and built-in dark syntax colours. Language picker/settings mapping and active theme application are not yet wired. Logical-cell recolouring is skipped for RTL paragraphs to preserve their existing visual shaping; full RTL editor mapping remains open. GUI native pixels, latency and cross-platform acceptance are not established by these tests.

### Palette R2 corrections

`08a5c3b` / `d033846` preserve pending non-tab queries during tab metadata updates and retain GUI clipboard adapters for accepted Vim Copy/Cut/Paste. A condition-variable regression deliberately holds matching pending during refresh. Two independent compiling assertion failures are retained in `E:/Development/_cache/kwil-palette-frontends-proof-201ac392d14f42a49df31bcea3662f42`, PIDs53204/60660. Restored host5 tests PID62452 pass. Earlier custom assertion text caused one harness rejection; that run is not counted. R3 source review and accepted native GUI actions remain open.

### Reopen reservations R4

`f46df0e` stages named cached reopens through a bounded native-admission worker request before consuming history/cache. Provisional reservations survive ordinary registry refresh until the addressed UI receipt. Evidence: `E:/AgentData/scratch/kwil/kw20-reopen-r4/` (workspace27, worker14, fail-first cases, one compiling provisional-reservation mutant, exact restoration, strict VM Clippy and fmt). Independent R4 review is running. No native, latency or full-v1 acceptance is claimed.

### Native syntax and accepted palette actions at 9021239

A copied debug `frame-harness` binary built offline/locked/-j4 in 5.13 s (Cargo41780). Executable SHA256 `875D6C9AB689D990629316175344E756F1806FF899B31E4AFC34468E7022A182`; source `90212390348bf1cbcc33581a7005e50bc0194d7f`. Hidden isolated runner57728 and GUI64560 exited0. The probe verified native caption/resize/min/max/system-menu, both icons and DWM cloak. APPDATA/XDG/KWIL_WORKSPACE_ROOT were isolated beneath `E:/Development/_cache/kwil-highlight-native-20261023/capture/`; no global input, clipboard or display changes.

Visually inspected `highlighting.png`, `goto-prompt.png`, `goto-accepted.png`, `new-file-accepted.png` in that capture directory. Rust keywords/strings/numbers/comments are coloured. Accepting Go to Line opens `:` mode, then accepting `55` places the caret on line55 with no remaining popup pixels. Accepting New File creates the second empty tab. Fixture SHA256 stayed `90397D146B53A1B15BAF3BF4BBCA1706E6DAC04AB0C63010046D9C841912EBA5`; app stderr was empty. This establishes these actions on this Windows candidate; no latency, native clipboard, all-platform, final incremental-parse or full-v1 acceptance is claimed.

### Recovery dirty-state and highlighting R1 follow-ups

R4 review found a reserved-saved-state collision after reopening recovery. The extended regression assertion-failed before the correction (PID78036). `05ff3aa` allocates an unreachable saved marker, skips it for later edits, and restores the allocator beyond persisted saved markers. Positive cached reopen/SaveAll/undo/redo/close and saved-marker100 recovery tests pass (restored28124/8884). Independent compiling reservation/restore faults were rejected in `E:/Development/_cache/kwil-recovery-marker-proof-3d7f32c898b24182a4ecd31a60565192`, PIDs52836/13140. Independent R5 review remains required.

`9021239` additionally fixes terminal highlighting erased by a trailing-space edit and Save As language detection without a revision change. Both corresponding compiling faults are rejected in `E:/Development/_cache/kwil-highlight-integration-proof-df63b01eed93410991c9f043ec076843`, PIDs58864/10712; restored tests PID17680/54960 pass. The remaining R1 full-reparse finding is being corrected through bounded transaction metadata and worker-side incremental edits. It is not waived by the native capture.

### Incremental syntax and recovery review closure

`0b770f1` provides a separate, bounded syntax transaction descriptor without replacing the journal observer. Exact sequential edits are converted to tree-sitter byte points on the syntax worker and staged before one parse. Missing chains, evicted recipes, cancellation and failures reset the parser. The model metadata slot holds at most 4096 shapes (98,304 bytes plus fixed overhead), with no extra snapshots or payloads; the worker retains one prior source beyond the existing bounded queues. Duplicate frontend source handles are released before retirement.

Evidence in `E:/Development/_cache/kwil-incremental-r2/`: 27 focused contracts pass (highlighting16, receipts3, compound5, Engine staging2, retained incremental1). Three independent compiling mutants failed at intended assertions: metadata16096, undo coordinates2184 and staging56036. Restored highlighting16 PID36000 and Engine2 PID38528 pass. Strict two-crate all-target/all-feature Clippy77052, formatting69180 and whitespace checks pass. All child processes exited. Independent R2 review matched all four hashes and the archived patch to this commit and found no actionable scoped issue.

Independent KW20 R5 review at `05ff3aa` also cleared the saved-state identity collision, preserving guarded close, Save All, reservations and previous recovery protections. These reviews do not establish native/performance, full-platform, disk-growth or full-v1 acceptance.

Hosted Windows full tests passed for the earlier `d2147ea` candidate. Unix jobs initially stopped at Clippy: `49771c9` simplifies equivalent IPC guards, and `6d189e7` uses an octal permission literal in its Unix-only test. Their final hosted run remains pending. No local full suite was run.

### Live settings screen, first integration

`637699d` adds the shared settings lifecycle. `4481a49` connects it to both frontends after their first frame, replacing one-shot settings reads with worker-backed reloads. The generic screen supports category navigation, search, validated edits, reset and automatic saves. Runtime settings apply only after an accepted load/save receipt. Open Settings File creates a missing file on the worker before opening it. Text/IME input is confined to the panel, and a displayed panel blocks document scroll-copy. Unrelated live settings changes preserve the current Vim mode.

The settings host has ten focused contracts and five independent compiling mutants, retained in `E:/Development/_cache/kwil-settings-session-49771c9`. Root validation: twelve selected model contracts and one actual terminal keyboard/save/render contract pass (restored Cargo52476). Missing-file creation and terminal Enter independently rejected compiling faults, PIDs19272/32784, in `E:/Development/_cache/kwil-settings-frontends-proof-ae39d7ea11cb4a8e9c6e86b24704ef94`. Strict GUI/TUI/model all-target/all-feature Clippy76204 and rustfmt43840 pass. Earlier Clippy27112 found one nested conditional, corrected before the final pass. An earlier terminal fault run reached the intended custom assertion but was rejected by the harness's log-text check; it is not counted.

Hidden Windows candidate: UI source `4481a49` (built before final whitespace formatting), plus the then-uncommitted additive, unused `TextFile::try_close` seam. Build Cargo4616 exited0 in5.16s. Copied executable `E:/Development/_cache/kwil-settings-native-20261023/kwil.exe`, SHA256 `95F58ABDCC4C71E03EE34F9362BBA12F3F7562155DE4E03F2E8FA25F82869497`. Isolated runner13576 and GUI46528 exited0, owned HWND175243630. Caption/icons/cloak checked; no global input or owner profile/clipboard/display changes.

Visually inspected `capture/settings-home.png`, `settings-saved.png` and `vertical-tabs.png`. Accepting Open Settings from the palette displays the screen. Searching `tabs layout`, accepting the choice, and closing the panel immediately shows vertical tabs. The isolated `capture/config/kwil/settings.toml` contains `layout = "vertical"`; app stderr is empty. Fixture SHA256 remains `90397D146B53A1B15BAF3BF4BBCA1706E6DAC04AB0C63010046D9C841912EBA5`.

This is partial KW46 integration, not full settings/v1 acceptance. Claude supplied the binding native-renderer layout design; category rail and basic keyboard navigation are implemented, but specialized key capture/conflict and file-type/association panels, dynamic themes, full visual layout/damage polish and native Open Settings File acceptance remain open. Independent lifecycle/frontend review is running. Hosted run39865843591 passed Linux and Windows; macOS reached a catalogue fixture using a filename APFS rejects. `08a41ff` uses an APFS-supported Unicode filename while retaining arbitrary-byte identity coverage on Linux and surrogate coverage on Windows. Its next hosted result remains pending.
