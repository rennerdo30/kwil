# core-syntax (KW-26)

Highlighting without drawing or a dependency on `core-text`. The editor submits an
immutable snapshot and a visible byte range to `Worker`. Its wake callback posts an
event; `take_update()` delivers the latest result, and `previous()` retains the last
successful highlights while new work runs. `Span::range` contains **absolute
document byte offsets**, clipped to the requested range; spans are sorted and
disjoint. `Class::name()` is the stable theme scope name for KW-37.

## Integration

Implement `TextSource::len()` and `chunk(offset, max_len)` for a mapped file or a
piece-tree snapshot. Chunks start exactly at the requested offset and contain at
most `max_len` bytes. Borrowed chunks avoid allocations; positioned reads can
return owned chunks. Before EOF, an empty chunk is an I/O error. Snapshots must
remain immutable, and reads must be bounded. The engine never assembles a complete
document buffer. A piece-tree adapter can return the first chunk of
`snapshot.chunks(offset..min(offset + max_len, len))`.

`Request` carries the snapshot revision, the visible range, and an optional
`InputEdit` against `previous_revision` (points use UTF-8 byte columns). A matching
base edits the retained tree with `Tree::edit`, then passes it as `old_tree` to
`Parser::parse_with_options`. A skipped revision safely triggers a fresh parse.
Another viewport can reuse an unchanged revision without parsing. The revision
must uniquely identify a document snapshot; do not reuse it for different bytes.

The mailbox keeps one pending request, eight retired requests and one result.
Submitting replaces pending work and cancels the active generation. The worker
drains retired snapshots outside the mailbox lock, including on cancel and stop.
It can hold an additional batch of eight snapshots while draining, plus its
current request: at most 18 accepted snapshot references across worker and mailbox.
No snapshot or parser-tree destruction runs on the submitter.

If the retire slots fill while the worker is busy, `submit()` returns a boxed
`SubmitError { request, error: Error::Busy }` without cancelling accepted work.
The caller retains ownership of that request for retry on a later worker event;
it must retain the snapshot until it can hand it off, rather than dropping its
last reference on the UI thread. Closed submissions likewise return ownership.
This explicit backpressure keeps both submission time and storage bounded; a
consumer must handle rejection instead of blindly unwrapping submission results.
Parser progress callbacks, source reads,
query progress callbacks and lexer steps check cancellation; stale results are
discarded. Idle workers wait on a condition variable, with no periodic timer.
`wake` must only schedule an event, never block. `Worker::drop` signals cancellation
without joining; `shutdown()` joins off the UI thread. **Parser trees and old-tree
disposal belong to the worker.** `Engine` is the synchronous, worker-owned API for
headless consumers; do not run it or destroy it on the editor UI thread.

## Range queries and bounds

We use upstream tree-sitter's query API rather than `tree-sitter-highlight`:
`QueryCursor::set_byte_range` visits only the viewport plus a byte margin while
retaining the incremental tree. This avoids the highlight library's full-source
slice/highlight-stream interface and allows a chunked `TextProvider`. Capture
overlaps are flattened; later query patterns win, with narrower captures breaking
ties. Standard capture names map to the sixteen fixed classes. Numeral nodes
captured as `constant.builtin` are mapped to `number`; syntax errors to `error`.

Default `Config` bounds:

- Tree-sitter input: at most **20 MiB**, configurable. Inputs at or above the
  tree-sitter u32 offset limit always use the lexer. The AST below this input cap
  still has grammar-dependent memory cost; it is not a hard AST-memory quota.
- Margin: **4 KiB** on each side; viewport plus margin admitted up to **256 KiB**.
  Split huge lines/requests at the caller; oversized windows return `InvalidRange`.
- Query predicate text: at most **64 KiB per capture**, read in 16 KiB chunks.
  Predicates on longer tokens see a truncated prefix, so classification can be
  approximate. Query match state and output capture counts are capped; exhaustion
  returns `QueryLimit`, retaining the worker's last successful highlights.
- Above the parse cap, or with a disabled grammar, only the admitted window is
  copied and lexed. Keyword/comment tables cover all listed languages, along with
  strings and numbers. This deliberately approximate lexer starts in neutral
  state at the margin: strings/comments opened farther back, nested delimiters,
  heredocs and language-specific escapes can be misclassified. It never scans
  from the beginning of a huge file to recover state.

Base-language queries are supported. Recursive language injections, local-variable
resolution and embedded Markdown inline/code grammars are not implemented here.
The Markdown block grammar supplies headings, reference links and code fences;
KW-27 code-block `raw` text can be submitted separately, with offsets relative to
that raw source (not to its containing Markdown block).

## Languages, versions and licences

Checked against the crates.io API on **2026-10-23**:

- [tree-sitter 0.27.1](https://crates.io/crates/tree-sitter/0.27.1), released
  2026-10-08, MIT. Query support includes its regex/streaming-iterator dependencies.
- [Arborium 2.18.2](https://crates.io/crates/arborium/2.18.2), released
  2026-08-28, umbrella MIT OR Apache-2.0. We depend on individual grammar crates,
  whose `LanguageFn` exports work with upstream tree-sitter 0.27.1. No umbrella
  highlighter, themes, patched runtime or GPL feature group enters the graph.
- Dev-only [memmap2 0.9.12](https://crates.io/crates/memmap2/0.9.12), MIT OR
  Apache-2.0, serves borrowed mapped chunks in the retained measurement example.

`default = ["starter"]` enables these **32** features, selecting individual
`arborium-<language>` 2.18.2 C grammars except for official `tree-sitter-rust`
0.24.2 behind the unchanged `lang-rust` feature:

```text
lang-c          lang-cpp         lang-c-sharp    lang-css
lang-go         lang-html        lang-java       lang-javascript
lang-json       lang-kotlin      lang-lua        lang-markdown
lang-php        lang-python      lang-ruby       lang-rust
lang-bash       lang-sql         lang-swift      lang-toml
lang-typescript lang-tsx         lang-xml        lang-yaml
lang-dockerfile lang-make        lang-ini        lang-diff
lang-batch      lang-powershell  lang-haskell    lang-scala
```

The selected grammar manifests are **MIT**, except
[`arborium-ini`](https://crates.io/crates/arborium-ini/2.18.2), **Apache-2.0**.
Transitive `arborium-jsdoc` and WASM-only `arborium-sysroot` are MIT. No GPL grammar
is selected. Kotlin's highlight query separately carries Apache-2.0 provenance.
[LICENSES.md](LICENSES.md) records every grammar source/version/commit, the
verification method, tarball and licence hashes, Rust source equality, and Ruby's
blank upstream pin. [LICENSES.txt](LICENSES.txt) retains the licence texts.
Disabling defaults and enabling `lang-rust` builds only the official Rust
grammar; detection and lexical tables remain available for every language.

`detect(path, first_line)` checks filenames, extensions, shebang interpreters and
XML/HTML/diff first-line markers, inspecting at most 512 bytes. Filenames and
extensions take precedence over content. It recognises special names including
Dockerfile, Containerfile, Makefile, Gemfile, Rakefile and Cargo.lock.

`GrammarProvider` is the prebuilt-library seam. A future loader can return an
ABI-compatible `Language` and owned highlight query; it must retain its library
handles through engine destruction. Provider handles drop after parser/query/tree
state. Loading, downloading and publishing grammar libraries are a later step.

## Verification and measurements

On the owner Windows PC, Rust 1.99.1, `-j 4`, 2026-10-23 JST. These are one-shot
samples, not percentile claims or native UI keystroke-to-pixel acceptance.
These original measurements used arborium-rust 2.18.2. Round 1 replaces that
grammar for verifiable provenance; the figures are historical, and were not
remeasured for the official Rust grammar (the fix brief forbids new benchmarks).

Clean **dev** crate builds, separate fresh target directories, dependencies already
downloaded (no cache deletion): **29.07 s** default 32-language set; **12.90 s** with
`--no-default-features --features lang-rust`. Cargo timings are retained under
`E:/Development/_cache/cargo-target/kwil-kw-26-syntax-clean-{default,minimal}-20261023/cargo-timings/`.
These include dependency/C grammar compilation; measurement instrumentation is off.

Initial release sample, using positioned disk reads: **160.428 ms** for an edit and
60-line viewport in a file with 50,000 one-line top-level Rust functions. The first
lexical sample was **54.075 ms**, but included disposal of that previous parse
tree, so it was not an isolated lexer measurement.

One corrected run was explicitly authorized by the coordinator. Rust snapshots
used borrowed mmap chunks; the lexical sample used its own fresh engine:

| Work | 50k one-line top-level functions | Normal 5k lines (500 ten-line functions) |
|---|---:|---:|
| First full parse | 279.479 ms | 14.332 ms |
| Apply `Tree::edit` | 0.004 ms | 0.003 ms |
| Incremental reparse with edited old tree | 129.329 ms | 0.207 ms |
| Query 60 visible lines + margin | 1.131 ms | 0.600 ms |
| Dispose old tree on worker | 19.813 ms | 0.019 ms |
| Total edit request | 150.277 ms | 0.830 ms |

The 50k-top-level case is a **known worst case**, above one frame. It still uses
the edited old tree and a range-restricted query. The parse callback served
2,273,774 bytes versus 37,480 for normal source, showing broad parser work despite
incremental reuse; wide top-level sibling structure is a likely contributor, not
a proven root-cause diagnosis. The UI keeps its previous highlights throughout.
The coordinator accepted this limitation; no further benchmark runs were made.

The corrected 1 GB log fallback sample queried **60 lines in 0.200 ms**, reading
exactly **11,312 bytes** including the margin from a **1,073,741,835-byte** fixture.
No full-file buffer, mapping or scan was needed for that highlighter request.

Thirteen focused contracts cover Rust/Python/JSON/Markdown byte ranges, detection,
incremental changed-region equality with a fresh parse, fragmented chunks, clipping,
fallback tables, huge-source read bounds, all bundled query compilation,
cancellation, coalescing/previous-result retention, and rejected inputs. Every
contract failed under deliberately broken implementation copies and passed after
restoration. A separate cancellation mutation emitted a stale wake and failed the
gated worker contract. `tools/fail-first.ps1` preserves the mutations and always
restores source in `finally`.

Round 1 adds destructor-recording tests for submit, cancel, drop and orderly
shutdown, including unlocked destruction and bounded retirement with returned
rejected-request ownership. A separate SQL test checks integer and decimal
numbers plus a string. `tools/round1-fail-first.ps1` restores deliberate broken
copies in `finally`: all four destruction paths fail on submitter destruction,
the queue bound fails when raised to 64, and independently breaking each SQL
predicate fails at the integer or decimal assertion respectively.

The query audit scanned all packaged query files for the 32 selected grammars
plus JSDoc (including inherited query sources). Only SQL 2.18.2 highlights lines
38 and 41 contain Lua-pattern predicates: `^[-+]?%d+$` and
`^[-+]?%d*\.%d*$`. `Language::bundled()` patches these two patterns to ASCII digit
classes, with a double-escaped literal decimal point in the SCM string, before
query compilation. Other percent signs are operator tokens or Ruby string
delimiters, and Kotlin mentions removed `#lua-match?` predicates in a comment.
Provider queries are unchanged; percent signs are not rewritten globally.

Reproduce checks from the repository root (PowerShell 7):

```powershell
& crates/core-syntax/tools/run-cargo.ps1 -CargoArgs @('test','-p','kwil-core-syntax','-j','4','--locked')
& crates/core-syntax/tools/fail-first.ps1
& crates/core-syntax/tools/round1-fail-first.ps1
& crates/core-syntax/tools/run-cargo.ps1 -CargoArgs @('test','-p','kwil-core-syntax','--no-default-features','--features','lang-rust','-j','4','--locked')
```

The runner sets the process-only target directory, records its cargo PID and uses
a timeout. To reproduce timings, use fresh owned target directories with
`build -p kwil-core-syntax -j 4 --locked --timings`, optionally adding
`--no-default-features --features lang-rust`. Generate the ignored 1 GB fixture
with the brief's `spike/textbench` generator. The retained one-shot measurement is
`cargo run --release -p kwil-core-syntax --example measure --no-default-features
--features lang-rust,measure -j 4 --locked`; set `CARGO_TARGET_DIR` to the owned
cache directory and `KWIL_NO_UPDATE=1` before release runs. `measure` gates all
timing instrumentation out of production builds.

Not run locally: workspace full test suites, Linux/macOS execution, GUI/TUI typing
or drawing, native keystroke latency, prebuilt library loading and recursive
injections. Cross-platform full suites belong to the PR CI/coordinator.
