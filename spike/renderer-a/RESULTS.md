# M0 renderer A (gpui 0.2.2) results

Measured 2026-10-23 on the owner PC: Windows 11 Pro 26300, Ryzen 9 9950X3D2, Radeon RX 9070 XT, 160 Hz display, Rust 1.99.1, release profile from `.cargo/config.toml`.

**Build and run status:** `gpui = "0.2.2"` from crates.io builds and runs on Windows (DirectX 11 + DirectWrite backend) with Rust 1.99.1. No distribution (`gpui-box`, `gpui-pre`) was needed. The only warning is a future-incompat note for `proc-macro-error2 v2.0.1`.

Command: `renderer-a E:/Development/kwil/fixtures/log-1g.log --script [--capture <dir>]`. The fixture is 1 GB with 20,353,527 lines. The script is shared with renderer B (`spike/spiketrace`).

## Numbers (5 runs after one warm-up; raw JSON in `results/run1..5.json`)

| Measure | Result |
|---|---|
| Launch to first frame (process creation → first painted frame) | **median 172.8 ms** (170.0–180.6) |
| Presents at idle (1.5 s, twice) | **0** |
| UI-thread wakeups at idle | **~160 WM_PAINT/s** (240 per 1.5 s = every vblank at 160 Hz); CPU under the 15.6 ms timer resolution |
| Typing 20 characters | 20 presents, **all full-window** (20.5 Mpx for a 1.02 Mpx window); CPU 4.3 ms total, max frame 0.38 ms |
| Scrolling, 60 steps | 60 presents, **all full-window** (61.4 Mpx), no copy; CPU 21 ms total, max frame 0.6–2.4 ms |
| Palette (open, type 4, close) | 6 presents, all full-window |
| Startup frames | 4 full presents (2 at gpui's default size before the off-screen move, see Traps) |
| Peak working set / private memory | 1073 MB (mostly the touched mapped file) / **70 MB private** |
| Clean release build (`-j 8`) | **166.5 s** |
| Clean dev build (`-j 8`) | **167.6 s** |
| Incremental dev rebuild after touching or editing `src/main.rs` | 1.2–1.3 s |
| Incremental release rebuild (thin LTO, 1 CGU) | 26.2 s |
| Release binary | 5.1 MB (dev binary 13.4 MB) |
| Crates in the graph | 420 unique (normal edges), 441 with build deps |
| Target dir size | 1.6 GB release, 1.7 GB dev |

Clean-build timings may include contention: renderer B was building in parallel at times.

## What the source shows (gpui 0.2.2, Windows)

- **The vsync thread invalidates every window on every vblank, forever** (`platform/windows/platform.rs:240–270`: `DwmFlush` then `RedrawWindow(RDW_INVALIDATE)`). That is why the UI thread gets a WM_PAINT per vblank even when nothing changes. gpui only redraws when an entity is dirty, so idle presents stay at 0. The wakeups are cheap locally; over RDP they cost CPU only, not bandwidth.
- **Every present is a full frame.** `directx_renderer.rs:205` calls `swap_chain.Present(0, 0)` with no dirty rects or scroll rect (no `Present1`), and every frame rebuilds and repaints the whole scene. Spec §6 needs damage-only presents and scroll-by-copy; gpui supports neither.
- **For one second after each input, an active window re-presents every vblank** (`window.rs:1036–1058`, "to prevent the display from underclocking the refresh rate"). Our test window is inactive, so the numbers above **do not include this**. With a focused window, each keystroke or scroll would add about 160 full-window presents (1 s at 160 Hz). Over RDP/VNC that is the worst case the owner ruled out.
- **There is no CPU renderer.** DirectX 11 only, so a GPU-less RDP session runs on WARP (software D3D).
- Off-display window bounds are moved onto the default display (`window.rs:1309`, `check_given_bounds` → `default_bounds()`).

## Captures

1280×800 client, scale 1.0, captured off-screen with `PrintWindow(PW_RENDERFULLCONTENT)`:
- `results/a-1-file.png`: first screen of the 1 GB file
- `results/a-2-scrolled.png`: after 180 lines of scrolling
- `results/a-3-palette.png`: the palette filtered by "save", with the typed line visible

## Traps found

- **Window placement:** gpui clamps a negative origin onto the primary display. Before this was fixed, the first two runs **showed the window on the owner's main monitor for about 6 s each** (unfocused, at 757,404). The fix: `show: false`, then `SetWindowPos` to (-6000, 100) and `ShowWindow(SW_SHOWNOACTIVATE)` from a foreground task (`park_offscreen`).
- `SetWindowPos` called inside the `App::run` closure sends a synchronous WM_SIZE while the App is borrowed. gpui's resize callback then fails silently: viewport 0, black window. Doing it from `cx.spawn` works.
- `window.window_handle()` is gpui's own inherent method; use `HasWindowHandle::window_handle(window)` for the HWND.
- First frame includes 2 frames at gpui's default window size, before the off-screen move takes effect.

## Not measured

- RDP and VNC sessions (needs a second machine or VM; RDP into this PC would take over the owner's console session).
- Frames while the window is active (the 1 s re-present loop above); quantified from source only.
- Cold disk cache.
