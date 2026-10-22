# KW-7 verification

Base: `b1b5c55658953a8572ea10662adf4f2aa4e7f730`, branch `kw/7-format`.
Windows x64, pinned Rust 1.99.1, isolated target directory, `-j 4`.

- Initial formatter golden run: 16 passed, TOML comment-preservation failed.
  `Table::fmt()` cleared comments; replaced with whitespace-only decor visiting.
- Deliberately broken copy: identity `format` plus disabled `check_size`.
  **0 passed, 20 failed**, exit 101. Mutation cargo PID 27768, exited and reaped.
  `verify-fail-first.ps1` restores source in `finally`; the normal contracts then
  pass **20/20**, including the previously failed TOML case.
- All formatter goldens assert exact output and a second format with identical
  bytes and no edit. Other contracts cover invalid input with line numbers,
  optional tidy steps, bounded output, UTF-8, selection boundaries, one-edit
  application, large-document/small-selection admission, external I/O, nonzero
  exit, invalid output, overflow, and timeout/reaping.
- Single authorized measurement: generated **10,000,004-byte JSON** on disk in
  ignored `fixtures/format-10mb.json`, formatted once with the release example.
  **88.585 ms**, **112.89 decimal MB/s**, **14,864,878 output bytes**. Measurement
  PID 24388, exit 0. Timing covers validation, formatting and result allocation;
  excludes fixture generation, disk read and process startup. No warmup or loop.
  Measured before final lint, line-ending lookup caching for non-JSON formatters,
  and expanded YAML notice; JSON formatting code and fixture are unchanged.
- Fresh focused release example build, with dependencies and thin LTO:
  **5.16 seconds**, 21 compiled packages including this crate, at `-j 4`.
  Existing registry/target artifacts were retained; no clean or cache bypass.
  This is the observed build cost, not a repeated clean-build benchmark.
- Final local gate: `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`
  and `cargo fmt --all --check` pass; focused `cargo test -p kwil-core-format
  --locked -j 4` passes **20/20**. No other source scope changed.

Not run locally: full workspace test suite; Linux/macOS native execution; GUI,
TUI or editor integration; KW-18 undo engine integration; owner acceptance.
Only the replacement contract is tested here. Cross-platform workspace tests
and Clippy belong to CI; the parent handles CI monitoring and independent review.

YAML scope: coordinator approved tidy-only for this PR on 2026-10-23, with the
explicit notice in the API, README and PR body. Structural YAML formatting and
semantic round-trip dependency evaluation are a follow-up, not a passing claim.

## Review round 2 (2026-10-23)

Started from exact PR head `a4350e43c3d58f18b5041ebaa7891c68d369cdaf`
on isolated local branch `fix-7b`. Merged main `4198232` as `8f2ca64` before
implementation. Cargo reconciled the auto-merged lockfile without further edits.

- Preserved all round-1 fixes: Markdown, HTML, SQL, YAML, TOML, CSS escapes and
  Windows timeout descendant cleanup; all 13 round-1 tests still pass.
- macOS: accept EPERM only after bounded inspection finds no live members of
  the still-reserved process group. ESRCH remains an empty-group success. The
  leader is reaped afterwards; no group signal can occur after PID release.
- CSS: decode identifier escapes for URL recognition, emit original bytes,
  preserve complete URL/function spans. Cases include escaped `url`, case
  variants, six-digit escapes, escape terminator whitespace and `{`, `;`, `}`.
- Generic tidy now has literal/comment tokenizers for Python, JS/TS, Rust, Go,
  C/C++, C#, Java, Shell, Ruby, PHP, Lua, Kotlin and Swift. Raw strings, text
  blocks, templates, heredocs/nowdocs and long brackets retain interior bytes.
  Uncertain syntax protects the suffix including EOF. Python indentation is
  retained; contextless generic selections receive no edit with an explanation.
- Unknown types/CSV/data and other untokenized types receive no edits and the
  required status. Explicit trim has a separate API and warned palette title;
  it may trim literals, but preserves every ending and EOF style. YAML never
  gets a final newline.

**Fail first:** on merged `8f2ca64` with its unchanged round-2-broken formatter,
all six initial regression tests failed (cargo PID 77788, exit 101). The expanded
ten-test mutation run rejected all nine deliberate defects, with one unknown-type
control passing (cargo PID 42100, exit 101). In particular, the new successful
formatter/descendant contract failed on the surviving fixture's OS lock, waited
for its bounded six-second lifetime, then failed without a live fixture left.
The explicit trim mutation failed on unchanged literal whitespace.
Reproduce with `verify-round2-fail-first.ps1` in an isolated idle checkout.

**Restored source:** `cargo test -p kwil-core-format -j 4` passed **43/43**
(20 contracts, 13 round-1, 10 round-2; final cargo PID 53316, exit 0).
`cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings` passed
(final cargo PID 39372, exit 0). `cargo fmt --all --check` and `git diff --check`
passed. Shared target/cache artifacts were retained; only changed inputs compiled.
Owned children are reaped by the API; lock contracts prove descendants stopped.
No GUI/editor/player or visible window was launched.

Evidence logs (first line identifies the tested base plus deliberate/local edits):
`E:/Development/_cache/cargo-target/kwil-kw-7-format/round2-mutation.log` and
`round2-final-tests.log`. All mutation sources were restored in `finally`.

**Not run locally:** full workspace tests, native macOS/Linux execution, a new
performance measurement, GUI/TUI/undo integration or owner acceptance. macOS CI
must pass the new contract that first formats successfully with a zombie-only
leader, then successfully formats and stops a live descendant. The parent owns
CI monitoring, independent review and merge; this implementation job does not merge.
