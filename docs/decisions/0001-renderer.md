# 0001: GUI renderer: own thin renderer (B), not gpui (A)

Status: accepted by the owner 2026-10-23 (M0 spike, Pair Desk KW-1).

## Decision

Kwil's GUI uses **path B**: `winit` for windows and input, `softbuffer` as the CPU renderer for the first frame and for remote mode, our own glyph cache and damage tracking, and `wgpu` (DX12 only on Windows) as a GPU path that starts on a worker and takes over locally after the first frame. `gpui` (path A) is rejected.

## Why

The numbers decide, and they all point the same way:

| Measure (1 GB / 20.35M-line fixture, owner PC) | A: gpui 0.2.2 | B: winit 0.30 + softbuffer 0.4 |
|---|---|---|
| Launch to first frame (median of 5) | 172.8 ms | **~37 ms** (46.7 ms before turning OLE drag-and-drop off at creation) |
| Spec target / hard limit | 40 / 100 ms: **fails the hard limit** | **meets the target** |
| Presents at idle | 0 | 0 |
| UI-thread wakeups at idle | ~160/s (WM_PAINT every vblank, forever) | **0** |
| Typing, per key | 1 full-window present (1.02 Mpx) | **1 present of 51,200 px** (one line + status, 5 %) |
| Scrolling, per step | 1 full-window present, no copy | **1 present: screen copy + 3 new rows (102,400 px)** |
| After any input, focused window | ~160 extra full presents (1 s at refresh rate, by design in gpui) | none |
| CPU renderer for GPU-less RDP | none (DX11 only, so WARP) | **yes (softbuffer, ≤ 0.5 ms per frame)** |
| Present API | `Present(0, 0)`, no dirty or scroll rects | GDI `BitBlt` per damage rect; screen-to-screen copy for scrolling |
| Private memory | 70 MB | **11 MB** (CPU path); +~1 MB with DX12 device |
| Clean release build, `-j 8` | 166.5 s | **7.4 s** (CPU only), 44.3 s with wgpu |
| Clean dev build, `-j 8` | 167.6 s | **4.9 s** (CPU only), 42.1 s with wgpu |
| Incremental dev rebuild | 1.2–1.3 s | **0.5 s** (0.7 s with wgpu) |
| Crates in the graph | 420 | **29** (95 with wgpu) |
| Release binary | 5.1 MB | **0.64 MB** (4.4 MB with wgpu) |
| Window placement control | moves off-display windows on-screen | full control (hidden, cloaked, no activation) |
| Native window controls (KW-39) | Zed draws its own title-bar controls | OS decorations by default |

Spec §6 requires event-driven drawing with no idle frames, damage-only presents, scrolling by copy and a CPU renderer for remote sessions (owner, 2026-10-23). gpui provides none of these, and its first frame is 4.7× over the target. Fixing that would mean forking gpui's platform and renderer layers, which is more work than path B.

### What B costs us

The spec already named these: text input and IME, accessibility (`accesskit`), widgets (tabs, palette, panels) and layout are ours to build. B's spike covers drawing, scrolling, the palette overlay and typing. IME, accessibility and Unicode shaping beyond the ASCII fast path are not in the spike.

## Supporting measurements

### GPU start-up cost (wgpu 30.0.1, B with `--features gpu-init`)

| wgpu setup | Device ready | Private memory |
|---|---|---|
| All backends (Vulkan + DX12), RX 9070 XT | ~385–460 ms | ~185 MB |
| DX12 only, RX 9070 XT | **~185 ms** | ~12 MB |
| DX12 WARP (the GPU-less RDP case) | ~175 ms | ~12 MB |

Consequences:
- The first frame must come from the CPU; even the best GPU path is 4–5× over the budget.
- On Windows, wgpu uses DX12 only. Initialising Vulkan as well costs +200 ms and +170 MB.
- Over RDP without GPU policy, WARP rasterises every frame on the CPU anyway. The damage-only CPU renderer is the correct remote path.

### Startup breakdown (B, `KWIL_TRACE_STARTUP=1`)

| Step | ms from process creation |
|---|---|
| `main` (an empty Rust exe reaches `main` at ~8.5 ms) | ~13 |
| event loop built | ~17 |
| window created (with drag-and-drop off; ~20 ms with it on) | ~27 |
| cloaked and shown, font ready | ~31–35 |
| first frame presented | ~37 |

Remaining wins for M1: register drag-and-drop after the first frame, delay-load shell32/ole32 (~4.5 ms of loader time over an empty exe), and possibly a raw Win32 window instead of winit's (~10 ms creation).

### Correctness evidence

- B's copy scrolling is **pixel-identical** to a full redraw: a window-DC capture after 60 copy-scroll steps equals the capture after a forced full repaint (`cmp` identical).
- Both renderers' captures show the three screens: file view, scrolled view, palette filtered by "save" (A: `spike/renderer-a/results/*.png`; B: regenerate with `--capture`).
- B's present counts were identical across all 5 measured runs.

## Not measured (open)

- **Real RDP and VNC sessions.** An RDP loopback into the owner's PC would take over the console session. This moves to KW-30 (VM or second machine): bytes on the wire and damage per action.
- macOS and Linux. The spike ran on Windows only; the spec's M0 asked for all three. B's design (winit + softbuffer) is cross-platform, but Wayland damage and Linux startup need measuring in M1 (KW-11).
- Cold disk cache (needs admin tools to flush).
- A focused window (A's 1 s re-present loop is known from gpui's source; B has no such loop).

## Reproduce

```
cargo run --release --manifest-path spike/textbench/Cargo.toml -- gen fixtures/log-1g.log 1024
cargo build --release -j 8 --manifest-path spike/renderer-b/Cargo.toml [--features gpu-init]
renderer-b fixtures/log-1g.log --script [--capture <dir>]     # one JSON line of stats
KWIL_TRACE_STARTUP=1 renderer-b ...                           # startup marks on stderr
KWIL_DX12=1 / KWIL_WARP=1                                     # wgpu backend selection (gpu-init)
cargo build --release -j 8 --manifest-path spike/renderer-a/Cargo.toml
renderer-a fixtures/log-1g.log --script [--capture <dir>]
```

Agent runs keep windows off the owner's desktop: B cloaks its window with DWM and shows it without activation; A parks its window off-screen from a spawned task (see `spike/renderer-a/RESULTS.md`, Traps). Compile timings: `spike/compile_times.sh <crate dir> <fresh target dir> [--features ...]` (a fresh `CARGO_TARGET_DIR`, `-j 8`, clean release, clean dev, then a touch of `src/main.rs`).
