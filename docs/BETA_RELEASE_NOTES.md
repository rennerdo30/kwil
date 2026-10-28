# Kwil 0.1.0-beta.2

Draft notes for the next candidate. Artifacts and packaged native gates are not yet qualified.

An early beta for evaluation and feedback. This is not an RC or a claim that the
full [v1 specification](https://github.com/rennerdo30/kwil/blob/v0.1.0-beta.2/docs/SPEC.md)
is complete. Test with copies of important files.

## Downloads and installation

The release contains raw executables for Windows, macOS and Linux, on x64 and
ARM64, plus `SHA256SUMS`. Windows needs both matching files: rename the `.exe`
asset to `kwil.exe` and the `.com` asset to `kwil.com`, in the same folder.
Start the GUI with `kwil.exe`; use `kwil.com --tui` for the terminal.
On macOS/Linux, make the matching executable runnable and use `--tui` to force
the terminal frontend.

These binaries are unsigned. Checksums detect a mismatch; they do not
authenticate the publisher. There is no qualified MSI, ZIP installer, package
manager distribution or macOS notarization in this beta.

Beta builds now follow published betas and stable releases by default; stable
builds default to stable releases. An explicit update.channel setting always
wins. Set `channel = "stable"` in `[update]` to receive only stable releases;
update.check = false disables checks. Downloads still use SHA-256 verification,
without independent signature verification. Managed packages with
kwil.no-update require explicit replacement. Frozen beta.1 copies must be
replaced manually to obtain this updater. Stable download installers do not
select betas.

## Changes since beta.1

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

- GUI and TUI text editing, tabs, undo/redo and saving.
- File search, command palette, configurable keymaps and hex editing.
- Shared UTF-8, UTF-16, UTF-32 and supported legacy encoding paths.
- Syntax highlighting and bounded Markdown preview.

This list identifies areas to evaluate, not blanket platform or performance
acceptance. Exact candidate checks and remaining release gates are recorded in
[BETA.md](https://github.com/rennerdo30/kwil/blob/v0.1.0-beta.2/docs/BETA.md).

## Known limitations

- Any-size opening and resident index memory limits are unfinished. Mapping
  refusal does not yet have the required positioned-read fallback.
- Complete metadata pre-growth admission is unfinished. Large or demanding
  editing workloads have not received bounded-memory acceptance.
- Encoding and line-ending commands, complete regex/long-line search and folder
  replacement remain incomplete.
- Compare panes/collapse, formatter execution, log following/filtering, custom
  themes and some GUI/TUI feature parity remain incomplete.
- Accessibility, remote rendering, platform integration, crash/recovery and
  performance budgets have not completed their full qualification.
- Markdown preview is limited to 256 KiB of logical source. ISO-2022-JP is
  explicitly unsupported.

Report the platform, exact beta version, steps to reproduce and expected versus
actual behavior at https://github.com/rennerdo30/kwil/issues. Attach only files
and logs you choose to share; remove private content first.
