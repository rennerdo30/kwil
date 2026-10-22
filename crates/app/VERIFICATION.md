# KW-13 verification and pending gate

Base: `778e875098a04badc3c4e3d14afefecd5cf5301c`; branch: `kw/13-launcher`.

## Implemented

- Pure injectable display/session/flag selection and terminal fallback decisions.
- Windows GUI and console subsystem entry points sharing the application library.
- Console-to-GUI first-frame acknowledgement, unchanged arguments, bounded startup, and terminal-owned fallback.
- Windows release staging for both `.exe` and `.com`, retaining SHA256SUMS names/format.

## Verification (Windows, implementation tree based on the commit above)

All cargo commands used `CARGO_TARGET_DIR=E:/Development/_cache/cargo-target/kwil-kw-13-launcher` and `-j 4`.

- `cargo test --locked -j 4 -p kwil --features windows-console`: 23 frontend/fallback tests and four CLI integration tests passed. The one-shot measurement is ignored in regular runs.
- Deliberately inverted selector/fallback results: all 23 functional tests failed, zero passed. Restored immediately.
- Deliberately changed version/help output: all four CLI tests failed. Restored immediately.
- `cargo test --locked -j 4 -p kwil --lib measure_real_detection_once -- --ignored --nocapture`: real environment plus Windows session/window-station probes took **39.2 microseconds**, one measurement.
- `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`: passed after fixing a needless return in the CLI timeout helper.
- `cargo clippy --locked -j 4 -p kwil --all-targets --features windows-console -- -D warnings`: passed.
- `cargo fmt --all --check`: passed.
- PE headers: `kwil.exe` subsystem 2, staged `kwil.com` subsystem 3.
- `.exe --version` and `.com --version`: `kwil 0.1.0`, exit 0.
- Direct `.exe --hidden <unique temporary file>`: alive after 2500 ms; closed its recorded PID 2180.
- `.com --hidden <unique temporary file with spaces>`: launcher PID 64932 returned exit 0 after GUI first-frame acknowledgement; its recorded GUI child PID 23512 remained alive, DWM cloak attribute was 1, then was closed. Temporary file removed.
- An earlier smoke harness waited for inherited stderr EOF after launcher exit. The GUI held that handle, so the harness blocked; closed recorded descendant PID 66156 and corrected the harness to read the PID line instead. This was a harness error, not a launcher exit failure.

## Pending, not acceptance

Paired update implementation is blocked on a design decision sent to the coordinator: use a staged, locked transaction with rollback and recovery, or introduce permanent stubs/version-directory indirection. Two independent PE paths cannot be replaced by one atomic filesystem operation. `crates/update` is unchanged; **do not release this branch**. The release documentation records this gate.

No updater transaction tests, release builds, actual service/SSH sessions, native macOS/Linux launches, or full local workspace tests ran. Service and SSH behavior is covered through injected inputs. Three-OS CI starts with the PR and is handed to the parent; this delegated task does not watch or poll PRs.
