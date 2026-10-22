# KW-22 / KW-23 verification

Starting base: `b1b5c55658953a8572ea10662adf4f2aa4e7f730` (`main`).
Final implementation measured: `e7d8ff36e3e9dfed5938c30749238b14bd497845`.
Branch: `kw/22-search`.

## Focused checks

- `cargo test -p kwil-core-search -j 4 --locked`: **11 passed**, 0 failed,
  0 ignored; 0.06 s. No workspace test suite was run locally.
- `cargo clippy --workspace --all-targets -j 4 --locked -- -D warnings`: pass.
- `cargo fmt --all --check`: pass.
- `git diff --check`: pass.
- Tests use one temporary directory per filesystem test, and work windows of
  128 bytes plus haystack chunks of 1/2/3/7/61/127/256 bytes for overlap ownership
  across copied overlap windows and borrowed main chunks.
- `fail-first.ps1` ran each named test against its deliberately broken
  implementation. Each ran one test, failed an assertion, and exited **101**;
  the script restored original files in `finally`. All tests then passed.

| Test | Deliberate defect detected |
|---|---|
| tiny_chunks_and_work_windows_match_reference | Zero retained overlap bytes |
| case_word_selection_and_invalid_bytes | Disable case folding |
| count_progress_and_cancellation_stop_early | Stop incrementing the count |
| capture_replace_plans_and_literal_dollars | Replace capture text with `BROKEN` |
| filter_boolean_runs_and_incremental_partial_line | Invert the filter result |
| filter_long_lines_are_bounded_and_runs_enforce_capacity | Invert the filter result |
| files_gitignore_globs_previews_ranges_and_mapped_adapter | Disable `.gitignore` |
| files_cancellation_backpressure_and_disconnect | Count all 500 lines at the first result |
| files_oversize_lines_report_error_and_range_caps | Hide the range truncation flag |
| invalid_ranges_chunks_and_match_bounds_are_errors | Ignore the declared match bound |
| parallel_default_matches_serial_and_preserves_order | Corrupt per-shard counts |

Full local mutation output: ignored `fixtures/search-fail-first.log`.
The script is committed so the evidence can be reproduced without that log.

## Authorized optimized measurement

After the first measurement below, the coordinator explicitly authorized the
copy-free chunk optimization and **one** targeted remeasurement: each query at
one thread and at the shipped default. This run measured the committed
implementation `e7d8ff36e3e9dfed5938c30749238b14bd497845`; subsequent changes only
record its evidence. The fixture was not regenerated. There were no warm-ups,
repeated samples, benchmark loops or cache flushes. Real windows are 1 MiB;
compilation used release mode and `-j 4`.

**Shipped editor default:** `search_default` uses half the logical CPUs capped
at four search workers (four on the owner PC), plus a light channel-draining
coordinator. This leaves CPU headroom for the UI. `search_with_threads` supports
lower admission counts. Only patterns proven not to consume LF are split at
nearby newline boundaries; multiline patterns, files without nearby newlines
and small ranges stay serial, preserving exact non-overlap semantics. Main
chunk bodies are borrowed; only unresolved overlap tails/prefixes are copied.

| Query | Count in each configuration | 1 thread | 4 threads (editor default) |
|---|---:|---:|---:|
| Literal rare `line=20353525` | 1 | **676.149 ms** | **22.488 ms** |
| Literal common `INFO` | 5,090,586 | **136.712 ms** | **36.614 ms** |
| Regex `ERROR line=[0-9]{1,8}` | 5,086,074 | **531.799 ms** | **144.023 ms** |

**Both literal cases meet the 300 ms target at the shipped four-worker
default**, and both meet the 1 s hard limit. The one-thread rare case still
misses 300 ms; the budget is not changed or waived. Regex has no separate budget.

Run order was rare/1, rare/4, common/1, common/4, regex/1, regex/4. Page-cache
state was uncontrolled; rare/1 includes the new mapping's first access and
rare/4 follows it in the same process. These numbers therefore do not establish
an isolated parallel speedup or a cold-cache guarantee. The spec budget is for
a file in memory. Full local output: ignored `fixtures/search-measurement.log`.

The new equivalence test checks serial versus parallel hits/counts, hit order,
Unicode word boundaries, zero-width shard boundaries, multiline/giant-line
fallback and cancellation. All 11 mutation checks failed assertions with exit
101, and the restored tests passed. Workspace Clippy/fmt passed afterward.

## Original single fixture measurement

Owner PC, release profile, `-j 4` compilation; **one search thread** (caller),
no worker pool for this measurement. Real work windows are 1 MiB. The example
runs each query exactly once; no warm-up, repetition, cache flush or stress loop.
The fixture was generated with the specified `spike/textbench` command:
**1,073,741,835 bytes, 20,353,526 lines**. Page-cache state was not controlled;
the rare query is first and includes the mapping's first access.

| Query | Count | Elapsed |
|---|---:|---:|
| Literal rare `line=20353525` | 1 | **419.744 ms** |
| Literal common `INFO` | 5,090,586 | **145.676 ms** |
| Regex `ERROR line=[0-9]{1,8}` | 5,086,074 | **528.087 ms** |

At original implementation `4a02bf9fd9b9e211bdc22730fa1d67872e632d9a`, the common
literal met 300 ms and the rare literal missed it. Both met the 1 s hard limit.
That miss triggered the explicitly authorized optimization and single targeted
measurement above; these original numbers are retained without a target waiver.

Generation built the separate spike workspace's existing dependencies, which
temporarily refreshed its `kwil-core-text` lock entry from 0.0.0 to 0.1.0. That
generated lock change was restored; the final scope is only `core-search` and
the root `Cargo.lock`.

## Not run / remaining

- No local full workspace test suite, GUI, TUI, native launch, live UI frame
  budget test, cold-cache benchmark or other-platform local test.
- Cross-platform full tests belong to hosted PR CI; the parent monitors CI and
  independent review and decides acceptance. This delegated job does not merge.
- The API's declared regex match-length bound and folder grep line-buffer cap
  are documented limits, not arbitrary-length regex/line-buffer claims.
- No actual editor undo transaction, watcher rotation or UI wiring is in scope.
