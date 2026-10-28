# GUI daily-use verification (KW-58 / KW-48)

Base: `origin/main` at `75a73a44f142978258312c8e220a242a11376f14`.
Implementation: `9334f3ae347d899cf52f555ad6bdbab7d67ec032`.
Branch: `kw/beta2-gui-daily-use`. No WIP UI branch was imported.

## Resize cause and correction

The existing resize callback retained the latest size, but Windows could deliver a
redraw after every individual resize. Every redraw then resized the CPU surface,
relayed out the viewport, repainted it and, on DX12, recreated swapchain resources.
The base probe produced 50 full presents for 50 intermediate sizes.

- An eight-millisecond one-shot deadline coalesces resize/redraw pairs before
  surface allocation or painting. New sizes replace the pending geometry without
  extending the deadline indefinitely.
- A one-shot native timer also wakes inside Windows' native move/size loop.
  Consuming the deadline cancels the timer; asynchronous presenter deferral also
  clears it, so an expired deadline cannot create a busy event loop.
- Windows native move/size state selects the existing CPU presenter during the
  drag. The existing GPU presenter resumes at the final geometry after exit,
  avoiding driver swapchain recreation on every drag frame. No renderer or
  dependency was replaced.
- Settled idle drawing remains event-driven with zero presents.

The existing finite `frame-harness` now sends 50 actual App resize callbacks,
each immediately followed by an App redraw callback. This intentionally models
interleaved `WM_SIZE` / `WM_PAINT`, rather than only measuring the cheap size
callback. The fixed Windows probe additionally sends native enter/exit-size-move
messages and asserts CPU ownership during the drag. It changes the real native
window to the final 1196 x 748 client geometry and captures it.

Times below include the resize callback plus immediate redraw handling, in ms.
Deferred final painting and GPU handoff are outside these per-step times.
These are bounded hidden App probes, not measurements of physical mouse input
or end-to-end compositor latency.

| Backend | Base min / mean / max | Fixed min / mean / max | Intermediate presents |
| --- | --- | --- | --- |
| CPU | 4.313 / 5.740 / 9.150 | 0.003 / 0.083 / 0.221 | 50 -> 0 |
| Auto (DX12) | 12.220 / 14.975 / 20.329 | 0.001 / 0.034 / 0.126 | 50 -> 0 |

Every fixed step passes the 8.333 ms assertion. The final CPU run recorded one
full present plus a partial update; auto recorded two full presents plus a partial
update across CPU painting/GPU handoff. Both then passed the zero-idle-present
assertion. Window-DC CPU captures show the correctly sized menu, tab, editor,
caret and both scrollbars. The final auto capture verifies DX12 readback against
the retained CPU pixels; it is not a DWM compositor capture.

Full per-step arrays, executable hashes and captures are in these evidence roots:

- CPU base: `E:/AgentData/scratch/kwil/gui-daily-resize-55492-e7fc066b694745209f820eb31a82f46c/`
- Auto base: `E:/AgentData/scratch/kwil/gui-daily-resize-48988-80b2b35ae5d74d4b8a8f1649a425d12c/`
- CPU fixed, PID 69212: `E:/AgentData/scratch/kwil/gui-daily-resize-70732-c780b01e59ad47ac8660d1f565a26d3d/`
- Auto fixed, PID 66976: `E:/AgentData/scratch/kwil/gui-daily-resize-71444-12894c67ecf242c4807f98ad66807c3c/`

Each root contains `resize-timing.txt` and `artifact.txt`. Fixed roots also contain
`resize-drag.png`, `resize-final.png`, `resize-idle.txt` and `frame-counts.txt`.
The unchanged production base assertion failed with
`intermediate resize paints must be coalesced` (PID 67776).

## KW-48 results

Existing editing, scrollbars, native decorations and icon assignment were already
present on main. One additional real gap was found and fixed: Select All / EOF
caret reveal could set the horizontal offset beyond the scrollbar range, leaving
an empty-looking viewport. Caret reveal now clamps to that range, and the range
reserves one cell for the caret after the final glyph. Its App assertion failed
before correction (PID 64196) and passes after it.

Final hidden daily-use evidence, PID 55624:
`E:/AgentData/scratch/kwil/gui-daily-daily-48184-1212b0d620e34d88967e12743489d0ed/`.

| Requested check | Result and evidence |
| --- | --- |
| Vertical/horizontal scrollbars | Works: both overflow ranges are nonzero; track clicks page and thumb drags reach both endpoints. `daily-scrollbars.png` shows rightmost line endings and the bottom scrollbar positions. |
| Typing and caret | Works: App IME input inserts `daily `, advances the caret by six bytes and paints the dirty marker. `daily-typed.png`. |
| Selection | Works: mouse drag selects text in `daily-selection.png`. Select All remains visible at EOF after the correction, in `daily-saved.png`. |
| Undo/redo | Works: command routing removes and restores the exact inserted prefix. Assertions in the existing App harness. |
| Save | Works: asynchronous save completes, marks the revision clean and matches the original fixture plus the exact prefix on disk. `daily-use.txt`. |
| Native window controls / resize | Works: caption, thick resize frame, minimize/maximize styles and enabled native Close command are asserted. Real final client geometry is captured. `native-window.txt` and resize captures. |
| Header/taskbar icon | Works at the native API: both `WM_GETICON` small/big handles are present, populated from the existing Kwil asset. Normal launches retain taskbar visibility; only the `--hidden` attribute branch requests skip-taskbar and a cloak. `native-window.txt` and window-creation source. |

Saving emits the existing best-effort Windows ACL/owner preservation warning
(`Access is denied`, OS error 5), while completing byte-exactly with a clean
revision. The warning is preserved; no storage or privilege policy was changed.

## Reproduce and checks

Use a process-local target directory and `KWIL_NO_UPDATE=1` (the script supplies
the latter). All profile/workspace/instance state is isolated per invocation.

```powershell
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-beta2-gui-daily-use'
cargo build -p kwil --features kwil-ui-gpu/frame-harness --locked -j 4
powershell -NoProfile -ExecutionPolicy Bypass -File crates/ui-gpu/scripts/daily-use.ps1 -Executable "$env:CARGO_TARGET_DIR/debug/kwil.exe" -Probe resize -Renderer auto -RequireCoalescing
powershell -NoProfile -ExecutionPolicy Bypass -File crates/ui-gpu/scripts/daily-use.ps1 -Executable "$env:CARGO_TARGET_DIR/debug/kwil.exe" -Probe daily
```

The execution-policy argument applies only to that child PowerShell process.
The script records its launched PID, limits the native run to 20 seconds and
closes only that process if necessary. All final native PIDs exited normally.

- Focused `kwil-ui-gpu` scrollbar tests: 11 passed.
- `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`: passed.
- The same strict Clippy check with `kwil-ui-gpu/frame-harness`: passed.
- `cargo fmt --all --check` and `git diff --check`: passed.
- Final debug executable SHA256: `FD0E67B1F1E35F5C02A33BE60B083BAD2E9803510FEC91B5C01B9D05EEDD4993`.

Not verified: physical owner mouse/keyboard interaction, visible taskbar/caption
appearance, sustained manual drag feel, native macOS/Linux, full CI suite and
release executable. No full local suite, release, merge or publication was run.
