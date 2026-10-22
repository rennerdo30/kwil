# Fast text editor — v1 specification (draft)

Status: draft, 2026-10-23; owner decisions in §12. Working name **Kwil** (owner preference 2026-10-23; see `NAMES.md` for the name check).
Crate versions were checked on crates.io on 2026-10-23.

## 1. Product

`kwil` is a plain, very fast text editor for the agent era. It is what Notepad++ is today, built with modern tools, running on every platform, in a GUI window and in a terminal.

**P0 is speed.** Every other goal gives way to it. A feature that cannot meet the speed budgets (§3) does not ship in v1.

### v1 scope

- Open, edit and save any text file of any size. This includes files with millions of lines and files of several GB.
- Syntax highlighting for nearly every common language.
- Markdown: a rendered preview next to the text.
- Search and replace in the file and across folders (regex, case, whole word).
- Compare two files, or a file against its saved version, side by side.
- Tabs, a command palette, a fuzzy file finder and keyboard shortcuts.
- The basics every editor has, nothing more: undo/redo, multiple cursors, column selection, go to line, encodings, line endings, auto-indent, comment toggle, word wrap, zoom, recent files, and restoring unsaved work after a crash or close.
- Windows, macOS and Linux. GUI and TUI in one binary, chosen automatically (§6).
- Owner additions (2026-10-23): hex view for any file, "Format document" for any file type, releases built by GitHub Actions, and auto-update from GitHub Releases (§7).

### Not in v1 (on purpose)

Kwil is a text editor, not an IDE and not an AI editor. Not planned: AI features, chat, agents, LSP/IDE features, git panels, debuggers, terminals inside the editor, remote/SSH editing and collaboration.

Maybe later, only if speed stays intact: plugins (v2, as WASM components), split views, folding, bookmarks, macros, inline markdown rendering and a settings UI.

## 2. Principles

1. **Show the file first, do the rest after.** Paint the first screen from the raw file bytes. Line counts, highlighting, encoding checks and indexes then fill in behind it.
2. **Work in proportion to the viewport, not to the file.** Layout, shaping, highlighting and rendering only touch visible lines plus a small margin.
3. **Nothing blocks the UI thread.** Disk, search, parsing, diffing and saving run on worker threads and report progress. The UI thread only handles input, layout and frame submission.
4. **Measured, not assumed.** Every budget in §3 has a benchmark in CI. A regression fails the build.
5. **Small dependency graph.** Each dependency costs compile time and startup time and has to earn its place (§9).
6. **Event-driven, damage-only drawing.** No frame loop. Draw only what changed, and present only that region. This has to hold over RDP and VNC (§6).
7. **Memory is bounded by the viewport, never by the file** (owner, 2026-10-23, KW-40). Machines may have less RAM than the file is large. Files are mapped, not loaded; indexes are sparse; search, save, decode and diff stream in chunks; edit and undo buffers have caps and spill to disk. No code path builds a file-sized buffer in memory.

## 3. Speed budgets (proposed; the owner sets the final numbers)

Reference machines: the owner's Windows PC (Ryzen 9 9950X3D2, NVMe) and a mid-range laptop.

| Measure | Target | Hard limit |
|---|---|---|
| Launch to first frame with file content (any file size, warm disk cache) | ≤ 40 ms | 100 ms |
| Launch to first frame, empty window | ≤ 25 ms | 60 ms |
| Open a 1 GB / 20M-line file: first screen | ≤ 40 ms | 100 ms |
| Line index for that file, in the background | ≥ 2 GB/s | 1 GB/s |
| Jump to line 15,000,000 once the index is ready | ≤ 1 frame | 16 ms |
| Keystroke to pixels | ≤ 1 frame at 120 Hz (8.3 ms) | 16 ms |
| Scrolling, any file size | display refresh rate, 0 dropped frames | — |
| Literal search, 1 GB file in memory | ≤ 300 ms | 1 s |
| Memory for a clean 1 GB file | ≤ 64 MB plus the OS page cache | 256 MB |
| Command palette / file finder, 100k files | results on the first frame after a key | 16 ms |
| Diff of two 100k-line files | ≤ 100 ms | 500 ms |
| Second launch while running (single instance) | file shown ≤ 20 ms | 50 ms |
| Idle (nothing changing) | 0 frames, 0 % CPU | — |
| Typing one character | redraws one line + status field, no full-window present | — |
| Scrolling over RDP/VNC (remote mode) | one copy + newly exposed lines per step; no lag after the wheel stops | — |

## 4. Architecture

A Cargo workspace. The core crates have no UI dependencies, so changing the editor logic never recompiles the GPU stack, and the reverse.

```
crates/
  core-text      buffer (piece tree over mmap/pread), line index, cursors, undo, encoding
  core-syntax    tree-sitter integration, highlight queries, fallback lexer
  core-search    in-buffer search, find-in-files, replace transactions
  core-diff      line and word diff, alignment model
  core-markdown  markdown parse into a render model (no drawing)
  core-workspace files, sessions, hot exit, file watching, single-instance IPC
  core-commands  command registry, keymaps, settings (shared by both frontends)
  view-model     viewport, layout model, decorations; frontend-independent
  ui-gpu         GPU frontend (winit + renderer, see §6)
  ui-tui         terminal frontend (ratatui + crossterm)
  app            binary: argument parsing, picks a frontend, wires threads
  update         auto-update from GitHub Releases (owner wish KW-5; added 2026-10-23)
```

Threading: the UI thread, plus a small fixed pool of worker threads (`std::thread` + channels; no async runtime in v1). Workers own disk I/O, indexing, parsing, search and save. The UI thread reads immutable snapshots: the buffer is persistent (copy-on-write nodes), so a worker can search or highlight a snapshot while the user keeps typing.

## 5. Text engine (the core of the speed promise)

### Opening

1. `open()` maps the file (`memmap2`) or, if mapping is refused (network drives, some special files), uses positioned reads.
2. It reads the first and last few hundred KB at once: encoding sniff (BOM, UTF-16 detection, `chardetng` for legacy encodings), line-ending style, binary check.
3. It paints the first screen from those bytes before any full pass over the file.
4. A background pass builds the line index. It scans the bytes with `memchr` in parallel chunks, recording line starts sparsely (one checkpoint per N lines) so the index for a 20M-line file stays a few MB. The scrollbar and line count show "≈" until the pass ends.
5. UTF-8 is checked lazily per chunk (`simdutf8`). Invalid bytes are shown as escape glyphs and kept byte-exact on save.

Legacy encodings (Shift-JIS, Windows-1252, UTF-16, …) are decoded to UTF-8 with `encoding_rs` per viewport and per search chunk, using a sparse decode index built in the background, and encoded back on save. There is never a full decoded copy in RAM (principle 7).

**mmap safety:** if another program truncates a mapped file, reading it can crash the process (SIGBUS on Unix). Rule: files under 64 MB (configurable) are read fully into memory, which takes a few ms. Larger files are mapped, watched with `notify`, and re-checked by size and mtime before each region read. If they changed, the editor shows "file changed on disk" instead of reading stale pages.

### Editing

A **piece tree**, the same idea as VS Code's: the document is a balanced tree of pieces that point into the original read-only file or into an append-only "added text" buffer. Each node stores byte length, line-break count and UTF-16/char counts for its subtree, so offset ↔ line ↔ column lookups are O(log n). Opening a file never copies it, and an edit costs O(log n).

The tree's nodes are immutable and shared between versions. Snapshots are free, which gives undo and background readers without locks.

Library options: `ropey 1.6.1` and `crop 0.4.3` are good ropes, but both need the whole text in memory as validated UTF-8, which breaks the "1 GB opens instantly" goal. `qem` (a file-backed engine for huge files with a sparse on-disk line index; the author's own figures are 3 GB opening in ~5 ms) is the closest existing match and should be benchmarked in week 1. Decided (M0, `docs/decisions/0002-text-engine.md`): our own engine. `core-text` opens the 1 GB fixture with its first screen in 0.09 ms (qem: 60 ms), indexes at ~10 GB/s (qem: 2 GB/s) and uses 4 MB private memory (qem: 85 MB). The piece tree is built on it in M2; `qem` stays the baseline.

### Long lines

Minified files can have one 100 MB line. Layout is chunked per line: only the visible column range is shaped. Word wrap is computed lazily for visible lines and cached. Horizontal scrolling is virtual, like vertical scrolling.

### Undo

Transactions group edits (a typing burst, one replace-all, one multi-cursor action). Undo history is linear in v1, can optionally be saved to disk with the session, and has a memory cap that drops the oldest history first.

### Saving

Write to a temp file in the same folder, `fsync`, then an atomic rename. File permissions, owner (Unix), ACLs and attributes (Windows) and the line-ending and encoding style are kept. Very large files are written streaming from the piece tree. Optional backup copy. Read-only and elevated-only paths show a clear prompt instead of failing silently.

## 6. Rendering and UI

### Decision: path B, own thin renderer (M0, `docs/decisions/0001-renderer.md`)

Decided 2026-10-23 from measured numbers. B reaches the first frame in ~37 ms (A: 173 ms), uses 0 idle wakeups (A: ~160/s), presents one line + status per key (A: the full window) and scrolls by copy (A: full repaint). It builds clean in 7.4 s (A: 166 s). gpui has no CPU renderer and no dirty or scroll rects. wgpu device creation takes ≥ 175 ms even on DX12 only, so the CPU first frame is required, and wgpu uses DX12 only on Windows (Vulkan adds ~200 ms and ~170 MB).

The spike compared these two paths

| | A. `gpui` (Zed's UI framework) | B. Own thin renderer |
|---|---|---|
| Stack | `gpui` 0.2.2 on crates.io, or a distribution such as `gpui-box` | `winit 0.30` + `wgpu 30` + `swash` glyph rasterizing + own glyph atlas; `taffy` for panel layout |
| Pros | Proven for a top-speed code editor; a polished UI comes quickly | Full control of startup, frame timing and look; a small graph; the same model drives the TUI |
| Cons | Pre-1.0 with breaking changes; heavy dependency graph (compile time); macOS was its strongest platform; several unofficial distributions; built around redrawing the whole window each frame, which is the wrong model for RDP/VNC (check in the spike) | More work: text input/IME, accessibility and widgets are ours to build |

The spike built the same three screens with both: a 1 GB file, scrolling, and the command palette. Real RDP and VNC sessions were not measured, because an RDP loopback would take over the owner's console session; that moved to KW-30.

### First frame in milliseconds

Creating a GPU device takes 50–200 ms on some systems. Path B therefore paints the first frame on the CPU (`softbuffer`, a small blit of already rasterized glyphs). Locally, the GPU renderer starts in parallel and takes over on the next frame, and the user never sees the switch. The CPU renderer is a complete backend: in remote mode it stays in use.

Glyphs: monospace fast path. ASCII runs skip full shaping and come straight from the atlas. Only non-ASCII runs, emoji and ligatures go through shaping (`swash` / `parley 0.12`; `cosmic-text 0.19` is the fallback). Fonts load lazily, starting with the configured font. Font fallback is found on demand and cached.

### Event-driven rendering (no frame loop)

Kwil draws only when something changed. There is no fixed FPS loop and no idle frames. This matters most over RDP and VNC, but it also saves battery and CPU everywhere.

- **Redraw triggers:** input, a worker result (highlights, search hits, index progress), a window event (resize, expose, DPI) or a timer the UI asked for. Nothing else. At idle, the frame and CPU counters are zero.
- **Damage tracking:** the view model records which rectangles changed (per line, gutter cell, cursor, status field). The renderer redraws only those and presents only those:
  - Windows: DXGI `Present1` with dirty rectangles and a scroll rectangle (flip-sequential swap chain). Microsoft documents that this metadata is passed on to terminal servers to cut traffic.
  - Linux: Wayland `damage_buffer` and EGL/Vulkan present-with-damage where the driver honours it (some only claim to; measure).
  - CPU renderer: `softbuffer` present with damage.
- **Scrolling is a copy, not a repaint:** the existing pixels move (scroll rect / blit) and only the newly exposed lines are drawn. RDP and VNC can send a copy command (VNC *CopyRect*) instead of pixels.
- **Batched progress:** background progress (line counting, search counts) updates the screen at most a few times per second and only damages the small status area.
- **Cursor:** blinking is a small damage rectangle, stops after 10 s idle, and is off in remote mode.

### Remote desktop mode (RDP, VNC)

A remote session often has no real GPU. RDP renders Direct3D in software (WARP) unless GPU use for RDP is enabled by policy. A GPU-style renderer then burns CPU, and every full-window present costs bandwidth.

- **Detection:** Windows `GetSystemMetrics(SM_REMOTESESSION)`, plus live switching on session-change events (`WM_WTSSESSION_CHANGE`), for example when the same session is moved from local to RDP. Linux: xrdp/VNC session markers where present. macOS: no reliable signal, so it's a setting. Override: `render.remote = "auto" | "on" | "off"`.
- **In remote mode:**
  - The CPU renderer with damage is used (it is a full backend, not just the first-frame helper).
  - Smooth scrolling and all animations are off.
  - Cursor blink is off.
  - No translucency or blur.
  - Solid backgrounds, so codecs compress well.
- **Budgets in remote mode:**
  - Idle: 0 presents.
  - Typing one character: damage of one line plus the status field.
  - Scrolling one line: one copy plus one line of new pixels.
  - Measured with a scripted RDP session and a TigerVNC session that count damaged pixels and bytes on the wire per action.

### Choosing GUI or TUI

One binary. `kwil` starts the GUI when a desktop is available and the TUI when it is not:
- Linux: no `WAYLAND_DISPLAY`/`DISPLAY`, or no connection to the display.
- Any platform: an SSH session without forwarding, or a non-interactive service session.
- If window or renderer creation fails, Kwil also falls back to the TUI when it is attached to a terminal.

Flags: `--tui` forces the terminal UI and `--gui` forces a window. The check costs well under 1 ms and runs before anything else.

### Terminal UI

`ratatui 0.30` + `crossterm 0.29`. It uses the same view model, commands, keymaps, search and diff. True colour, mouse and bracketed paste. Only changed cells are written to the terminal, never a full clear, so it also stays fast over SSH. Markdown preview is text only.

### Look and feel (state of the art, built for speed)

- **Calm, content-first chrome.** No toolbar by default: a thin title strip with tabs and a status line that only shows something when there is something to say. Everything else is reached through the command palette.
- **Command palette** (`Ctrl/Cmd+Shift+P`) with one input: `>` commands, `:` go to line, `#` open tabs, no prefix for files. Fuzzy matching (`nucleo`, from Helix). Results appear on the first frame after each key; there is no spinner below 16 ms.
- **Signal strip** instead of a classic minimap: a slim column beside the scrollbar showing search hits, changes since save and diff hunks across the whole file, even for 20M lines (drawn from the sparse index).
- **Motion that never waits:** optional smooth scrolling, driven only while the scroll is moving (no idle frames). It is off in remote mode. No other animations.
- **Markdown preview:** side by side, one key away.
- **Diff view:** aligned side by side, word-level highlights, collapsed unchanged regions and change navigation (`F7` / `Shift+F7`).
- **Themes** (owner, 2026-10-23, KW-37): theme files in TOML (UI colours plus syntax scopes), with a built-in dark and light theme designed for long sessions, a live-reloaded user theme folder, hover preview in the palette, optional OS dark/light follow, and import of VS Code and TextMate colour themes. The active theme is cached so theme parsing never runs on the startup path.
- **Window controls** (owner, 2026-10-23, KW-39): native on every platform. Windows uses the system caption buttons with snap layouts and a theme-matched dark/Mica title bar; macOS uses native traffic lights and full-screen tiling; Linux uses server-side decorations, or libdecor on GNOME/Wayland. Any drawn title-bar content keeps native hit-testing.
- **Logo** (owner, 2026-10-23, KW-38): an original SVG logo, with the app icon set (`.ico`, `.icns`, PNG sizes) generated from it.
- **Typography:** good default font size, line height and ligature support; crisp text at fractional DPI scales on Windows.

## 7. Feature specification

### Files
New, open, save, save as, save all, close, reopen closed tab, recent files, drag and drop, open from the command line (`kwil file:line:col`), single instance (a second launch passes files to the running one and exits), restore of unsaved buffers (journaled to disk every few seconds), file-changed-on-disk prompt (reload / compare / keep), and encoding and line-ending switch per file.

### Editing
Undo/redo, multiple cursors (click, Ctrl+D next match), column selection, line move/duplicate/delete, indent/outdent, comment toggle, go to line, bracket match, word wrap, whitespace display, zoom and trim trailing whitespace on save (optional).

### Search
- **In file:** incremental, as you type. Regex (`regex 1.13`), case, whole word, in-selection. Hits are highlighted across the whole file in the signal strip, with live counts ("12 of 3,481"; "≈" while counting in the background).
- **Replace:** with regex capture groups; replace-all is one undo step.
- **Find in files:** ripgrep's engine (`grep-searcher 0.1.17`, `grep-regex`, `ignore 0.4.33` for .gitignore-aware walking). Results stream in. Include/exclude globs.

### Compare
Two files, or a buffer vs. its saved version. Line diff with `imara-diff 0.2` (histogram/Myers; used by gitoxide and Helix), word-level refinement with `similar 3.2`, and an option to ignore whitespace. Large files are diffed in the background.

### Hex view (owner, 2026-10-23, KW-8)
"Open as hex" and "Toggle hex view" for any file: an offset column, 16 bytes per row in hex, and an ASCII column. It uses the same mapped file and viewport model as text, so multi-GB binaries open instantly (row = offset / 16, no line index needed). Files detected as binary on open offer hex view. The viewer comes in M1 and byte editing in M2. It works in the GUI and the TUI.

### Format document (owner, 2026-10-23, KW-7)
"Format document" and "Format selection" for any file type. Built-in in-process formatters cover JSON, XML/HTML, YAML, TOML, CSS and SQL. Every other type gets the generic tidy steps (re-indent, trim trailing whitespace, normalize line endings), and an external formatter can be configured per type, run on a worker with a timeout. Formatting runs on a worker and applies as one undo step. A file-type picker lets any file be treated as any type.

### Log watcher mode (owner, 2026-10-23, KW-41)
"Like glogg, only better." Follow mode (`tail -f`) with auto-scroll at the bottom and a pause when the user scrolls up; clean handling of rotation and truncation. A filter pane shows only lines matching one or more regexes (AND/OR/NOT) with live counts; clicking a line jumps to it. Several coloured highlight rules and automatic level detection for common log formats; line marks. Faster than glogg on multi-GB logs: the line index extends on append instead of rescanning, filters stream from workers, hits show in the signal strip, and lines with timestamps support a time-range jump. It works in the TUI too (`kwil --follow app.log`). Filter results are kept as compact line-number runs, never as copied text (principle 7).

### Markdown
Parsed with `pulldown-cmark 0.13` (fast; CommonMark plus tables, task lists, strikethrough). Rendered natively by our renderer, without a webview (a webview would cost startup time and memory). Code blocks are highlighted with the same syntax engine. Images are decoded lazily on a worker.

### Syntax highlighting
- `tree-sitter 0.27` with `tree-sitter-highlight`. Grammars come from `arborium` (a tree-sitter distribution with ~70–96 permissively licensed grammars, with a feature flag per language).
- Incremental parse on a worker after each edit. The UI keeps the last highlights until new ones arrive, so typing never waits on the parser.
- Only visible ranges are queried. For files over a size limit (e.g. 20 MB), full-file parsing is skipped and a fast lexical highlighter is used for the visible window (keywords, strings, comments, numbers per language).
- v1 bundles ~30 common languages. The rest load as prebuilt grammar libraries on first use, which also keeps clean builds fast.

### Settings and keymaps
One TOML settings file that reloads live; no settings UI in v1. Every action is a command with an id, so it can be bound and found in the palette.

Keymaps (owner, 2026-10-23, KW-2): the keymap is configurable and ships with three presets, chosen by one setting (`keymap = "vscode" | "notepad++" | "vim"`). Any binding can be overridden in the same file.
- **VS Code** (default): the common VS Code shortcuts (Ctrl+D selects the next match).
- **Notepad++:** the Notepad++ shortcuts (Ctrl+D duplicates the line, Ctrl+G goes to a line).
- **Vim:** modal editing with normal, insert and visual modes. It covers the common motions, operators with counts, text objects, registers, `.` repeat, `/` search and the `:w`, `:q` and `:s` commands. Full Vim compatibility (vimscript, plugins) is not a goal.

### Releases and updates (owner, 2026-10-23, KW-4, KW-5)
- Releases are built by a tag-triggered GitHub Actions workflow on self-hosted runners. It produces signed artifacts for every platform as a **draft** GitHub Release; only the owner publishes.
- Auto-update checks GitHub Releases on a worker at most once a day, never on the startup path. Updates are verified (signature plus checksum) and applied on the next start or on request. It can be turned off (`update.check = false`) and has stable and prerelease channels. Package-manager installs leave updates to their manager.

### Platform
- Windows 10/11 (x64, ARM64): native title-bar integration, high DPI, IME, a portable mode (settings next to the exe), and an "Open with kwil" shell entry.
- macOS 13+ (Apple silicon, Intel): native menu bar, trackpad scrolling, IME.
- Linux (Wayland and X11).
- Accessibility: screen-reader support through `accesskit` (GUI) for v1 basics (text, cursor, selection announcements).

## 8. Reliability

- No data loss: hot-exit journal, atomic saves, a crash handler that flushes the journal and keeps a recovery copy of unsaved buffers.
- Fuzz tests for the piece tree, encodings and the line index (`cargo-fuzz`). Property tests check that edits and undo round-trip byte for byte.
- A save must reproduce the original bytes exactly for untouched regions (encoding, BOM, mixed line endings, invalid UTF-8).

## 9. Build speed and disk use

Goal: a fast inner loop, and Rust build output that doesn't fill the disk.

**Proposed budgets** on the owner PC: clean debug build ≤ 60 s; incremental rebuild after a `core-text` edit ≤ 3 s; after a `ui-gpu` edit ≤ 5 s. Tracked with `cargo build --timings` in CI.

**Workspace layout:** many small crates. The frontends depend on core crates, never the other way round. Heavy dependencies (GPU, tree-sitter grammars) are isolated in leaf crates.

**Dependency rules:** no async runtime, few proc macros (`serde` derive only in the settings crate), no `syn`-heavy helper crates in core. Each new dependency notes its compile cost in the PR.

**Profiles** (`Cargo.toml`):

```toml
[profile.dev]
debug = "line-tables-only"   # small debug info; full debug only when needed
[profile.dev.package."*"]
opt-level = 2                # dependencies are fast at run time and compile once
debug = false
[profile.release]
lto = "thin"
codegen-units = 1
panic = "abort"
[profile.profiling]
inherits = "release"
debug = "line-tables-only"
```

**Linker:** Linux uses `rust-lld` by default since Rust 1.90 (the Rust blog reports up to 7× faster linking on incremental rebuilds). Windows: enable `rust-lld` per project in `.cargo/config.toml` (`[target.x86_64-pc-windows-msvc] linker = "rust-lld.exe"`), because it is not the Windows default. macOS: the default `ld-prime` is already fast. `wild` is Linux-only as of 0.9 and optional there.

**Cranelift backend:** nightly-only preview. It is offered as an opt-in for fast debug builds (`cargo +nightly` with `-Zcodegen-backend`), never required.

**Caches and disk use:**
- One build folder outside the repo, set in the project's `.cargo/config.toml` (`[build] target-dir = "E:/Development/_cache/cargo-target/kwil"`). No environment variable or PATH changes.
- Cargo's automatic global-cache cleanup (stable since Rust 1.88) keeps `~/.cargo/registry` in check. For the build folder, a `just clean-old` task runs `cargo sweep --time 14` (removes build files unused for 14 days), and CI uses `sccache` for clean builds.
- `debug = "line-tables-only"` and `opt-level`-ed dependencies without debug info keep the build folder small (often less than half the size of default debug info).
- Watch the "build-dir layout v2" Cargo work (in testing since March 2026). It separates build cache from final artifacts, which makes cleanup simpler.

**Toolchain:** pinned in `rust-toolchain.toml` to the current stable at project start (check on the Rust blog; reported 1.97 shipped on 2026-07-09).

## 10. Testing and benchmarks

- Benchmarks (`divan`, or `criterion`) for every budget in §3, with fixture files: 1 GB of logs, a 20M-line CSV, a 100 MB one-line JSON, a mixed-encoding sample and a 100k-file tree.
- Startup tracing: the binary logs `t_launch → t_first_frame → t_index_done` with `--trace-startup`. CI fails on a regression over 10 %.
- A frame-time harness scripts scrolling and typing and checks the 99th percentile.
- Self-hosted CI runners only (no paid GitHub minutes). Windows, macOS and Linux runners. Full suites run on CI, not on the owner PC.

## 11. Milestones

| Milestone | Content |
|---|---|
| M0 Spike, KW-1 (done 2026-10-23: renderer B, own text engine) | Renderer A vs. B, text engine vs. `qem` baseline, startup measured on all three platforms; decision record |
| M1 Viewer | Instant open of any file, damage-only rendering and scroll copy, remote-mode detection, GUI/TUI auto-select, TUI, go to line, encodings, hex viewer |
| M2 Editor | Piece tree editing, cursors, undo, save, tabs, session restore |
| M3 Find & palette | Search/replace, find in files, command palette, file finder, keymaps with VS Code / Notepad++ / Vim presets, settings, log watcher mode |
| M4 Language | Tree-sitter highlighting, markdown preview, format document |
| M5 Compare | Diff views |
| M6 Polish & ship | Themes, remote-mode measurements on RDP and VNC, installers (MSI/winget, dmg/Homebrew, AppImage/Flatpak), GitHub Actions releases, auto-update, platform integration and accessibility |

Work packages are tracked on Pair Desk, project `kwil`, grouped by milestone: one item per feature area, with its spec section and done criteria. M0 is KW-1; the spec work packages are KW-9 to KW-36; owner wishes are KW-3 (keymaps), KW-4/KW-6 (releases), KW-5 (auto-update), KW-7 (format), KW-8 (hex view), KW-37 (themes), KW-38 (logo), KW-39 (native window controls), KW-40 (less RAM than file size) and KW-41 (log watcher).

## 12. Owner decisions and open questions

Decided (owner, 2026-10-23, KW-2):
- **License: MIT.** GPL grammars and crates are therefore out; only permissively licensed dependencies ship.
- **Speed budgets:** left to the project ("i trust you"). The §3 values stand until the M0 numbers justify stricter ones.
- **Keymap:** VS Code-like by default, configurable, with VS Code, Notepad++ and Vim presets (§7).

Open:
1. Name: Kwil is the working name; a web3 database ("Kwil DB") uses the same name (see `NAMES.md`). A trademark search is needed before going public.
2. Plugin direction for v2: WASM components (sandboxed, any language) is the proposal.

## Sources

- Crate versions: crates.io API, 2026-10-23.
- [Rust: LLD default on Linux in 1.90](https://blog.rust-lang.org/2025/09/01/rust-lld-on-1.90.0-stable)
- [Rust blog (build-dir layout v2 call for testing)](https://blog.rust-lang.org/)
- [Rust 1.97 release report](https://www.linuxcompatible.org/story/rust-1970-drops-legacy-symbol-mangling-and-stabilizes-cargo-warning-controls/) (secondary source)
- [Cranelift backend readme](https://rust.googlesource.com/rust/+show/HEAD/compiler/rustc_codegen_cranelift/Readme.md)
- [Wild linker 0.9](https://phoronix.com/news/Wild-Linker-0.9), [Wild in the Rust Innovation Lab](https://rustfoundation.org/?p=105369)
- [Qem: a Rust text engine for huge files](https://users.rust-lang.org/t/qem-a-backend-first-rust-text-engine-for-huge-files/139281)
- [gpui-box](https://docs.rs/crate/gpui-box/0.1.1), [gpui-pre](https://docs.rs/crate/gpui-pre/0.3.3)
- [Introducing arborium](https://fasterthanli.me/articles/introducing-arborium)
- [DXGI flip model, dirty rectangles, scrolled areas](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-1-2-presentation-improvements), [IDXGISwapChain1::Present1](https://learn.microsoft.com/en-us/windows/desktop/api/DXGI1_2/nf-dxgi1_2-idxgiswapchain1-present1)
- [Firefox: partial invalidation on Windows](https://bugzilla.mozilla.org/show_bug.cgi?id=1575159), [Mesa: swrast ignores swap-with-damage](https://gitlab.freedesktop.org/mesa/mesa/-/issues/10537)
- [Why RDP feels slow: input, rendering, network](https://comcomponent.com/en/blog/windows-rdp-slow-fast-network/), [Smooth scrolling slow over RDP](https://www.syncfusion.com/forums/95597/slow-scrolling-with-a-rdp-connection)
- [Rust GUI comparison 2026](https://www.pistack.xyz/posts/2026-08-25-rust-gui-frameworks-egui-iced-slint-comparison/), [Are we GUI yet?](https://areweguiyet.com/)
