# Renderer integration and remaining platform gates

The CPU renderer is the onscreen backend. It retains a viewport-sized pixel image,
uses native decorations and presents only damage. No event-loop timer or frame loop
is installed. Empty model changes do not request redraws. OS expose requests restore
the retained image; buffer age does not force text layout or glyph rasterization.

## App integration

- `run` keeps its existing signature and flags. `run_with_events` adds an app callback.
- `GuiEvent::DroppedFile(PathBuf)` requests opening a file; it never replaces an edited
  buffer implicitly. Windows OLE is registered after the first CPU present. macOS/Linux
  drop events come from winit. The app integration package owns handling these requests.
- `GuiEvent::RenderPolicyChanged(RenderPolicy)` exposes `cursor_blink_allowed()`,
  `smooth_scrolling_allowed()` and `translucency_allowed()`. Apply them to future cursor
  and animation work. Drawing here is already opaque and instantaneous.
- Settings use `core-commands::Settings::path/load` on a worker after the first frame.
  Windows `SM_REMOTESESSION` supplies the initial detector, with registered WTS session
  messages for live changes. A disconnected remote session remains remote until a
  console connection. Linux checks xrdp/VNC/X2Go environment hints. macOS uses the
  explicit setting. `auto`, `on`, and `off` preserve their override semantics.
- This renderer reads settings at startup. Live settings-file watching belongs to app
  integration; it installs no separate watcher or polling loop.
- `text::cells(bytes, columns, tab_size)` exposes grid columns, widths and original byte
  ranges for input/hit testing. CJK occupies two cells, combining/emoji sequences stay
  clustered, tabs advance to configured stops (default four), and invalid bytes each
  display as `\xNN`. At most 64 KiB is inspected per visible row; individual clusters
  are capped at 256 bytes and layout at 4096 columns. Pathological longer combining
  sequences are clipped for display only. Original bytes remain untouched.

ASCII uses fontdue tiles. Non-ASCII runs are itemized by script and fallback face and
shaped/rasterized with swash on a deferred worker. Before their lazy font is ready,
the first CPU frame uses a visible placeholder of the correct grid width. Font discovery
never runs on the first-frame path. Combining clusters prefer the same primary font as
ASCII, and fallback can render colour emoji. Positive and negative fallback results
are cached (4096 entries); loaded fallback font data is capped at 16 faces / 64 MiB,
and Unicode pixel tiles at 8 MiB. Missing or over-budget faces show a visible box.
The queue is bounded; temporarily full queues retry their affected rows. Deferred
glyph damage follows copied scroll pixels. Old font workers cancel after DPI changes.

## What the compositor receives

| Platform | Pixel movement | Presentation metadata |
| --- | --- | --- |
| Windows CPU | `copy_within` plus same-window GDI `BitBlt` | Exposed rows and status only through softbuffer/GDI |
| X11 CPU | `copy_within` plus server `XCopyArea` on Xlib windows | Exposed rows and status; if no native copy is available, moved pixels are also damaged |
| Wayland CPU | `copy_within`; Wayland has no window-pixel copy request | Moved pixels plus exposed rows via `damage_buffer`; surface protocol versions below 4 receive full damage |
| macOS CPU | `copy_within`; no public softbuffer scroll-copy seam | softbuffer 0.4.8 ignores damage and replaces the CALayer image; actions are disabled |

Wayland must damage moved pixels: advertising only the exposed strip would leave stale
content in a compositor that applies damage incrementally. macOS partial presentation
and compositor-level scroll copy are **open**, not claimed implemented. X11 and macOS
native behavior has not been exercised on this Windows machine.

## GPU status: partial completion

The `gpu` feature initializes wgpu on a worker after the first CPU frame and settings
load, only locally. Windows builds enable **DX12 only**; macOS enables Metal; Linux
enables Vulkan/GL. The retained GPU texture receives damage uploads and copies scrolling
pixels through a scratch texture. Remote mode stops these updates; returning locally
invalidates the backing image so skipped remote updates cannot leave it stale.

**There is no onscreen GPU takeover.** wgpu 30's public
[`Queue::present`](https://docs.rs/wgpu/30.0.1/wgpu/struct.Queue.html#method.present)
accepts no dirty/scroll rectangles. The coordinator approved retaining CPU onscreen
presentation and treating the worker/backing work as partial completion, rather than
adding a native DXGI implementation in this package. `onscreen_takeover=false` is
explicit in the trace. No GPU `PrintWindow` capture or switchover time is claimed.

Native caption buttons, snap behavior and resizing remain OS-owned. Windows dark/light
caption mode follows the configured theme/OS-follow setting; Mica is requested only
locally, after settings resolve. Remote mode requests `DWMSBT_NONE`. DPI events rerasterize
glyphs on a bounded worker (one load in flight). macOS keeps traffic lights/native
fullscreen; Linux keeps winit's offered server-side or client-side decorations. Native
menu-bar work and owner side-by-side acceptance stay with KW-32/KW-39.

## Dependency evidence

Latest stable versions checked through crates.io's primary API on 2026-10-23. All are
MIT or MIT/Apache-2.0; no non-permissive dependencies were added. swash was selected
because it supplies shaping/rasterization without adding another layout engine to
the existing grid/ASCII path. Alternative stacks were not benchmarked.

| Direct dependency | Release date | Observed release compile unit, `-j 4` |
| --- | --- | --- |
| swash 0.2.10 | 2026-07-17 | 8.13 s; its skrifa dependency 8.47 s |
| fontdb 0.24.0 | 2026-07-29 | 0.71 s |
| wgpu 30.0.1 | 2026-08-22 | 4.18 s; core 17.16 s, HAL 6.83 s, naga 18.57 s |
| pollster 1.0.1 | 2026-07-10 | 0.07 s |
| unicode-width 0.2.2 | 2025-10-06 | Already cached/shared; not timed anew |
| unicode-segmentation 1.13.3 | 2026-06-01 | Already cached/shared; not timed anew |
| x11-dl 2.21.0 | 2023-01-18 | Existing Linux transitive dependency; not measured on Windows |

These are overlapping Cargo unit durations, not additive clean-build wall times.
The branch's external target directory was reused; no cache was cleaned. The first
instrumented GPU build's dependency timings were retained even though the final crate
failed on a readback API compile error subsequently fixed. No claim of a passing clean
whole build is derived from that failed invocation. Later release builds passed.

The 32x32 embedded RGBA icon is a raster of the existing `assets/logo/kwil-icon-small.svg`,
not a redesign. `scripts/raster-icon.ps1` reproduces it using resvg 0.48.1 (MIT/Apache,
2026-08-02), a separate cached build tool that adds nothing to the app graph. SVG SHA256:
`1cd225f27698e6ccc74176d836d0346ad7308c4b9b45a08656ccee7c355a3e52`;
RGBA SHA256: `93adb7c5e9df6a130f7941a9a74fcb4a805b9a9babc99f3eee5a85223144766c`.
Both the Windows small/window and big/taskbar icon slots use this icon.
