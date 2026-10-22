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
