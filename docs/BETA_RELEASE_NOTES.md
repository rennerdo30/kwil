# Kwil 0.1.0-beta.3

Draft notes for the next candidate. Artifacts and packaged native gates are not yet qualified.

An early beta for evaluation and feedback. This is not an RC or a claim that the
full [v1 specification](https://github.com/rennerdo30/kwil/blob/v0.1.0-beta.3/docs/SPEC.md)
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

## Changes since beta.2

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

- GUI and TUI text editing, tabs, undo/redo and saving.
- File search, command palette, configurable keymaps and hex editing.
- Shared UTF-8, UTF-16, UTF-32 and supported legacy encoding paths.
- Syntax highlighting and bounded Markdown preview.

This list identifies areas to evaluate, not blanket platform or performance
acceptance. Exact candidate checks and remaining release gates are recorded in
[BETA.md](https://github.com/rennerdo30/kwil/blob/v0.1.0-beta.3/docs/BETA.md).

## Known limitations

- CR-only (classic Mac) line endings are not converted yet

- Any-size opening and resident index memory limits are unfinished. Mapping
  refusal does not yet have the required positioned-read fallback.
- Complete metadata pre-growth admission is unfinished. Large or demanding
  editing workloads have not received bounded-memory acceptance.
- Complete regex/long-line search and folder
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
