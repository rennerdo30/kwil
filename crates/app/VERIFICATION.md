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

## Approved paired-update follow-up (2026-10-23)

Starting commit for this follow-up: `d6e0e1817b26e7c0815ca116dca1f698c10ed5a3`. The owner approved staging, a shared startup/update lock, backups, rollback and journal recovery. The earlier paired-update gate is resolved by `crates/update/src/transaction.rs`; this is transactional with recovery, not crash-atomic.

- Eleven new filesystem tests cover the five requested cases plus missing backups, committed recovery, exe-only migration, interrupted rename/rollback retries and an incomplete committed pair, and concurrent recovery during lock upgrade. Temp folder names include PID, a per-process counter and time.
- Fail-first: the initial eight tests all failed with deliberately disabled transaction/startup operations. The ninth (rename/rollback retry) failed with startup recovery disabled. The tenth caught a real incomplete-commit recovery failure before its correction. The eleventh caught a real lock-upgrade race: another startup could finish recovery before the first took the exclusive lock. Both now require a restart. All eleven pass after restoration/fixes.
- The exe-only migration assertion also failed before changing install order to add the recovery-capable console before retiring the only exe.
- `cargo test --locked -j 4 -p kwil-update`: 13 passed (11 new plus 2 existing).
- `cargo test --locked -j 4 -p kwil --features windows-console`: 27 passed, one one-shot measurement ignored.
- Workspace clippy and feature-enabled app clippy with `-D warnings`, and formatting: passed.
- Native Windows mapped-image recovery: isolated copies of real GUI/console PEs with distinct appended old/new markers, synced backup hashes and an interrupted journal. Hidden GUI entry PID 73088 restored both old hashes while its new image was mapped and exited 1 with the restart message. Restart PID 8332 printed the version and exited 0.
- Final hidden first-frame integration smoke after adding startup recovery: launcher PID 76688 returned 0, GUI child PID 40832 reached its first frame with DWM cloak=1, then was closed by its recorded PID.
- Cross-process Windows lock smoke: a .NET process held the actual file lock; hidden console PID 9284 exited 1 with `lock unavailable`. After releasing it, console PID 55096 printed the version and exited 0. All recorded PIDs exited within their 10 s timeouts; the isolated smoke folder was removed after verifying its resolved path stayed under the branch cache directory.

No full local workspace tests, release build/download, actual SSH/service sessions, power-loss simulation, or native Linux/macOS runs were performed. Three-OS CI is handed to the parent; this delegated task does not watch or poll PRs. No tags or releases are created.
