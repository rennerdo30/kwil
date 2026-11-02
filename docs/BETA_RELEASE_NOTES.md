# Kwil 0.1.0-beta.8

Draft notes for the next candidate. Artifacts and packaged native gates are not yet qualified.

An early beta for evaluation and feedback. This is not an RC or a claim that the
full [v1 specification](https://github.com/rennerdo30/kwil/blob/v0.1.0-beta.8/docs/SPEC.md)
is complete. Test with copies of important files.

## Downloads and installation

- **Windows:** `kwil-<version>-windows-x64-setup.exe` (or `-arm64-`) installs
  for your user without admin rights, with a Start menu entry and an
  uninstaller. The `.zip` is the portable alternative: keep `kwil.exe` and
  `kwil.com` together.
- **macOS / Linux:** extract `kwil-<version>-<platform>.tar.gz` and run `kwil`
  (`--tui` forces the terminal frontend).
- The bare `kwil-<target>` files are what the built-in updater downloads.

Every package contains `LICENSE.txt` and `THIRD-PARTY-NOTICES.txt`; the About
page shows the same notices. The builds are unsigned, so Windows SmartScreen and
macOS Gatekeeper warn on first start. `SHA256SUMS` detects damaged downloads; it
does not authenticate the publisher. There is no package manager distribution
or macOS notarization yet.

Beta builds now follow published betas and stable releases by default; stable
builds default to stable releases. An explicit update.channel setting always
wins. Set `channel = "stable"` in `[update]` to receive only stable releases;
update.check = false disables checks. Downloads still use SHA-256 verification,
without independent signature verification. beta.2 copies update to
this release by themselves; frozen beta.1 copies must be replaced manually. Stable download installers do not
select betas.

## Changes since beta.7

- **Settings take effect:** ten settings that were saved but not yet used now
  change Kwil's behaviour, and so does smooth scrolling. Values are checked
  against their allowed ranges. Settings that only apply to newly opened files
  say so on the Settings page.
- **File types:** after the first registration you can register more file
  types or remove only some of them. Kwil keeps its record of what it
  registered up to date, so Unregister still removes everything Kwil added.
- **Faster command palette:** finding a file in a folder of 100,000 files now
  takes 5 to 13 ms per query on the CI runners (was up to 21 ms on Windows).
- **Rendering:** a pending redraw is no longer lost while the GPU waits for a
  free frame (seen with the software WARP renderer).
- **Compare any size:** comparing large files (or a large file against its
  saved version) works beyond the old limit; alignment, folding of unchanged
  regions and word highlights are worked out on disk within bounded memory.
  Compare says "1 change" and "1 unchanged line" in GUI and TUI.
- **Format Document / Format Selection:** formatting works right after a
  search, and Format Selection keeps the whitespace and line break the
  selection ended with, so the lines around it stay as they were.
- **Hot exit fix:** after Kwil restores unsaved tabs, new edits are saved to
  the recovery journal again and "Recovery pending" clears within seconds;
  before, an idle restored window could keep them unconfirmed.
- **Faster start on macOS:** fonts are prepared while the window is created;
  time to file content on the CI runner fell from about 207 to 138 ms (warm
  file) and from 188 to 153 ms (1 GiB file).
- Homepage: https://kwil.renner.dev
- CI: startup time to first file content and drawing regressions are measured
  on every change to main; the command palette is checked end to end in GUI
  and TUI; several flaky tests were fixed.

## Changes in beta.7

- **Hot exit and session restore:** quit with unsaved changes or untitled tabs
  and they come back on the next start, with undo where the recovery journal
  reaches; open tabs and their order are restored.
- **Single instance:** opening a file while Kwil runs adds a tab to the running
  window. `--new-window` opens a separate window without a saved session; it
  asks Save / Don't Save / Cancel before discarding unsaved changes.
- **Compressed files:** gzip, zstd, bzip2 and xz files (detected by content)
  open as text, decompressed to disk rather than into memory, and save back in
  the same compression. "Save uncompressed as…" exports a plain copy; the status
  bar shows the format.
- **Hex editor:** edit bytes in the hex or text column, go to offset, find a
  byte sequence, copy as hex, undo/redo and save, in GUI and TUI. Editing a
  freshly opened file in hex mode works.
- **Screen readers:** the GUI exposes menus, tabs, palette, prompts, settings,
  status bar and the visible editor text with caret and selection (AccessKit).
- **TUI:** Markdown Text/Preview/Split and the log-file commands (follow,
  filters, levels, marks, time range, highlight rules) now work in the terminal.
- **UI polish:** button labels no longer clip g/y/p; "1 file"/"1 match"
  singulars; long folder paths are shortened with "…"; the compare view's
  collapsed row aligns with the text.
- YAML formatting keeps the existing formatter (`pretty_yaml` changed values in
  tests; see docs/FORMATTERS.md).
- CI: lint runs in parallel jobs; build time and size are tracked on main and
  weekly; the benchmark workflow works again and times the palette over
  100,000 files (informational).

## Changes in beta.6

- **Replace in files:** enter a replacement in Find in Files, preview the
  affected files with match counts, untick files, then apply. Open files change
  in their tab (undoable, unsaved); closed files are saved with their encoding
  and line endings kept; files changed on disk since the preview are skipped.
- **Save As uses the system dialog.** Saving an untitled file opens the native
  Save dialog in the last-used folder or Documents, never the working folder.
  The TUI shows the full target path before writing.
- **Compare:** long unchanged regions fold into one expandable row (context
  lines configurable, default 3); changed words are highlighted in GUI and TUI;
  F7 / Shift+F7 jump between changes.
- **Language mode:** "Change Language Mode" with Plain Text and Auto Detect;
  map extensions or globs (`*.conf`) to languages in settings. Syntax colours
  follow the active theme and recolour without re-parsing.
- **Keymap presets:** VS Code (default), Notepad++ or Vim, switchable in
  Settings or the palette, with your overrides on top. The palette shows each
  command's current shortcut.
- **Classic Mac (CR-only) line endings:** such files open as separate lines,
  keep CR when editing and saving, show "CR" in the status bar and convert
  between LF, CRLF and CR.
- **File types page:** shows the real state right after unregistering; Register
  selected / Unregister selected / Register all text types. Bulk registration
  never includes executables, scripts or system-managed types.
- **Memory on large files:** the line index is capped (64 MiB, enough for
  files well over 100 GB); compare and very large clipboard copies refuse with
  a clear message instead of exhausting memory.
- Reload after an external change no longer falls back to the old text on
  Windows, and quitting after "Overwrite" no longer stalls.
- Randomized tests for text editing, line counting and 40 encodings, plus a
  weekly fuzz run.

## Changes in beta.5

- **Saving a file another program changed** no longer ends in a dead end. Kwil
  asks: overwrite the file with your version, or reload it from disk. Reload
  also works when the recovery journal failed on the change. (Overwrite is
  available for files up to 64 MiB, which Kwil keeps its own copy of.)
- **Tab context menu** (right-click a tab): Close, Close Others, Close to the
  Right, Close Saved, Close All, Save, Reload from Disk, Compare with Saved,
  Copy Path, Show in File Explorer / Reveal in Finder / Open Containing Folder.
  Closing never discards unsaved changes without asking.

## Changes in beta.4

Every command in the command palette now runs; none answers "command not
implemented" any more (a contract test for GUI and TUI enforces it).

- **Check for Updates** in the palette and under Help, in GUI and TUI.
- **Themes:** select with live preview (Esc restores), import a theme file,
  follow the system light/dark mode.
- **Log files:** follow mode with pause/resume, text and regex filters, level
  toggles, line marks, go to a time range, highlight rules.
- **Compare:** compare with another file or with the saved version, side by
  side with line and word highlights, next/previous change, ignore whitespace,
  collapse unchanged regions.
- **Files:** change encoding (reopen or save with), convert LF/CRLF line
  endings, reload from disk, keep local changes after an external change, open
  a folder as the workspace root.
- **Format Document / Format Selection:** built-in formatters for JSON, XML,
  HTML, YAML, TOML, CSS and SQL; rustfmt, black and prettier when installed
  (configurable per language, see docs/FORMATTERS.md).
- Formatting and Replace All work right after opening a file.

## Changes in beta.3


- Windows setup (NSIS, per user) and zip; tar.gz archives for macOS and Linux.
- Third-party license notices ship with every package and on the About page;
  CI refuses dependencies under licenses that are not accepted.
- Uninstalling removes Kwil's "Open with" entries first
  (`kwil --unregister-file-types`).
- Updating an installed Windows copy also updates its version in Installed apps.
- Smaller macOS and Linux binaries (symbols stripped).

## Changes in beta.2

- Deadline-bounded native worker exit waiting.
- GUI notification and receiver ownership retained through shutdown.
- Stable/prerelease update settings and numeric prerelease ordering.
- Guarded save/quit, readiness, cleanup and IPC regression fixes.
- Smooth window resizing: intermediate sizes are coalesced and no stale frame
  shows after a drag (KW-58).
- Scrollbars, typing, caret, selection, window controls and the window/taskbar
  icon checked and corrected (KW-48).
- Saving on Windows no longer warns about owner/ACL preservation unless the
  file's permissions really differ afterwards.
- File-type registrations use normal paths that Explorer can launch; broken
  Kwil-owned registrations repair themselves at startup (KW-70).
- Several intermittent test failures fixed (KW-68, KW-69).

## Evaluation areas

- CR-only (classic Mac) files open as separate lines, retain CR on Enter and save,
  and convert through the same line-ending command as LF and CRLF.
- GUI and TUI text editing, tabs, undo/redo and saving.
- File search, command palette, configurable keymaps and hex editing.
- Shared UTF-8, UTF-16, UTF-32 and supported legacy encoding paths.
- Syntax highlighting and bounded Markdown preview.

This list identifies areas to evaluate, not blanket platform or performance
acceptance. Exact candidate checks and remaining release gates are recorded in
[BETA.md](https://github.com/rennerdo30/kwil/blob/v0.1.0-beta.8/docs/BETA.md).

## Known limitations

- Large originals use bounded positioned reads without requiring memory mapping.
  Real low-RAM machines and multi-GB files have not been measured.
- Complete metadata pre-growth admission is unfinished. Large or demanding
  editing workloads have not received bounded-memory acceptance.
- Complete regex/long-line search and folder
  replacement remain incomplete.
- The TUI supports text-only Markdown preview and log follow, text/regex,
  level/time/unknown filters, highlight rules, marks and result navigation.
  Log row copy uses OSC 52 and requires terminal clipboard support.
- Log follow mode re-indexes the file after each change; regex log filters skip
  lines over 1 MiB.
- Accessibility, remote rendering, platform integration, crash/recovery and
  performance budgets have not completed their full qualification.
- Markdown preview is limited to 256 KiB of logical source. ISO-2022-JP is
  explicitly unsupported.

Report the platform, exact beta version, steps to reproduce and expected versus
actual behavior at https://github.com/rennerdo30/kwil/issues. Attach only files
and logs you choose to share; remove private content first.
