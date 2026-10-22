# KW-11 / KW-12 / KW-39 verification

Base: `e51e41bd687276e99eef2dbfee60ac9a88586119`, branch `kw/11-renderer`.
PR: https://github.com/rennerdo30/kwil/pull/14. No merge or release.
Only `crates/ui-gpu/**` and dependency resolution in `Cargo.lock` changed.

## Review correction, source b09c102

The independent review of `1b9016c` requested three fixes. `ae7e029` corrects the
queued-scroll/glyph-result interleaving, Unicode recovery after an unavailable font,
and directional visual ordering. `b09c102` also preserves cursive advances after the
first corrected capture exposed gaps with proportional Arabic fallback.

- **15/15 focused ui-gpu tests pass.** All 15 have assertion-level mutant fail-first
  evidence: the original 14 at `review-r1-unit-mutants/`, the added cursive invariant
  at `review-r1-cursive-mutant/`, under the evidence root below. This includes all
  prior width/escape/tab/remote/cache tests as well as new queue/status/RTL cases.
- The queue regression executes scroll(2), glyph arrival for old row 5, then another
  queued scroll before paint; the dirty row follows the copied pixels. Fixed status
  glyph damage is checked separately.
- Default and all-feature workspace clippy passed with warnings denied; fmt passed.
  No full suite or startup benchmark was rerun. The source remains based on the
  original pinned branch; the coordinator owns PR13 integration overlaps.
- A native deliberate mutation removed the failed-font recovery activation. The
  hidden app reported `unicode_enabled=false` and failed the `Unicode recovery
  unavailable` assertion (PID 43544, expected nonzero exit). Restoring the code
  recovered CJK/emoji/combining/invalid-byte and RTL display despite the unavailable
  configured family, preserving a visible font-unavailable notice.
- Final copied-binary capture (`b09c102`, PID 72952, exit 0) was visually inspected:
  Hebrew/Arabic order, mixed Latin/digits and connected Arabic are visible. No Cargo
  invocation ran during that capture; the compile lane had been released.
- Final probe: pending glyphs 0, Unicode enabled, idle 0 presents; scroll 1 partial
  present with **97,280 damage / 924,160 copied pixels**; status 1 partial present
  with **24,320 damage pixels**. Different counts reflect the surviving fallback
  font's 19-pixel cell height. Forced repaint is 1,024,000 pixels. Scroll capture
  byte-equals repaint; primed GPU backing readback equals CPU after scroll.
- Native controls, both icons, cloak and WTS/OLE/drop registration remain true.
  Session policy still switches remote/blink-off to local/blink-on. The native
  pre-cursive corrected run (PID 75640) also exited 0.
- Current stable `unicode-bidi 0.3.18` (MIT/Apache, 2024-12-16) adds one lockfile
  package. First focused dependency compile unit: 0.65 s. Final cached native
  release build took 18.64 s, including app thin-LTO linking.

Evidence root: `E:/Development/_cache/kwil-kw11-evidence/`.
Failing native run: `review-broken-font-33268-1/`.
Final visually checked capture: `review-restored-font-rtl-25624-1/first.png`.
The copied executable's SHA256 is
`4a494b65a3310e5aedf1510d52bdb7692bbbeb413460239c91a43348c8a5d7fb`.

GPU onscreen takeover, macOS partial presentation/copy, real RDP/VNC and the 40 ms
startup target retain their prior open status. The earlier measurements below are
retained as evidence for their named commits, not relabelled as current startup proof.

## Source/offline checks

- Nine focused renderer unit tests pass with all features on `21932a7`.
- Each of the nine tests failed by assertion against a deliberately broken function,
  then passed restored. `scripts/fail-first.ps1` reproduces this without an extra
  checkout. Logs identify the tested commit on their first line.
- Mutant evidence: `E:/Development/_cache/kwil-kw11-evidence/fail-first-4950ae2/`.
- Default workspace clippy and all-feature workspace clippy passed with warnings denied;
  `cargo fmt --all --check` passed. All commands use the branch's external Cargo cache
  and `-j 4`. Focused tests only; no full local suite or benchmark loop.
- Native release builds passed with the `kwil-ui-gpu/frame-harness` feature. Most recent
  code build (`21932a7`) took 19.19 s including thin-LTO app linking; preceding cached
  rebuilds took 17.48/17.53 s. These are not clean-build or dev-incremental timings.
- Direct dependency compile-unit costs and release/license evidence are in RENDERER.md.

## Startup: target remains failed

One baseline and one after measurement, actual `kwil.exe --hidden`, release profile,
same 120-line ASCII fixture, updates disabled. Times include process loader time.

| Mark | e51e41b plus trace-only instrumentation | 4950ae2 |
| --- | --- | --- |
| GUI entry | 50.260 ms | 47.549 ms |
| First CPU frame | 78.931 ms | 74.213 ms |
| GUI-entry to first-frame interval | 28.671 ms | 26.664 ms |
| DX12 device initialization | Not present | 295.912 ms |
| Onscreen GPU switchover | Not present | Not implemented |

The <=40 ms target is **not met**, including by the baseline. The <=100 ms hard limit
held for these two samples. These are single observations, not distributions or proof
of a statistically significant speedup. Later primary-font and big-icon corrections
were functionally captured on `21932a7`; startup was not remeasured after those changes.
The M0 renderer-only ~37 ms figure is not a current actual-app measurement.

Evidence: `E:/Development/_cache/kwil-kw11-evidence/baseline.stderr` and
`after-4950ae2.{stderr,stdout}`. Baseline PID 66244 was explicitly stopped after its
2-second observation; scripted after PID 75520 exited 0.

## Hidden native functional checks on 21932a7

Mixed fixture: ASCII, Japanese/Chinese/Korean, colour emoji including a ZWJ sequence,
combining accents, tabs, and invalid bytes FF/C0/80. Window-DC capture was viewed and
checked. The native window stayed DWM-cloaked and unfocused. The original icon is
installed in both `WM_GETICON` small and big slots. Native caption/resize/min/max flags,
subclass registration, session notifications, OLE initialization and drop-target
registration all reported true.

Window: 1280 x 800 = 1,024,000 pixels. With the current physical font cell height:

| Action | Presents | Full presents | Damaged pixels | Copied pixels |
| --- | --- | --- | --- | --- |
| Idle (250 ms) | 0 | 0 | 0 | 0 |
| Scroll three rows | 1 | 0 | 138,240 | 864,000 |
| Force full repaint for comparison | 1 | 1 | 1,024,000 | 0 |
| Status update | 1 | 0 | 34,560 | 0 |

Scroll damage is three exposed rows (103,680 pixels) plus status (34,560 pixels).
The copied area is 25 retained rows. The post-scroll window capture is byte-identical
to the forced-repaint capture. The harness primes the GPU backing before scrolling;
its post-copy readback equals the CPU image. No GPU onscreen/PrintWindow claim is made.

Before the measured idle interval the mixed-file settling phase had 11 presents,
2 full, 3,454,720 damaged pixels. This includes the initial surface, settings/DPI font
correction, index status, and arriving Unicode glyphs. Pending glyph requests were zero
at the idle boundary. No periodic redraw was installed.

Evidence and visually checked PNG:
`E:/Development/_cache/kwil-kw11-evidence/capture-21932a7-18148-1/`.
Its PNG is attached to Pair Desk KW-11. PID 61768 exited 0.
The earlier `4950ae2` mixed check also exited 0 (PID 32120).

Synthetic `WM_WTSSESSION_CHANGE` messages to the owned window changed auto policy to
remote/blink-off and back to local/blink-on without changing any OS session.
A separate, isolated settings run used `render.remote="on"`, `cursor_blink=true`,
`smooth_scrolling=true`: it stayed remote with blink disabled even after console-connect.
It produced the same idle/scroll/status counters; GPU backing readback was absent.
Evidence: `E:/Development/_cache/kwil-kw11-evidence/remote-21932a7-66692-2/`.
PID 35752 exited 0. No owned window process survived any run.

## Not run / not completed

- Onscreen GPU takeover, GPU `PrintWindow(PW_RENDERFULLCONTENT)` capture or switchover
  measurement: no damage-present API is exposed by wgpu 30. CPU remains onscreen by
  coordinator decision; the GPU portion is explicitly partial.
- macOS compositor scroll-copy/partial presents: softbuffer replaces full CALayer
  contents. Wayland cannot advertise only exposed pixels after copying moved content.
- Native Linux/macOS execution; compositor receipt and wire-bandwidth measurements.
- Real RDP/VNC session switching (KW-30); no console takeover was attempted.
- Actual OLE drag/drop gesture, moving between monitors/DPI values, native titlebar/Mica
  visual acceptance, macOS/GNOME/KDE owner side-by-side checks (KW-32/KW-39).
- Final-head <=40 ms startup acceptance, cold disk cache, or a full local test suite.

The coordinator owns independent review, CI monitoring and merge. KW-11 retains open
plan steps for the native presentation gaps and startup target. KW-12/KW-39 retain
their explicit native/owner acceptance limits; no owner verdict was set.
