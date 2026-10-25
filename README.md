<p align="center"><img src="assets/logo/kwil-wordmark.svg" alt="Kwil" width="420"></p>

A blazing fast plain text editor for GUI and terminal, written in Rust.

Installation packaging is being prepared: see [one-line installation](docs/INSTALL.md)
for its published-release requirements and platform dependencies.

- Opens files of any size instantly (a 1 GB, 20M-line log shows its first screen in 0.1 ms).
- One binary: a native window when a desktop is available, a terminal UI when not (`--gui` / `--tui` to force).
- Built for remote desktops: no frame loop, no idle redraws, damage-only drawing and scrolling by copy.

Try the current state: `scripts/run.bat` on Windows or `scripts/run.sh` elsewhere. It builds and opens the README; add `--pull` to update first, `--release` for full speed, `--big` for the 1 GB test log, or `--tui` for the terminal UI.

Status: early development (M0 done; see [docs/SPEC.md](docs/SPEC.md) and [docs/decisions](docs/decisions)).

## Screenshots

Historical Windows GUI captures from 24 October 2026, using a development build
with the frame harness enabled. See the [gallery and provenance](docs/SCREENSHOTS.md)
for build identity, capture method and limits; these do not validate the current checkout.

[![Kwil showing Markdown source beside its rendered preview](docs/images/windows-markdown-split-20261024.png)](docs/SCREENSHOTS.md#markdown-split-view)

[![Kwil About dialog showing the captured build and renderer](docs/images/windows-about-20261024.png)](docs/SCREENSHOTS.md#build-diagnostics)

## Known limitations

- **ISO-2022-JP** (and other stateful encodings) are not supported yet. Such files open with a clear "unsupported encoding" message instead of garbled text. All other common encodings work, including UTF-8, UTF-16, Shift_JIS, EUC-JP, GBK, Big5 and Windows-1252. See [docs/SPEC.md](docs/SPEC.md) §5.

License: [MIT](LICENSE).
