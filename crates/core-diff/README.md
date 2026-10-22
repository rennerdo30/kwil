# core-diff

`compare` takes immutable, indexable byte-line sources; it has no dependency on
`core-text` or a frontend. Keep both snapshots alive and unchanged while comparing
and pass those same snapshots to `refine`. All line indexes, alignment rows and
byte ranges are zero-based. Ranges are half-open.

Histogram is the default; Myers (with imara's runtime heuristics) is optional.
Changed hunks pair lines by ordinal position as `Modify`; any excess is `Delete`
or `Insert`. This is deterministic alignment, not a similarity-based line matcher.
Equal stretches occupy one run. `row(i)` resolves an individual row by binary
search. No per-character data is created for equal regions. `hunks` use the full,
uncollapsed row coordinate space; next/previous navigation is strict, does not
wrap, and returns hunk starts. `collapsed(context)` returns compact visible row
ranges and collapsed ranges; overlapping context is merged. An entirely equal
comparison collapses into one range.

Whitespace options treat horizontal ASCII whitespace (space, tab, vertical tab,
form feed) as whitespace. `All` removes it, `Trailing` removes it only at the end
of line content, and `Amount` maps each nonempty run to one space (it does not
make absent whitespace equal to present whitespace). Line terminators remain
significant independently of whitespace options. `ignore_line_endings` equates
CRLF with LF, retaining the distinction between a final newline and none. Input
sources may omit all line terminators, but then cannot report EOL differences.
Bytes, including invalid UTF-8, are preserved; normalization borrows slices and
streams normalized bytes into hashing/equality instead of copying line contents.

## Bounded worker work

Admission ceilings are fixed, regardless of input file size:

- 262,144 total lines across both sources (`Options::max_lines` can lower this).
- 1 MiB per line and 64 MiB total bytes visited across both sources.
- Inputs above any ceiling return `DiffError::TooLarge`; line-count overflow and
  count limits are checked before reading lines or allocating token storage.
- imara's global diff requires per-line integer tokens and borrowed interner
  entries. These metadata allocations grow only within the fixed line ceiling;
  neither input is loaded/copied into a file-sized byte buffer. The result has at
  most a bounded number of runs/hunks. This is a capped global diff, not a streaming
  diff for arbitrarily many lines. Callers must surface `TooLarge` to the user.
- Word refinement is **on demand for visible modified rows**, never eagerly for
  all changes. Skip lines over 4,096 bytes and hunks with over 64 lines on either
  side. `similar` receives byte slices and a 10 ms algorithm timeout; returned
  ranges refer to original bytes. A timeout may yield a coarser valid refinement.
  Do not retain an unbounded cache of returned refinements in the frontend.
- Cancellation is checked before admission, between line reads, before and after
  imara, between output hunks, and between word changes. imara 0.2.0 exposes no
  cancellation hook inside `Diff::compute`; cancellation during that call is
  observed immediately after it returns. One line's hashing/equality also runs
  uninterrupted, bounded by the single-line ceiling. Run this API on a worker.

## Evidence and reproduction

Current stable dependencies checked through the crates.io API on 2026-10-23:
`imara-diff 0.2.0` (released 2025-06-14, Apache-2.0), `similar 3.2.0`
(released 2026-08-17, Apache-2.0). `similar` enables only text/byte support and
adds `bstr 1.13.1` (MIT/Apache-2.0). No UI or async dependencies. The first
focused debug build compiled the new diff stack in 9.7 seconds including tests;
the isolated release example build took 8.73 seconds with four compile jobs.

Seven acceptance tests passed. `tests/fail-first.ps1` temporarily breaks one
behavior at a time, requires the corresponding acceptance test to fail with an
assertion, and restores the original source in `finally`. All seven mutations
were detected on the initial implementation; no compile error is counted as a
failing acceptance test. The script is a developer check, not part of normal CI.

One release measurement on the owner's Windows PC: **94.067 ms**, histogram,
100,000 lines per file, 1,000 modified lines (1%), 1,000 hunks and 100,000 alignment
rows. This meets the 100 ms target and 500 ms hard limit. Generation and loading
of the measurement-only fixtures are outside the timer; tokenization, line diff
and alignment construction are inside. On-demand word refinement is outside the
timer. This is one observation, not a distribution or a cross-platform result.

```powershell
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-28-diff'
cargo test -p kwil-core-diff -j 4 --locked
# On an idle machine, explicitly authorized single measurement only:
cargo build -p kwil-core-diff --example measure --release -j 4 --locked
# Launch release/examples/measure.exe hidden, with a 30s timeout and KWIL_NO_UPDATE=1.
```

The example generates separate files under ignored `fixtures/diff-100k-<PID>`.
Its full-memory fixture loader is measurement code only; production comparisons
borrow their caller's snapshots. No GUI, frontend integration or native drawing
acceptance is claimed here.
