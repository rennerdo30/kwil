<p align="center"><img src="assets/logo/kwil-wordmark.svg" alt="Kwil" width="420"></p>

A blazing fast plain text editor for GUI and terminal, written in Rust.

- Opens files of any size instantly (a 1 GB, 20M-line log shows its first screen in 0.1 ms).
- One binary: a native window when a desktop is available, a terminal UI when not (`--gui` / `--tui` to force).
- Built for remote desktops: no frame loop, no idle redraws, damage-only drawing and scrolling by copy.

Status: early development (M0 done; see [docs/SPEC.md](docs/SPEC.md) and [docs/decisions](docs/decisions)).

License: [MIT](LICENSE).
