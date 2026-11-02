# Benchmarks and speed gates (KW-33)

The optional **Bench budgets** workflow measures the core-text operations from
spec §3 that are currently compatible with main. It runs on pushes to `main`,
manual `workflow_dispatch`, and PRs carrying the `bench` label. Unlabelled PRs
skip the budget job. **Never make Bench budgets a required branch check.** The
normal `CI` workflow is unchanged: no fixture generation, release build or timed
measurement is added to it. The bench crate is a separate Cargo workspace,
excluded from the production workspace glob, with its own lockfile. Ordinary CI
does not compile its dependencies, and the production lockfile and cache keys
stay unchanged. Benchmark checks run in the optional workflow.

## Current coverage

| Measurement | Target | Hard limit |
|---|---|---|
| `TextFile::open` plus first 60 lines of the 1 GiB log | 40 ms | 100 ms |
| Background line-index algorithm, four workers | 2 GB/s | 1 GB/s |
| Jump to line 15,000,000 after indexing | one frame | 16 ms |
| Private memory after opening and indexing, excluding clean file page cache | 64 MiB | 256 MiB |

These are core-text measurements, not full-editor memory or input-to-pixels
acceptance. Windows samples process private usage; Linux sums anonymous resident
pages from `/proc/self/smaps`. This preserves the existing per-OS baseline's
memory definition. Divan reports API timings; the fresh-process harness supplies
the actual gate medians and memory measurements.

**GUI startup and second-launch numbers are informational.** The optional
workflow builds the release app with `startup-bench` (the existing
`ui-gpu/frame-harness` feature) on Linux, Windows and macOS. `bench-startup.py`
records launcher timestamps and the existing `KWIL_TRACE_STARTUP` stage marks.
It launches hidden native CPU windows with isolated profiles, records every
PID, and bounds every wait/exit. Linux uses Xvfb. These measure successful
native presentation, not compositor scanout or visible-desktop acceptance.

One sample each covers first/repeated empty processes, a 76 KiB text file, a repeated launch,
and a 1 GiB text file. "First" means a fresh process/profile; the runner's disk
cache is uncontrolled, so this is not a cache-flushed cold-boot measurement.
The first chrome frame and the first successfully presented fixture-content
row are separate marks. A missing, covered or undrawn row cannot satisfy the
content measurement. A vetted read-only preview counts while the final open
still owns editing/input; timing follows the renderer's paint fence. The
second process opens a different sentinel file only
after the original UI route reports ready. IPC acknowledgment and display by
the original process are reported separately. Raw logs and timestamps are
retained even on failure. Targets/hard limits remain 25/60 ms empty, 40/100 ms
file and 20/50 ms second launch; none of these numbers block CI yet.

Windows records Popen and native `CreateProcess` entry/return timestamps plus
the child's OS creation time in each case's `spawn` object. This separates
parent-side preparation and process creation from loader/CRT work before the
first Rust `process entry` mark. `app entry` follows console attachment;
recovery/argument and IPC marks follow it. Individual DLL initializers share
the loader/CRT interval. This probe launches `kwil.exe` directly: it does not
include a `kwil.com` to `kwil.exe` handoff. The console entry also attempts
authenticated forwarding before it considers starting a GUI child.

An informational palette case generates a 100k-file tree in the hosted runner's
temporary directory, discovers it through the shipped catalogue, and measures
one `file-0000` query through the shared palette worker until fresh results are
applied. Discovery and fixture generation are outside the timer. The job summary
reports the elapsed time and whether it misses 16 ms, alongside core-text medians;
the raw JSON is retained with the other results. This case is non-blocking, has
no baseline comparison, and does not measure GUI/TUI presentation or native input.
The `kwil-palette-bench` executable lives in its own `crates/bench-palette`
workspace with its own lockfile and CI target directory. It refuses to generate
the full tree outside GitHub Actions. The original `crates/bench` workspace,
including its sources, manifest, features and lockfile, stays unchanged; palette
dependencies cannot affect its resolution or measured process. Local tests use
a six-file tree only.
The isolated palette binary enables the dev-only `palette-profile` feature:
its raw log reports the number of labels examined/scored and matcher time.
Production builds contain neither these counters nor clocks. macOS reports the
same 100k-file query in the informational startup job; the existing Linux and
Windows core-text gates retain their original processes and target directories.

The other §3 budgets (search, diff, full-editor memory, typing, scrolling,
remote drawing) still need their own representative integration measurements.
KW-33 remains open for those and for promoting informational budgets to gates.

## Noise and regression policy

The gate uses **eight fresh candidate processes and eight fresh reference
processes**, in complete balanced **ABBA/BAAB** cycles (A = candidate,
B = reference). Both versions are fully built first, and the canonical fixture
is warmed outside timed work with a 1 MiB buffer. Hard limits and private memory
use the medians of the eight samples. No local benchmark loops run on the
owner's PC.

Every applicable hard limit is enforced, independently of the reference.
For timing, the trusted stored baseline pins the source code rebuilt and measured
on the same hosted runner as the candidate. Each ABBA or BAAB iteration produces
one balanced cost ratio: the geometric mean of its two opposite-order paired
candidate/reference ratios. This cancels multiplicative first-process penalties
instead of retaining a 5/4 ordering imbalance or averaging unpaired medians.
The comparison uses the median of these four per-iteration ratios. For indexing,
cost is inverse throughput (GB/s).

A cost increase **over 10%** fails when the full observed spread of balanced
ratios is above 1.10. If that spread straddles 1.10, the comparison is explicitly
**INCONCLUSIVE** and warns without failing the workflow. A warning is not timing
acceptance. The threshold remains 10%; a clear 15% slowdown fails even with a
20% first-process penalty. The result retains raw samples, pair order, balanced
ratios, their medians and every inconclusive warning for review.
Private memory compares directly with the stored OS median: paired timings cannot
waive memory growth. Missing inputs, wrong runner/fixture/schema, invalid numbers
and a reference checkout that differs from the pinned source fail closed.
Targets are informative; they do not become additional flaky gates. Thresholds
are never relaxed to obtain a green run. Raw samples, medians, source commits,
the stored baseline and reference manifest are uploaded even on gate failure.

## Stored baseline: keep `bench-baseline`

The **data branch is still required**. The workflow reads it but has only
`contents: read`; PRs and manual runs cannot overwrite a baseline or write main.
It fetches one exact data commit and reads all three inputs from that commit:

- `ubuntu-24.04.json`: the reviewed Linux samples and medians.
- `windows-2025.json`: the reviewed Windows samples and medians.
- `reference.json`: `source_head`, a full commit SHA containing the reference
  measurement crate and source. That checkout is independently verified before
  any sampling starts.

The existing data commit is `081a4df61f4ec8ea82d0988a7489dd3c84679722`;
its reference source is `2a297eb814064f68144c91a8df665855fe1eace2`.
Schema 1 remains compatible: the older result's startup fields are ignored by
the four current core-text gates. No baseline reseeding is part of this port.

A future baseline update is a deliberate reviewed change on the data branch,
not an automatic reward for a green candidate. Download each OS result JSON
from one accepted run, inspect raw samples and hard-limit results, then commit
those exact result files as `<runner>.json`. Update `reference.json` with the
accepted measurement source SHA and run URL in the **same data commit**. Push
with `git push origin HEAD:refs/heads/bench-baseline` after review. Keep the
measurement API, fixture version and memory definitions compatible. Do not point
at main before it contains the bench crate, use an unreviewed candidate, copy a
failed result, or change a threshold as part of refreshing the baseline.
Artifacts expire; the data branch keeps the trusted results and source pointer
independently of artifact retention.

## Commands and fixtures

Run on hosted CI using:

```sh
gh workflow run bench.yml --repo rennerdo30/kwil --ref kw/33-bench-ci
```

For an admitted benchmark machine (not the owner's PC):

```sh
cargo build --manifest-path crates/bench/Cargo.toml --release --locked -j 4 --features harness
# Only the fixture consumed by the current gates:
target/release/kwil-bench generate fixtures
# Optional spec fixtures, generated rather than committed:
target/release/kwil-bench generate fixtures --all
KWIL_FIXTURES="$PWD/fixtures" cargo bench --manifest-path crates/bench/Cargo.toml --locked -j 4 --features harness --bench text -- --sample-count 9 --sample-size 1
```

The deterministic generator uses bounded buffers. The required `log-1g.log` is
at least 1 GiB and contains over 20 million numbered lines. `--all` also creates
a 20M-line CSV, 100 MiB single-line JSON, mixed encodings and a 100k-file tree;
these optional fixtures do not imply gated coverage. All fixtures live under
the ignored `fixtures/` directory and CI caches them by generator hash and OS.

Focused gate tests use synthetic measurements, never large fixtures:

```sh
python -m venv .bench-venv
# Windows: .bench-venv/Scripts/python.exe
.bench-venv/bin/python crates/bench/test_gate.py
.bench-venv/bin/python crates/bench/test_pairs.py
```

The Rust API bench uses the branch's existing development-only Divan 0.1.21
(MIT OR Apache-2.0). The ordinary crate has no UI dependencies, release app
changes or background sampling threads.
