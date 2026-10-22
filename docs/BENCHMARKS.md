# Speed budgets

KW-33 measures only operations present at `b1b5c55`. Main was merged after KW-9
landed; the same TextFile methods now return checked Results, which the harness
propagates rather than bypassing file-safety checks. No new search/diff/edit API
is benchmarked. The benchmark crate is unpublished.
Its linked library has no default features, sampling thread or periodic timer.
The `harness` feature enables the binary and text measurements. Divan **0.1.21**
(MIT OR Apache-2.0, published 2025-10-04, checked through the crates.io API
2026-10-23) is a development dependency. Its nine added lockfile packages do not
enter the shipped binary. Existing serde_json/libc/windows-sys versions are reused.

## Measurements

All times use release builds and warm OS file cache. Text benches use four index
workers; startup uses the viewer's production count (available CPUs, at most eight). GB/s is
decimal bytes/second; fixture sizes and private memory use binary GiB/MiB.
No disk cache flush, privilege escalation, GPU device startup or network is involved.

| Measurement | What is timed / sampled | §3 target | Hard gate |
|---|---|---|---|
| Open + first screen | `TextFile::open` + first 60 raw lines, fresh process | ≤40 ms | ≤100 ms |
| Index | Fresh `TextFile::build_index(4)`, including worker creation/join | ≥2 GB/s | ≥1 GB/s |
| Jump to line 15M | `TextFile::lines(15_000_000, 60)` with ready index | ≤1 frame | ≤16 ms |
| Clean 1 GiB open memory | After index + jump, absolute process private memory | ≤64 MiB | ≤256 MiB |
| GUI launch with file | OS process creation to successful content present | ≤40 ms | ≤100 ms |
| GUI empty launch | OS process creation to successful present | ≤25 ms | ≤60 ms |
| Launch to index done | OS process creation to worker completion | No separate limit | OS baseline |
| Viewer idle | Actual successful presents in two scripted idle phases | 0 | 0 |
| Viewer scroll | 20 single-line steps; presents, copied/damaged pixels, CPU p99 | One copy + exposed row and status | 20 partial presents; Windows copy and exact damage |

Divan reports the first three timings, index byte throughput, and the associated
budgets on stderr. Memory is deliberately measured by the **fresh-process harness**,
not a shared Divan process retaining previous mappings and allocator state. Windows
uses `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage` (private committed bytes). Linux
sums `Anonymous` from `/proc/self/smaps` (resident anonymous pages); this excludes
read-only mapped file page cache, but is not Windows private commit. The OS
baselines are never mixed. This is the core/viewer measurement, not a claim that
every future editing path is memory-bounded.

`--trace-startup` prints `t_launch`, `t_first_frame`, `t_index_done` in milliseconds
since OS process creation to stderr, then exits after both a successful first
present and index completion. Indexing may finish *before* the first frame; its
timestamp is preserved. Windows uses process FILETIME; Linux uses `/proc/self/stat`
and CLOCK_BOOTTIME (clock-tick start resolution); macOS uses proc_pidinfo.
The flag suppresses automatic update startup. It works with `--gui --hidden` and
`--tui` in an interactive terminal. TUI cleanup restores the terminal before exit.
It never turns a failed present into a successful startup measurement.

The `frame-harness` app feature links the scenario seam. `--frame-script` starts a
bounded script worker only after the first frame and index: settle, idle 500 ms,
20 scroll steps, idle 500 ms, exit. The UI remains event-driven. Actual present
damage and successful native screen copies feed the statistics; a model-only
damage test cannot pass this gate. p99 is reported from the 20 measured CPU frame
durations (nearest rank = maximum); this is not input-to-display latency.
Windows enforces copy + exposed row + status. **TODO KW-11:** Linux native copy;
the current viewer presents the moved region as damage there. No real RDP/VNC
wire measurement is claimed (KW-30).

## Fixtures and light local use

Generation is explicit, never in a test or normal application startup. The port of
`spike/textbench gen` keeps its deterministic xorshift log format without linking
any spike. It writes bounded buffered chunks to ignored `fixtures/`:

- `log-1g.log`: at least 1 GiB, over 20M log lines, including `line=15000000`.
- `csv-20m.csv`: exactly 20M CSV rows.
- `one-line-100m.json`: 100 MiB, valid one-line JSON.
- `mixed-encoding/`: UTF-8/mixed endings, UTF-16LE, Windows-1252 and invalid bytes.
- `tree-100k/`: 100 shards, 1,000 small files each.

CI generates all fixtures once per OS/generator-source cache key. A completion
stamp is written only after successful full generation. Change the generator
version and baseline fixture version together when changing fixture semantics.
Locally generate only the log unless another fixture is specifically needed:

```powershell
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-33-bench-ci'
$env:KWIL_NO_UPDATE='1'
cargo build --release --locked -j 4 -p kwil-bench --features harness -p kwil --features frame-harness
& "$env:CARGO_TARGET_DIR/release/kwil-bench.exe" generate fixtures
python -m venv fixtures/bench-venv
fixtures/bench-venv/Scripts/python.exe crates/bench/run.py --bin-dir "$env:CARGO_TARGET_DIR/release" --fixtures fixtures --runner owner-windows --runs 3 --output fixtures/local.json
# Divan alternative (three samples, one operation each; do not run repeatedly):
$env:KWIL_FIXTURES=(Resolve-Path fixtures).Path
cargo bench --locked -j 4 -p kwil-bench --features harness --bench text -- --sample-count 3 --sample-size 1
& "$env:CARGO_TARGET_DIR/release/kwil.exe" --tui --trace-startup fixtures/log-1g.log
```

Agent GUI runs always use `--hidden` and a timeout. The runner records each child
PID and terminates only that child on its 60-second deadline. No local full suite,
stress loop, repeated benchmark campaign, cold-cache test or low-RAM run is allowed.
The fixture generator is bounded by the caller's launch timeout.

## CI and baselines

`bench.yml` runs on pushes to main, PRs with label `bench` (including later pushes),
and manual dispatch. Public GitHub-hosted `windows-2025` and `ubuntu-24.04` runners
are required by the repository rule, which supersedes the older self-hosted text
in the spec/issue. Linux uses Xvfb for actual window creation/presents.

The runner warms the mapped file cache using a 1 MiB buffer, then measures a
median of **five fresh processes**, separately for text, GUI file and GUI empty.
Every numeric budget fails both on its hard limit and on **cost >110% of the
stored OS median**. Throughput compares inverse GB/s, i.e. time per byte. A slow
baseline can never waive a hard limit. Raw samples, medians and source commit
are uploaded on success or failure. Tests inject slow timings to check every
hard limit, regression direction and missing/mismatched baseline rejection.

Baselines are `windows-2025.json` and `ubuntu-24.04.json` on the separate
**`bench-baseline` branch**. CI reads that branch with `contents: read`, never
writes to it or `main`, and fails closed if it is absent or incompatible. Files
use the same schema as the uploaded result JSON. This choice keeps baseline
updates explicit and reviewable, avoids artifact expiry silently resetting the
comparison, and prevents a PR from blessing its own results through CI writes.
Branch protection/review of baseline changes belongs to the coordinator.

To bootstrap or deliberately update a baseline:

1. Run the workflow on the intended reviewed source. A missing baseline fails
   **after** recording results; that is not a passing benchmark gate.
2. Download both `bench-<runner>` artifacts. Check their source commit, raw
   samples, hard limits and frame results. Investigate any slowdown first.
3. Commit the exact result JSON files to `bench-baseline` in a separate checkout
   (identity through environment; explicit-ref push). Never copy owner-PC numbers
   into a hosted baseline or mix runner OS/images. Record the reason and source
   run links in the baseline commit and KW-33.
4. Re-run the candidate gate against those stored files. Baseline updates do not
   change the §3 limits or the >10% comparison.

The acceptance proof temporarily adds a real one-second delay to the line-index
build on this PR, runs CI against the stored baseline, and removes it in a later
commit. The red run must specifically show an index budget/regression failure,
then the final head must pass the same workflow. Run links and local numbers are
recorded in the PR and KW-33.

## Slots for integration after other packages merge

- **TODO KW-22:** literal search, 1 GiB, target 300 ms / hard 1 s.
- **TODO KW-28:** 100k-line diff, target 100 ms / hard 500 ms.
- **TODO KW-16:** piece-tree edit, typing damage, keystroke-to-pixels ≤16 ms.
- **TODO KW-25:** palette/file finder against the generated 100k-file tree ≤16 ms.
- **TODO single-instance package:** second launch to shown file ≤50 ms.
- **TODO KW-10:** decoded mixed-encoding viewport measurements; current TextFile
  exposes raw bytes only.
- **TODO KW-11/KW-30:** Linux copy, native remote-session/wire metrics, display
  timing and dropped frames. A CPU p99 alone does not prove refresh-rate delivery.
- **TODO KW-40:** CI small-RAM machine / 4 GiB fixture; not substituted by a
  working-set estimate or an owner-PC stress run.
