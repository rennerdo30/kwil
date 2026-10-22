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

For Linux CI, the parent records CLOCK_BOOTTIME immediately before Popen and
reads that child's `/proc/<pid>/stat` start tick. It subtracts the difference
between this precise launch boundary and the kernel's floored creation tick
from the raw trace marks. That cancels the shared 10 ms quantization error.
Gated ages conservatively include process-spawn overhead; they measure launch
request to the child's actual timestamp, not time until Python reads the pipe.
The original OS marks, correction and spawn interval remain in each sample.
The standalone flag still prints the OS process-age marks. Windows uses its
precise process FILETIME without correction.

The `frame-harness` app feature links the scenario seam. `--frame-script` starts a
bounded script worker only after the first frame and index: settle, idle 500 ms,
20 scroll steps, idle 500 ms, exit. Each step waits for a successful present
acknowledgement before the worker sends another step (five-second deadline).
The 20 ms interval is a minimum gap, not an assumption that a hosted Windows
present completes within 20 ms. CPU p99 is still recorded. The UI remains event-driven. Actual present
damage and successful native screen copies feed the statistics; a model-only
damage test cannot pass this gate. p99 is reported from the 20 measured CPU frame
durations (nearest rank = maximum); this is not input-to-display latency.
At each scripted scroll event, before asking the view for damage, the harness
derives the exposed-row rectangle, status rectangle and exact copy area from
the viewport width, height and line height. Each successful present must match
those independent rectangles and copy area. Missing rows/status, misplaced
damage, missing copies and compensating copy sizes cannot pass through equal
aggregate totals. Windows enforces copy + exposed row + status. **TODO KW-11:** Linux native copy;
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
Completed fixtures are cached even when the performance gate fails, so the
deliberate slowdown and baseline bootstrap do not trigger regeneration each run.
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
Both OSes also smoke the TUI trace in a real terminal (Linux PTY and hidden
Windows console). These functional smokes are separate from GUI launch budgets.

The runner builds candidate and pinned baseline code before sampling, warms the
mapped file cache using a 1 MiB buffer, then measures **nine fresh processes per
version**, separately for text, GUI file and GUI empty. Samples are interleaved;
the version measured first alternates each round. Every numeric budget retains
its hard limit. Timings fail on **cost >110% of the baseline-code median measured
on the same OS runner**. Throughput compares inverse GB/s, i.e. time per byte.
Private memory keeps its original **stored OS median** comparison: CPU speed is
not a memory calibration. A slow reference can never waive a hard limit. Raw
candidate/reference samples, medians, exact baseline inputs and source commits
are uploaded on success or failure. Tests inject slow timings to check every
hard limit, regression direction, paired calibration, memory growth and
missing/mismatched baseline rejection.

The parent approved paired timing comparison on 2026-10-23 after
[run 39830020650](https://github.com/rennerdo30/kwil/actions/runs/39830020650)
measured unchanged baseline code at 10.96 GB/s versus its stored 17.35 GB/s;
the candidate measured 10.27 GB/s on that same host. Literal OS numbers were
mostly measuring hosted-machine variation. The threshold remains >10% and
hard limits remain independent of the calibration.

Baselines are `windows-2025.json`, `ubuntu-24.04.json` and a `reference.json`
source pointer on the separate
**`bench-baseline` branch**. CI reads that branch with `contents: read`, never
writes to it or `main`, and fails closed if it is absent or incompatible. Files
use the same schema as the uploaded result JSON; `reference.json` pins the
reviewed source head for both OSes. CI verifies the checked-out SHA before
measuring it. This choice keeps baseline
updates explicit and reviewable, avoids artifact expiry silently resetting the
comparison, and prevents a PR from blessing its own results through CI writes.
Branch protection/review of baseline changes belongs to the coordinator.

To deliberately update the initialized baseline:

1. Run the workflow on the intended reviewed source. A missing or incompatible
   baseline fails closed; samples are retained if execution reached measurement.
2. Download both `bench-<runner>-attempt-<n>` artifacts. Check their source commit, raw
   samples, hard limits and frame results. Investigate any slowdown first.
   Attempt-specific names retain earlier samples when a workflow is rerun.
3. Commit the exact result JSON files to `bench-baseline` in a separate checkout
   (identity through environment; explicit-ref push). Never copy owner-PC numbers
   into a hosted baseline or mix runner OS/images. Record the reason and source
   run links in the baseline commit and KW-33. Update `reference.json`'s
   `source_head` to the corresponding reviewed head whose merge produced the
   result files. Both OS files must represent that source. Never change the
   pointer merely to make an unexplained regression pass.
4. Re-run the candidate gate against those stored files. Baseline updates do not
   change the §3 limits or the >10% comparison.

The acceptance proof temporarily adds a real one-second delay to the line-index
build on this PR, runs CI against the stored baseline, and removes it in a later
commit. The red run must specifically show an index budget/regression failure,
then the final head must pass the same workflow. Run links and local numbers are
recorded in the PR and KW-33.

### Acceptance evidence

- **Package acceptance is partial; KW-33 remains open.** This package gates the
  original text/viewer APIs only. Typing and keystroke-to-pixels p99, search,
  diff, palette and single-instance budgets need coordinator integration in the
  slots below. Reported CPU frame p99 does not fulfill the input latency budget.
- The parent approved a reviewed-main refresh after KW-16/KW-25 integration:
  `bench-baseline` commit `081a4df` stores the exact nine-pair results from
  [run 39834499282](https://github.com/rennerdo30/kwil/actions/runs/39834499282)
  and pins reference source `2a297eb`. No hard limit or >10% threshold changed.
- [Run 39836398983](https://github.com/rennerdo30/kwil/actions/runs/39836398983)
  passed Windows; Linux failed open+first-screen at 0.051217 ms versus
  0.044938 ms (+14%). Candidate/reference production measurement source was
  unchanged; six of nine corresponding measurements were within 4%, while two
  candidate measurements were 23–25% slower. This is retained as a failed run,
  not discarded or used to waive the threshold.
- [Paired-protocol deliberate delayed-index red run](https://github.com/rennerdo30/kwil/actions/runs/39832069461),
  head `f4fd7d305efd8a46275e1938b083b2335257636e`: the actual one-second
  index delay fails both OSes. Linux reaches **1.0026 GB/s**, above the hard
  limit, but fails the relative gate versus **13.4645 GB/s** reference
  (**+1,243% time/byte**); startup-to-index fails too. This proves the approved
  calibration still detects an actual regression independently of hard limits.
  Windows fails the index hard limit. Both frame scenarios pass with present
  acknowledgements. The delay is removed after this run.
- Baseline seed: [hosted run 39827665077](https://github.com/rennerdo30/kwil/actions/runs/39827665077),
  source merge `c2530a2699c6afa5744bf33f9b36f98e6eb8bf13`; exact result files
  stored on `bench-baseline` at `8dadd5d76ea71ea67c2c3c703619897171b56721`.
  Both OS hard-limit checks, TUI traces and viewer scenarios passed before the
  expected missing-baseline failure.
- [Deliberate delayed-index red run](https://github.com/rennerdo30/kwil/actions/runs/39828494393),
  head `5a668071755e281b8f60440c519fd93400e1ddad`: one second in actual
  `LineIndex::build`, Linux **0.9831 GB/s**, Windows **0.8954 GB/s**, both below
  the **1 GB/s hard limit**. The delay is removed in `190753b`.
- Initial local light check (three samples, b1b5c55 text operations before main
  integration): fresh-process open **0.1636 ms**, index **6.8656 GB/s**, jump
  **0.0086 ms**, private **3.9336 MiB**. Separate three-sample Divan medians:
  open **0.06609 ms**, index **6.695 GB/s**, jump **0.001799 ms**.
- Final local viewer smoke before main integration: hidden GUI first frame
  **62.755 ms**, index done **129.760 ms**; idle **0 + 0** presents, scroll
  **20** partial presents, **19,456,000** copied pixels, **1,024,000** damaged
  pixels. Hidden Windows TUI trace exited successfully. Full suites, cold cache,
  low-RAM stress and native RDP/VNC were not run locally.

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
