# core-search

Worker-side engines for KW-22, KW-23 and the KW-41 line filter. No UI, runtime,
file mutation, persistent background thread, or file-sized working buffer.

## Integration contract

- `Haystack` borrows an immutable byte snapshot, returning chunks with absolute
  `u64` offsets covering the requested range. `TextFile`, byte slices and the
  piece-tree `Snapshot` have implementations here without flattening pieces.
  Both opening the chunk iterator and reading individual chunks may fail; all
  consumers propagate these errors. `TextFile` retains responsibility for mapping
  lifetime and external-change safety; this crate does not add a watcher.
- `search` emits ordered, non-overlapping `(offset, len)` hits (16 bytes each).
  Count mode retains no hits. The caller owns the worker and cancellation token,
  and should coalesce progress into its event loop's status updates. Collectors
  must cap or spool hits; signal-strip consumers can aggregate them by viewport
  bin instead of retaining millions of individual markers.
- **Shipped editor default:** `search_default` uses half the logical CPUs capped
  at four, leaving headroom for the UI. `search_with_threads` permits a lower
  admitted count; `search` remains the serial worker primitive. Regex HIR proves
  whether a pattern can consume LF. Only LF-free patterns with nearby newline
  shard boundaries run in parallel, preserving non-overlap/anchor semantics;
  multiline patterns, giant unbroken lines and small ranges stay serial.
  Count progress merges as workers advance. Hits retain order through bounded
  256-hit batches; later shards backpressure instead of storing out-of-order hits.
  The returned summary reports the actual worker count. One light coordinator
  drains channels in addition to the reported search workers.
- Literal case-sensitive search uses `memmem`; case-insensitive text literals
  use Unicode regex folding, with ASCII folding for non-UTF-8 byte patterns.
  Whole word uses Unicode `\b` at both ends, treating invalid bytes as non-word.
  Regexes compile these constraints into the expression, preserving alternatives
  and capture numbering. Offsets are original bytes; in-buffer encoding decode
  and offset mapping are separate adapters.
- Regex search requires a declared maximum possible match length, at most 1 MiB
  (64 KiB is a suggested UI default). Its overlap retains this bound plus Unicode
  boundary context. Expressions like `.*` require an input-specific guarantee;
  arbitrary-length multiline regex is outside this contract. Detected overlong
  matches return an error, but overlong matches may be missed silently across
  window borders. Callers cannot assume every bound violation errors. Prefer
  line-oriented `find_in_files` where completeness matters within a line.
  The selection restricts hits, preserving document
  context for anchors and whole words. Memory is O(1 MiB + declared match bound).
  Main chunk bodies are borrowed; only overlap tails and prefixes are copied.
  Parallel workers each keep the same bounded working set.
- `plan_replace` accepts ordered matches from the same source/pattern and emits
  ordered edits. `$1`, `${name}`, `$0`, `$$`, missing and optional captures follow
  `regex` interpolation. Literal replacements preserve dollar signs, including
  case-insensitive literals. The editor groups this stream into one undo step;
  it may spool edits or admit a capped Vec. Templates are capped at 1 MiB and
  each expanded edit at 8 MiB, checked before allocation growth.
- `LineFilter` applies an AND/OR/NOT expression to indexed patterns on each line.
  Lines are virtual haystacks: even giant lines are scanned in bounded windows.
  Runs use one-based, half-open line numbers. `LineRuns` optionally collects runs
  under an explicit capacity, returning an error rather than growing unchecked.
  With `include_unterminated=false`, the resume cursor stays at the incomplete
  final line, so append processing revisits only that line and new bytes.
  A static view may include that final line provisionally; the watcher replaces
  its provisional result on append and resets the cursor on rotation/truncation.
- `find_in_files` uses ripgrep's engine with `.gitignore` and root-relative
  include/exclude globs (exclude wins; include does not bypass ignore). Workers
  default to half the logical CPUs, capped at four, and caller overrides cannot
  exceed this. Each worker has a default 2 MiB grep buffer (configurable 64 KiB
  to 8 MiB); longer lines emit an explicit error. Results have capped previews
  and match ranges; ranges beyond the preview remain full-line-relative.
  UTF-16 LE/BE BOMs select a bounded line decoder before binary detection;
  other files retain grep's NUL detection. UTF-16 previews are UTF-8, while
  line offsets and match ranges refer to original file bytes, including the BOM.
  Ranges include a matching CR in CRLF; previews omit CRLF only. LF separates
  records and cannot be matched. UTF-16 lines use at most heap_limit/8 code
  units, allowing both raw units and decoded text within the worker budget.
  Different files interleave, while each file's results remain ordered.
  Supply a **bounded `sync_channel`**, consume on the UI side, and set the token
  or drop the receiver when superseding a query. Worker-only backpressure waits
  check cancellation every 5 ms; no UI timer or polling is required. Reads also
  check cancellation, including files with no matches (OS blocking I/O itself
  cannot be interrupted by the token).

## Verification

`verify.ps1` launches cargo with a recorded PID, timeout, private target directory
and four compile jobs. `fail-first.ps1` mutates this package only, checks each
acceptance test fails, and restores byte-identical originals in `finally`.
The fixture measurement example runs each query/configuration exactly once,
at one worker and at the editor default, without a warm-up or benchmark loop:

```powershell
./crates/core-search/verify.ps1 -CargoArgs test,-p,kwil-core-search,-j,4,--locked
./crates/core-search/fail-first.ps1
./crates/core-search/review-fail-first.ps1
./crates/core-search/verify.ps1 -CargoArgs @('run','--release','-j','4','--manifest-path','spike/textbench/Cargo.toml','--','gen','fixtures/log-1g.log','1024') -TimeoutSeconds 240
./crates/core-search/verify.ps1 -CargoArgs @('run','--release','-p','kwil-core-search','--example','count_fixture','-j','4','--locked','--','fixtures/log-1g.log') -TimeoutSeconds 240
```

Unit tests lower the work-window size to 128 bytes to exercise actual overlap
ownership, in addition to haystack chunks as small as one byte. Each filesystem
test owns a unique temporary directory. The real measurement uses 1 MiB windows.
Pass `--default-only` after the fixture path for one pass at the default worker
count only, as used for fix round 1.

## Dependencies

Checked against the crates.io API on 2026-10-23; current stable versions:

| Dependency | Version | Release date | Purpose | License |
|---|---|---|---|---|
| memchr | 2.8.3 | 2026-07-08 | SIMD literal/line scans (already in workspace) | MIT or Unlicense |
| regex | 1.13.1 | 2026-07-15 | Byte regex, captures, Unicode | MIT or Apache-2.0 |
| regex-syntax | 0.8.11 | 2026-06-09 | Prove LF-free shard safety (already transitive) | MIT or Apache-2.0 |
| ignore | 0.4.33 | 2026-08-04 | Ignore-aware capped parallel walking | MIT or Unlicense |
| grep-searcher | 0.1.17 | 2026-07-15 | Streaming file reader and line search | MIT or Unlicense |
| grep-regex | 0.1.14 | 2025-10-16 | Ripgrep regex matcher | MIT or Unlicense |
| grep-matcher | 0.1.9 | 2026-07-15 | Access per-line match ranges | MIT or Unlicense |
| globset | 0.4.20 | 2026-08-04 | Include/exclude without overriding ignore | MIT or Unlicense |

No new async runtime or derive/proc-macro dependencies. The lockfile adds 16
packages to the existing graph. First cold dependency check took about 5 s;
first focused test compile took 15.06 s at `-j 4` (owner PC, warm registry).
