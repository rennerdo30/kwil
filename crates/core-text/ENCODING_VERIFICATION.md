# KW-10 encoding verification

Historical KW-10 implementation `45c20bb`; integrated main `4198232` in `c98304f`.
Starting base: `778e875098a04badc3c4e3d14afefecd5cf5301c`.

## API contract

- `EncodedFile::new` detects from first/last at most 64 KiB. Detection is a
  heuristic; `reinterpret` is the explicit user override.
- `EncodedFile::new_with_encoding` accepts an explicit choice before automatic
  sniffing, so callers can recover with a retained TextFile after detection
  fails. For example, `FF FE 00 00` is ambiguous between UTF-32LE and a UTF-16LE
  BOM followed by U+0000; explicitly choosing UTF-16LE opens it correctly.
- `decode_range` takes original **raw byte offsets**, including the BOM. It
  returns the actual character-aligned source range and display segments.
  UTF-8 text is borrowed; invalid byte segments retain the original bytes.
  Requested and aligned ranges are capped at 1 MiB. Giant lines need column
  ranges. `decode_lines` includes line endings in the returned text.
- Build `DecodeIndex` on a worker with `index_in_background`; its callback
  wakes the frontend once. Before readiness, first-screen decode works, and
  distant requests return `IndexNotReady`. No polling is required.
- Checkpoints are neutral decoder states every approximately 64 KiB, carrying
  line rank and current line start. UTF-16 breaks are LF/CR **code units**, not
  raw `0A` bytes; CRLF is one break. Line lookup also skips giant lines using
  byte-rank checkpoints. Do not use `Snapshot`'s raw-byte line metrics as
  UTF-16 user-visible line numbers.
- `Snapshot::source_chunks` retains Original vs Added provenance without copying.
  Original offsets remain source byte offsets; Added chunks contain UTF-8 and
  may split a UTF-8 scalar across adjacent chunks. Saves join that scalar with
  at most three pending bytes. Byte-preserving non-UTF-8 saves reject edits
  inside original encoded characters with `SplitCharacter`; use the decoded
  range's boundaries. During conversion, logically adjacent original pieces
  share decoder state even when their source offsets are discontiguous. They
  can form a complete character after deleting intervening bytes. The decoder
  finalizes at an actual transition to added UTF-8 or EOF, and incomplete data
  still returns `MalformedSource` before touching the output writer.
- `save_plan` preserves encoding and BOM. `convert_to` changes the next save
  target. `change_line_endings` normalizes the next save; the streamed
  `plan_line_ending_edits` API produces raw snapshot-byte replacements with
  UTF-8 replacement text. Apply ascending replacements with a byte delta in
  one Buffer transaction, or spool and apply in reverse. The undo test proves
  one transaction restores the mixed original/added representation.
- `write_to` preflights the immutable snapshot, then streams to the writer.
  An unmappable character leaves the writer untouched. Original pieces copy
  verbatim when possible, including invalid bytes and mixed line endings.
  Same-encoding ending normalization changes only newline bytes; explicit
  encoding conversion refuses malformed input rather than replacing it.
  Atomic destination replacement, fsync and attributes remain KW-19.
- **Known v1 limitation (KW-45): ISO-2022-JP is unsupported.** Opening a detected
  ISO-2022-JP file or choosing its label returns
  `ISO-2022-JP is not supported yet (known limitation, KW-45)`. The fixture test
  asserts this rather than accepting garbled ASCII/UTF-8.

## KW-64 UTF-32 candidate

UTF-32LE and UTF-32BE now share the bounded decoder, import, save and provenance
paths. Four-byte BOMs take precedence over UTF-16 detection; an explicit UTF-16
choice still strips only its two-byte BOM. Explicit endian labels support
BOM-less files; bare `utf-32` remains ambiguous and is refused.

Invalid complete units retain their four raw bytes as opaque atoms; an incomplete
final unit retains its one to three bytes. Artificial read boundaries retain
pending input. Matching-encoding saves preserve those raw bytes. Cross-encoding
conversion refuses malformed input before writing. Atomic and gzip saves,
compare inputs, capsules and authenticated journals use the same codec identity.

ROOT's frozen R4/R5 receipts record 34 focused UTF-32 tests, 26 existing encoding
tests, and all-target strict checks for core-text, core-workspace and view-model
passing. Independent review and integration are pending. These results do not
qualify all-size resident indexes, encoding/EOL commands, native frontends,
performance, or full v1. Earlier measurements below remain historical evidence.

## Verification

On Windows, integrated commit `c98304f`:

```powershell
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-10-encodings'
cargo test -p kwil-core-text -j 4 --locked -- --test-threads=4
cargo clippy --workspace --all-targets -j 4 --locked -- -D warnings
cargo fmt --all --check
```

Results: **56 passed, 1 ignored** (the existing separate piece-tree 1 GB
measurement); clippy and fmt passed. Full workspace test suites run on CI.

There are 24 new encoding tests, with fixtures generated in unique test
directories under ignored `fixtures/`, using PID plus a per-process counter.
They cover no-edit/edited round trips for UTF-8, UTF-8 BOM, invalid UTF-8,
UTF-16 LE/BE with/without BOM, Shift_JIS and Windows-1252; mixed endings;
checkpoint/range boundaries in nine encodings; split added UTF-8 chunks;
unmappable and malformed conversion input; undoable ending plans; file-change
refusals; giant-line request caps; and the KW-45 error.

`scripts/encoding-fail-first.ps1` reproduces the Windows broken-copy checks.
Run from the repository root. Fourteen behavioral mutations were tested on the
WIP implementation over base `778e875`; every one of the 24 new tests failed
under at least one mutation. The script restores the original implementation
in `finally`. It records owned cargo PIDs and bounds each process to 120 s.
Evidence: `E:/AgentData/scratch/kwil-kw10/failfirst-summary.txt` and adjacent logs.
The ISO-2022-JP file test additionally failed before fixing the real ASCII-only
misclassification, then passed with the KW-45 detection and error fix.

## One-shot measurement

Reproduce once on Windows:

```powershell
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-10-encodings'
$env:KWIL_NO_UPDATE='1'
cargo build -p kwil-core-text --example encoding_viewport --release -j 4 --locked
# Invoke the resulting example with a 120-second process timeout.
E:/Development/_cache/cargo-target/kwil-kw-10-encodings/release/examples/encoding_viewport.exe
```

Generated **1,073,741,824 bytes**, 64-byte Shift_JIS lines, on disk under ignored
`fixtures/`; file mapped, no Buffer or full decoded text created. A background
worker completed the sparse index before the one timed middle viewport.
The decoded viewport was checked against the fixture's exact expected text.
The owned measurement PID was **74552**, exit 0; the fixture was removed.

| Measurement | Result |
| --- | ---: |
| Open + bounded detection | 5.820 ms |
| Background encoding index | 1,632.424 ms |
| 60 middle lines including indexed lookup + decode | **0.236400 ms** |
| Source / decoded viewport bytes | 3,840 / 4,800 |
| Total lines / checkpoints | 16,777,217 / 16,384 |
| Sparse index allocation | **393,216 B (384 KiB)** |
| Process private bytes before / after | 733,184 / **3,670,016 (3.50 MiB)** |

Private memory is Windows `PROCESS_MEMORY_COUNTERS_EX.PrivateUsage`, excluding
mapped file/page cache. This is one shared-machine sample; workspace clippy
was compiling concurrently. The encoding index decodes into discarded 16 KiB
scratch; its 0.658 GB/s includes decoding, file metadata checks and line rank.
It is separate from the existing parallel raw-byte LineIndex throughput gate.
No cold-cache flush, low-RAM hardware, GUI/TUI integration, fuzz campaign or
additional measurement runs were performed.

Dependencies checked on crates.io on 2026-10-23: `encoding_rs 0.8.42`
(2026-09-24, Apache-2.0/MIT plus BSD-3-Clause), `chardetng 1.0.0`
(2026-03-30, Apache-2.0/MIT), `simdutf8 0.1.5` (2024-09-22, MIT/Apache-2.0).
New transitive `core_detect 1.0.0` and `multiversion_no_op 1.0.0` are permissive;
the latter is a small proc macro, with no new syn dependency. The first focused
debug test build took 3.59 s; the release example build took 4.78 s in this
branch-specific target directory. These are build observations, not isolated
per-dependency compile-cost estimates.

## Independent review fixes

Review of `5cfb3a5` found two P2 defects: explicit choices lost to the UTF-32
sniff, and conversion finalized a decoder at each original-piece boundary.
Two regressions first failed on that implementation with constructor API glue
added: `UnsupportedEncoding("UTF-32")` and `MalformedSource`, respectively.

The explicit constructor now honors the choice before sniffing. Conversion
retains decoder state across original pieces. Tests join UTF-8, Shift_JIS and
UTF-16 surrogate bytes after a deletion, and still require errors for an
incomplete source at EOF or an actual transition to added UTF-8.

The updated mutation script checks all **26 encoding tests** against **16
behavioral mutations**, including reintroducing each review defect. Every new
test fails in at least one broken copy; restored focused tests pass. Updated
logs are under `E:/AgentData/scratch/kwil-kw10/review-1/`; original measurement
and mutation receipts above remain historical evidence on their named commits.
The one-shot 1 GiB measurement was not repeated for these detection-override
and save-stream fixes.
