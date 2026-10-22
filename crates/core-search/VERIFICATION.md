# KW-22 / KW-23 verification

Starting base: `b1b5c55658953a8572ea10662adf4f2aa4e7f730` (`main`).
Final implementation measured: `e7d8ff36e3e9dfed5938c30749238b14bd497845`.
Branch: `kw/22-search`.

## Fix round 3: rejected overlapping literal candidates

Integration base: `44adc5528423fd5a4c158ce5f40e888b457716d7`.
Merged exact PR #5 head `769f09d2fb18844b7b7adb807260abd96a864f29`
as `8ca20075cb4b8d53ac5fb81282f1dc20a47f4bfa` on
`t3/search-overlap-correctness` before editing. The only merge conflict was
`Cargo.lock`: the resolution retains the parent's packages and adds the search
dependencies, without changing versions or other manifests.

The round-3 review found that literal `a a` with whole word enabled misses the
valid `(offset=3, len=3)` hit in `xa a a` in both case modes. Non-overlapping
candidate iteration consumes the byte-1 candidate before its boundary fails,
skipping byte 3. That also removes its edit from replace-all.

Whole-word literals now use the existing byte-regex engine with compiled Unicode
boundary assertions. Boundary failures can retry overlapping starts inside the
matcher; accepted matches still advance to their end. This avoids repeatedly
restarting a literal search after each rejected start on periodic input.
Case-sensitive literals without whole word retain the unchanged `memmem` scan.
Both literal case modes keep replacement dollars literal. The shared AST
composition retains trailing extended-mode comments, scoped flags and capture
numbering; no public function or trait signature changes.

**Validated implementation:** `8f68d2298f7816fd7cdd5fdb2d2375c1f39de2d8`,
2026-10-23, 05:03 JST. The parent-controlled Cargo lane was granted by the
upstream launcher job, then released directly to the named next job after these
finite checks; every process exited before handoff, with a 180-second timeout.

- **Fail first:** the saved pre-fix `search.rs` was verified against Git blob
  `8aaf8b6184a7060c9a29ce01994111469f99b9d1` from `769f09d`, substituted only in
  this isolated worktree, then restored byte-for-byte in `finally`. The new
  `whole_word_literal_rejected_overlap_search` and
  `whole_word_literal_rejected_overlap_replace_all` tests both failed assertions
  (Cargo PID 15564, exit 101). Each reported `left: [[], []]`, proving that both
  case modes missed the required hit or edit, rather than failing compilation.
- **Fixed:** all **20/20** focused crate tests passed (PID 16848, exit 0, 0.10 s).
  The new checks cover periodic literals, accepted-hit non-overlap, Unicode case
  folding, non-word-ended patterns, invalid bytes, selection context, count mode,
  tiny chunks, work-window borders, parallel equivalence and literal dollars.
  All 18 prior tests passed, including every earlier review regression.
- Package all-targets Clippy with warnings denied passed (PID 77936, exit 0).
  Package formatting passed (PID 13352, exit 0); `git diff --check` also passed.
- Checks used `--offline --locked -j 4` and the configured shared target
  `E:/Development/_cache/cargo-target/kwil`, without any target override or clean.
  The first test build compiled missing search dependency artifacts (13.05 s);
  the fixed test build reused them and rebuilt only core-search (1.04 s).
- Logs are `%TEMP%/kwil-search-r4-86146cf7/{fail-first,focused-tests,clippy,fmt}.log`.
  Each first line records the commit, phase, start time, source timestamps and
  SHA-256 hashes. Fixed `search.rs` SHA-256:
  `419EEFD4369D87E563F0CB9ACD077A237B1F0EF8E0D6715A04CBAC8A79BB0B59`;
  test source SHA-256:
  `774093A2BA9BA879E873962882289A473727C10F3159DF57CF0F3670ECCE9E6F`.

Fixed-source commands (tests used two test threads):

```text
cargo test -p kwil-core-search --lib --offline --locked -j 4 -- --test-threads=2
cargo clippy -p kwil-core-search --all-targets --offline --locked -j 4 -- -D warnings
cargo fmt -p kwil-core-search -- --check
```

No benchmark rerun is authorized or performed for this round. No workspace
tests/Clippy, native, GUI/TUI or frontend integration check ran locally.
The prior independent round-2 review measured warm medians of
**18.896 ms** at `0c0cfc8` and **18.649 ms** at `f3242e7`, resolving the earlier
first-access concern. Those remain historical evidence, not measurements of this
implementation. Hosted CI, independent re-review and frontend integration remain
with the parent; no acceptance gate is waived.

### Verification follow-up: case-folding mutant anchor

The next review found that `fail-first.ps1` still looked for the old
`.case_insensitive(true)` constructor call. Verification-only commit
`ce405412f30574010c5c4a39881bc54cbd0c219f` repairs the anchor to
`.case_insensitive(options.case_insensitive)`, replacing it with `false` to
disable folding at the three current builder sites. Production source is unchanged.

On 2026-10-23 at 05:17 JST, the parent authorized exactly two serial runs after
the frontend's lane release. The actual named mutant was read from the harness's
constant PowerShell `HashtableAst` via `SafeGetValue`, applied in this isolated
worktree, and restored byte-for-byte in `finally`:

- Mutant: `case_word_selection_and_invalid_bytes` compiled, then failed its
  intended assertion: actual offsets `[0, 35]`, expected `[0, 4, 35]` (the
  uppercase match was missing). Exactly one test ran; PID 25456 exited **101**.
- Restored: the same exact test passed **1/1**; PID 38560 exited **0**. The
  restored source hash matches the validated implementation above, and Git
  confirms no production source diff.
- Each Cargo process had a 60-second timeout. Both used `--offline --locked -j 4`
  and the configured shared target, without invoking the legacy verification
  wrapper's private target override. Dependencies were reused; only core-search
  recompiled (1.23 s for the mutant, 0.86 s restored).
- Logs: `%TEMP%/kwil-search-casefold-86146cf7/{mutant,restored}.log`; each first
  line names the commit, phase, harness/source hashes, timestamp and matched-site
  count. Both processes exited before direct lane handoff to the named backend job.

```text
cargo test -p kwil-core-search --lib --offline --locked -j 4 -- --exact tests::case_word_selection_and_invalid_bytes --test-threads=1
```

No broader test or mutation selection, benchmark, Clippy, native or frontend
run was repeated for this anchor-only follow-up. The previous 20-test and lint
evidence remains unchanged; parent review and hosted CI remain separate gates.

## Fix round 2: extended-mode regex comments

Base: `f3242e7c7e04241e9b26555975108703a720edcd`.
Round 2 review found that whole-word raw string wrapping rejects the otherwise
valid `(?x)ab #comment`: the trailing comment consumes the wrapper's closing
syntax. Whole-word boundaries now wrap serialized parsed AST, whose printer
removes comments while retaining capture syntax and inline flags. HIR remains
the source of LF-consumption analysis. HIR serialization was tested but would
renumber captures after simplifying a zero-count repetition: `(a){0}(b)` would
make `b` capture 1 instead of capture 2. The regression also caught that defect
before switching to AST serialization. An explicit `(?-i:...)` still overrides
the caller's case-insensitive option.

`whole_word_extended_comments_preserve_flags_and_captures` failed before the fix
with `error: unclosed group`. The same test failed against a deliberately broken
copy restoring raw wrapping (`review-fail-first.ps1 -TestName
whole_word_extended_comments_preserve_flags_and_captures`); the script restored
the implementation byte-for-byte. The restored crate suite passed **18/18**,
covering alternatives, numbered/named replacements, inline/caller case flags,
and every earlier read-error, snapshot, UTF-16, CRLF and streaming regression.
Workspace Clippy (`--all-targets --locked -j 4 -- -D warnings`), fmt and diff
checks passed with incremental cache reuse. No literal search hot-path change,
new performance run, full local suite or native/UI launch occurred in this round.
Re-review and new-head hosted CI remain with the parent.

## Fix round 1

Started at PR head `0c0cfc872ace55ed779db7870a8acb3d62fc366c` on local
`fix-22`; merged `main` at `419823207a8fb31f9e09c21ccdc69debc02e62e3`.
The only merge conflict was `Cargo.lock`; Cargo regenerated it offline from
main's lockfile and the combined manifests. No new dependency was added.

- `Haystack::chunks` now reports iterator-construction and individual read
  errors. Search windows, parallel partitioning, line filters and replacement
  planning propagate them. The `TextFile` adapter preserves change checks;
  the new `Snapshot` adapter searches non-contiguous pieces without flattening.
- Whole-word regexes include Unicode word-boundary assertions in the compiled
  expression, so `a|ab` can try `ab` when `a` fails its trailing assertion.
  Non-capturing wrapping preserves `$1` and named capture numbering. Literals
  retain their fast path and compare Unicode word status on both sides of each
  endpoint, matching the same `\b` semantics even for punctuation.
- Find-in-files recognizes UTF-16 LE/BE BOMs before binary detection. It decodes
  one bounded line at a time and streams hits with original-file byte offsets
  (including the BOM in absolute offsets). The preview is UTF-8; match ranges
  are original bytes relative to the original line start. A malformed surrogate
  becomes one replacement scalar covering one original code unit. Binary files
  without a text BOM still follow grep's NUL detection.
- Ranges are recomputed on original records, including CR in CRLF; only the
  preview loses the terminator. LF is the record separator and cannot be matched.
  A standalone trailing CR stays visible. UTF-16 byte-mode partial-scalar regex
  hits cover that scalar's original code units. Line-buffer overflow emits an
  error; decoding, range output and previews retain fixed caps and cancellation.

**Fail first:** before the regex/encoding/CRLF fixes, the new tests failed with
`a|ab` returning only the standalone `a`, UTF-16 returning zero of four matching
lines, and `\r` returning an empty range list. After implementation,
`review-fail-first.ps1` verified all six new tests against deliberately broken
copies, restoring original bytes in `finally`. Every mutant ran one test and
reported `test result: FAILED` (the PowerShell wrapper returned exit 1).

| New test | Deliberate defect detected |
|---|---|
| whole_word_alternatives_captures_and_literal_boundaries | Remove compiled boundary constraints |
| piece_snapshot_chunks_search_and_replace_stay_immutable | Stop advancing piece offsets |
| haystack_read_errors_reach_search_filter_and_replace | Swallow a replacement read error |
| changed_file_adapter_reports_errors_for_file_and_snapshot | Bypass TextFile's read/change gate |
| files_utf16_boms_keep_original_offsets_and_skip_binary | Return decoded UTF-8 match offsets |
| files_crlf_ranges_include_matching_terminator | Match only the trimmed preview |

Restored focused tests: **17 passed**, including all original tests, tiny piece
boundaries, immutable snapshots, initial/chunk read errors, changed-file errors,
UTF-16 LE/BE, supplementary scalars, malformed surrogates, original byte ranges,
CRLF, preview truncation, bounded-line errors and UTF-16 cancellation.

Implementation commit: `4513c5e0bb84f1441ffd6eac603e4e97e027f107`.
Requested workspace Clippy (`--all-targets --locked -j 4 -- -D warnings`),
format check and `git diff --check` passed. Clippy initially rejected three
single-range test assertions; these were rewritten and Clippy then passed.
No full workspace test suite was run locally.

The Haystack API change touches the literal path, so the authorized **one**
default-worker remeasurement ran on this exact committed implementation:

```powershell
./crates/core-search/verify.ps1 -CargoArgs @('run','--release','-p','kwil-core-search','--example','count_fixture','-j','4','--locked','--','E:/Development/kwil/fixtures/log-1g.log','--default-only') -TimeoutSeconds 240
```

Existing fixture: **1,073,741,835 bytes**. Four search workers; one pass per query,
in the order shown; no warm-up, cache flush, repeated sample or fixture generation.
The release build incrementally recompiled core-text and core-search, reusing
the existing shared target cache (8.07 s).

| Query | Count | Four workers |
|---|---:|---:|
| Literal rare `line=20353525` | 1 | **1,549.012 ms** |
| Literal common `INFO` | 5,090,586 | **38.539 ms** |
| Regex `ERROR line=[0-9]{1,8}` | 5,086,074 | **169.578 ms** |

**The rare literal misses both the 300 ms target and 1 s hard limit in this
run.** The common literal meets both. Counts agree with the original evidence.
The rare query was first on the new mapping and includes its first access;
page-cache state was uncontrolled. This does not isolate a cause or prove an
in-memory regression. The earlier 22/37 ms result below remains historical,
not a passing measurement for this commit. No second timing was taken under
the one-run budget. Parent owns any decision about further measurement and
performance acceptance, plus final-head hosted CI and independent review.

## Regex match-bound limitation

The in-buffer regex engine requires every possible match to fit the declared
`max_match_bytes`. A match longer than this bound may be **missed silently across
window borders**: for example, `x{200}` with a bound of 2 can return no match
without an error. Only overlong matches that the windows actually expose can
produce a bound error. Callers must not assume every violation errors, even
when a small-input probe does error. This limitation also applies to line filters
using the bounded window engine. Prefer the line-oriented `find_in_files` mode
where completeness matters: it searches complete lines and reports an error if
a line exceeds its configured heap cap. It does not support multiline matches.

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
