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
