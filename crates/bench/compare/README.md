# External compare measurement (KW-28)

This separate Cargo workspace measures the production all-size engine without
changing the original core-text benchmark executable, lockfile or baselines.
`Bench budgets` runs it on `ubuntu-24.04` and `windows-2025`. Dispatch with
`compare_only=true` on the PR branch to skip the other benchmark jobs.

`generate` writes two real, non-sparse UTF-8 files in bounded 1 MiB chunks.
Each row is 1,024 bytes with a unique numeric prefix. Sixteen rows, spaced
evenly through the right file, have one substituted byte. The hosted job runs
one pair of 1 GiB files, removes that private fixture, then runs a 2 GiB pair.
This measures a sparse-difference workload, not Myers' worst-case edit distance.

`measure` starts in a fresh process. It uses the same snapshot capture on the
left and encoding-detected `DecodedNativeLease` on the right as the production
compare worker, seals immutable `InputStore`s, then invokes `stream::begin`.
The timer includes opening/indexing the left snapshot, both captures, alignment
and explicit retirement. It excludes fixture generation, compilation, GUI/TUI
projection and word refinement. Disk caches are uncontrolled. One sample per
size is evidence, not a statistical regression comparison.

Linux records `getrusage(RUSAGE_SELF).ru_maxrss` (KiB converted to bytes).
Windows records `GetProcessMemoryInfo`'s `PeakWorkingSetSize` (peak RSS) and
`PeakPagefileUsage` (peak private committed bytes, not physical RAM), plus final
`PrivateUsage`. Native high-water marks cover the whole measurement process;
they are not inferred from the engine's admission ledger. Scratch disk and
admitted resident high-water marks are reported separately. Private bytes are
not available as a native Linux peak counter here and are reported as null.

A ten-minute cooperative limit cancels through the existing engine API and
retires scratch owners. It emits `complete=false`, the stop phase, elapsed
time and process high-water marks. This is a lower bound on completion time,
not a successful diff. A separate subprocess timeout bounds a stuck operation;
raw progress is retained if that fires. Incorrect hunk/row counts, IO failures
and missing terminal output fail the measurement job. The 1 MiB smoke requires
completion; the large-file measurements initially have no performance gate.
Only promote completed, reasonable hosted numbers to budgets.

Local usage is restricted to at most 8 MiB by both generation and measurement:

```powershell
cargo build --manifest-path crates/bench/compare/Cargo.toml --release --locked -j 4
python crates/bench/compare/run.py --binary <target>/release/kwil-compare-bench.exe --sizes-mib 1 --require-complete --output <reports>/compare
```

Run local Cargo builds/checks through the project's machine queue. No multi-GB
fixtures or benchmark loops belong on the owner's PC.

Source fences already have focused coverage in
`core-text/src/compare_input/tests.rs`:
`source_changed_and_io_payload_survive_failure_with_the_loan` rejects a source
changing at the final seal, and
`native_invocation_handle_is_captured_without_a_buffer_index_and_rechecks_stamp`
checks native handle capture and mutation rejection. `InputBuild` calls
`SourceLease::validate` before capture and after EOF; `NativeFileLease` compares
length, modification time and file identity. `SnapshotLease` validates its
pinned original generations through its worker cursor. Once sealed, the
engine reads the immutable private stores, so later original-file edits cannot
change the captured comparison. These checks use metadata fences, not an
adversarial content hash against writes that also restore metadata.
