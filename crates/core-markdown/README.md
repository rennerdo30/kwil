# Markdown render model

`parse(&str)` produces a flat, owned preview model; there is no drawing, HTML
execution, image loading or syntax highlighting here. CommonMark plus tables,
task lists and strikethrough use `pulldown-cmark` 0.13.5 (MIT, released
2026-10-21, verified through the crates.io API on 2026-10-23). Default parser
features are disabled: no HTML renderer, CLI or SIMD dependency is required.
Footnotes and other optional extensions are off.

Blocks contain global, zero-based, half-open byte and line ranges. Byte starts
include the source line's container markers; ends follow the parser, which can
include a trailing blank line in a list item. Quote depth and list depth apply
to every block, including code and tables. A list item contains its first text
paragraph; subsequent paragraphs and nested items follow it in source order.
An item containing only a nested block can have no spans. Ordered numbers are
the displayed sequence from the list's starting number; bullet/delimiter
characters preserve the source marker.

Inline spans are flat runs with composable emphasis, strong, strike and link
styles. This avoids recursive inline trees and lets either frontend apply its
own styles. Inline code is a distinct kind. Links share their URL with `Arc`.
A paragraph containing only an image becomes an image block; images mixed with
text remain inline references, retaining their position. Alt text is decoded
plain text. HTML blocks and inline HTML remain inert text. Code holds the first
info-string word as its language and the parser's unhighlighted text (CommonMark
normalizes line endings and removes indentation/fences).

## Section parsing after an edit

Run this on a worker over an immutable UTF-8 snapshot, never the typing path:

1. Before the edit, call `block_boundaries(old_document, old_changed_lines)`.
2. After the edit, call `block_boundaries(new_document, new_changed_lines)`.
   The helper uses parser offsets to select complete top-level blocks and their
   immediate neighbours. Blank lines do **not** delimit fenced code, loose lists,
   quotes or HTML reliably. Unclosed fences can select the rest of the file.
3. Map the old selected byte range through the edit, union it with the new
   selected range, then expand to complete new top-level blocks again if the
   union cuts one. Replace all old model blocks intersecting that range, using
   `parse_section(new_document, new_range)` for the replacement. Shift surviving
   blocks' byte and line coordinates by the edit's deltas. Keep the previous
   model until the worker finishes; discard stale results by snapshot version.
4. If either snapshot has link reference definitions, reparse the entire
   admitted document: changes or removals can affect distant links/images.
   The helper returns the full range whenever definitions exist. Direct section
   parsing only resolves definitions present inside that section.

Boundary discovery is an O(document bytes) parser scan; it builds only bounded
root ranges, not render spans. This first implementation reduces model rebuilding,
not parser scan complexity. Source-line lookup uses sparse 4 KiB checkpoints,
not an entry per line. Section parsing accepts a borrowed range (no full source
copy), validates its bounds/UTF-8 boundaries and preserves document coordinates.
Callers should supply complete block ranges; arbitrary partial blocks are parsed
as standalone Markdown. Keep only the preview sections needed by the frontend.

The hard admission limit is 20 MiB of **document** bytes, checked before parsing,
even when requesting a small section. Larger files return `TooLargeForPreview`.
The caller can check its file size before mapping/decoding. A second cap of
1,000,000 parser events returns `ModelLimit` for unusually fragmented markup;
the partially built model is dropped. Both parser working storage and model
storage are thus limited to an admitted preview, never an unrestricted file.
These limits are preview policy, not a promise that the whole file fits in RAM.

`render_text(model, width)` is a plain-text fallback with heading, quote, task,
list and code markers. It preserves hard breaks and code spaces, wraps long
words/code safely, and treats zero width as one. Width counts Unicode scalar
values, **not** graphemes or terminal display cells; frontends should use spans
with their terminal's cell-aware layout for styled text. Plain rendering omits
link destinations and inline style delimiters. Tables keep cells/alignment in
the model; plain text emits pipe-separated rows and wraps narrow rows.

## Verification and one-shot measurement

Use the branch-specific target directory and `-j 4`:

```powershell
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-27-markdown'
cargo test -p kwil-core-markdown -j 4 --locked
cargo run -p kwil-core-markdown --example parse_once --release -j 4 --locked
```

The example generates slightly over 5 MiB in memory, outside the timed region,
then parses exactly once. It includes headings, quotes, tasks, tables, fenced
code, inline markup and body paragraphs. It prints bytes, blocks, elapsed time
and throughput; it performs no file I/O and no measurement loop.

Windows owner-PC result (2026-10-23, single optimized run): **5,243,380 bytes**,
**11,452 blocks**, **36.244 ms**, **137.97 MiB/s**. Release compilation in the
new branch target took 3.81 s. This measurement preceded the quoted-list
regression fix and final lint changes; it is a sample, not a UI latency gate.

Fail-first evidence: all original 12 integration tests failed against the API
skeleton returning `InvalidRange` (0 passed / 12 failed), before implementation.
All 18 final tests (17 integration, one model-limit unit test) also failed
against that deliberately broken parser before restoration. A quoted-list
regression assertion failed on the implementation before its fix. The restored
selection passes 18/18. No fixture files or shared temporary test paths are used.
