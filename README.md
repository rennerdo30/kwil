<p align="center"><img src="assets/logo/kwil-wordmark.svg" alt="Kwil" width="420"></p>

A plain text editor for the desktop and terminal, written in Rust. Speed and a
quiet, responsive interface come first.

**Early beta:** `0.1.0-beta.1` is prepared as a draft. The full v1 specification
is still in progress. See [beta status](docs/BETA.md) and
[release notes](docs/BETA_RELEASE_NOTES.md) before evaluating it with copies of
important files.

## Features

- **Desktop and terminal interfaces:** use the native GUI or the keyboard-driven
  TUI from the same application. Force either with `--gui` or `--tui`.
- **Everyday file editing:** tabs, new/open/save/save as/save all, recent files,
  reopening closed tabs, undo/redo and recovery journals.
- **Editing tools:** multiple cursors, rectangular selection, indentation,
  comment toggling, line operations, go to line, whitespace display and word wrap.
- **Search and navigation:** in-file find/replace, case and whole-word options,
  regex support, folder search, a command palette and fuzzy file navigation.
  Search completeness and folder replacement remain beta limitations.
- **Configurable controls:** VS Code, Notepad++ and Vim keymap presets, custom
  bindings and TOML settings, with settings interfaces in both frontends.
- **Text encodings:** UTF-8, UTF-16 LE/BE, UTF-32 LE/BE and supported stateless
  legacy encodings, including Shift_JIS, EUC-JP, GBK, Big5 and Windows-1252.
  Untouched encoded bytes and malformed-byte provenance are preserved by the
  shared text/save engine. Per-file encoding and line-ending commands are unfinished.
- **Hex editing:** inspect physical bytes and edit in hex, with an explicit
  text/hex choice for binary input.
- **Syntax highlighting:** bundled language support and a generic fallback for
  other text, with background highlighting work.
- **Markdown preview in the GUI:** text, preview and split modes, including
  supported local images. Rendered preview is limited to 256 KiB of logical source.
- **Rendering designed for remote desktops:** event-driven updates, a CPU
  renderer and remote-mode controls. Cross-platform damage and remote bandwidth
  budgets still need qualification.

These describe implemented areas to try, not a promise of complete GUI/TUI parity
or acceptance of every v1 requirement. Compare views, formatter execution, log
following, custom themes and accessibility remain incomplete. Any-size opening,
resident index memory limits, full allocation admission and published performance
budgets are also unfinished. ISO-2022-JP is explicitly unsupported.

## Try it

The beta uses raw executables for Windows, macOS and Linux, on x64 and ARM64.
After publication, download the matching assets and `SHA256SUMS` from
[Releases](https://github.com/rennerdo30/kwil/releases).

On Windows, keep the matching pair together as `kwil.exe` and `kwil.com`:

```powershell
.\kwil.exe notes.txt
.\kwil.com --tui notes.txt
```

On macOS or Linux, make the downloaded executable runnable, rename it to `kwil`
and run:

```sh
./kwil notes.txt
./kwil --tui notes.txt
```

Beta binaries are unsigned and installed/replaced manually. Stable installers
and the auto-updater do not select beta releases. See the
[installation guide](docs/INSTALL.md) for stable-release requirements.

To build from source, use `scripts/run.bat` on Windows or `scripts/run.sh`
elsewhere. Add `--release` for an optimized build or `--tui` for terminal mode.

## Screenshots

Actual beta TUI sessions captured through a hidden Windows ConPTY and rendered
from recorded terminal output, at 120 × 32 cells. See the
[capture method and limits](docs/SCREENSHOTS.md#terminal-beta-captures).

[![Kwil beta terminal editor showing Rust syntax highlighting](docs/images/tui-editor-beta1-20261026.png)](docs/SCREENSHOTS.md#terminal-beta-captures)

[![Kwil beta terminal search showing four highlighted matches](docs/images/tui-search-beta1-20261026.png)](docs/SCREENSHOTS.md#terminal-beta-captures)

Historical Windows GUI captures from 24 October 2026 use a development build
with the frame harness enabled. The [gallery and provenance](docs/SCREENSHOTS.md)
identify their source and capture limits; they are not beta artifact verification.

[![Markdown source beside its rendered preview](docs/images/windows-markdown-split-20261024.png)](docs/SCREENSHOTS.md#markdown-split-view)

[![About dialog with build and renderer diagnostics](docs/images/windows-about-20261024.png)](docs/SCREENSHOTS.md#build-diagnostics)

## Project

- [Beta status and checks](docs/BETA.md)
- [v1 specification and performance targets](docs/SPEC.md)
- [Architecture decisions](docs/decisions)
- [Report an issue](https://github.com/rennerdo30/kwil/issues)

License: [MIT](LICENSE).
