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

## Review round 3 (2026-10-23)

Base: `f9dff6f0a3dc48aa516b4ba60d9149b4a69029dd`, same isolated `fix-7b`.
No broader source scope, native run, benchmark, cache wipe or full suite.

- PHP heredoc/nowdoc ends at its closing label token, so a following expression's
  opening quote is scanned rather than hidden by an entire-line span. Indented
  labels and labels followed by a Unicode identifier continuation are covered.
- PHP `#[...]` attributes are preserved as an uncertain suffix, instead of being
  mistaken for hash comments that hide a multiline string opener.
- JS/TS line comments end at U+2028/U+2029 as well as CR/LF; subsequent template
  literals retain all whitespace. Both Unicode separators and JS/TS aliases are
  covered.
- Shell treats only LF as structural: bare CR and CRLF's CR retain their argument
  bytes. CR is also excluded from shell comment-word boundaries, so a following
  hash cannot hide an opening quote. Existing heredoc contents stay exact.
- SQL `#` outside a recognized literal/comment reports an explicit dialect error
  and applies no edit, rather than exposing MySQL comment contents as SQL. Other
  dialects' hash operators/temp names are refused too, without guessing a dialect.

**Fail first:** `verify-round3-fail-first.ps1` copies only the unchanged broken
base's `literals.rs`/`languages.rs`, runs the six new tests, and restores both
files in `finally`. **0 passed, 6 failed**, exit 101, owned cargo PID **54164**.
Failures show literal spaces removed in both PHP cases and JS templates, Shell's
final LF replaced by CR, SQL hash-comment text exposed as code, and reindent
changing a PHP string with all tidy options enabled.

**Restored source:** `cargo test -p kwil-core-format -j 4`: **49/49 pass**
(20 contracts, all 13 round-1, all 10 round-2, 6 round-3), PID **79212**, exit 0.
Clippy's identical-branches warning prompted an equivalent combined PHP guard;
the final round-3 selection then passed **6/6**, PID **58884**, exit 0.
`cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings` passed,
PID **40928**, exit 0. `cargo fmt --all --check` and `git diff --check` passed.
All Cargo invocations were serial, after explicit KW-48 lane release; the lane
was returned immediately after checks. Owned Cargo processes exited and the
existing external-process lock/reap contracts passed; no fixture was left live.

Cache: retained existing target artifacts under
`E:/Development/_cache/cargo-target/kwil-kw-7-format`; only changed inputs compiled.
Logs `round3-red.log`, `round3-green.log`, `round3-final.log`, `round3-clippy.log`
in that directory identify the tested base plus edits on their first line.
No native macOS/Linux, full workspace tests, GUI/TUI/undo integration or owner
acceptance ran locally. The parent owns the new CI result, review and merge.

## Review round 4 source candidate (2026-10-23)

Base: `9ade72ae2c5465c528a268a56d681c2592482466`, same isolated `fix-7b`.
The fourth review found two further boundary assumptions. The implementation
approach is now conservative admission, rather than guessing missing grammar:

- SQL consumes complete identifier/number atoms before keyword classification,
  and refuses unquoted non-ASCII atoms with an explicit dialect explanation.
  Recognized Unicode quoted identifiers, literals and comments still format.
- Heredoc body searching is admitted only after validating the entire simple
  header (heredoc operators/delimiters and spaces/tabs). Any other header token
  protects the suffix through EOF. A physical LF inside a quoted argument or
  continuation is never presumed to be the shell's lexical NEWLINE.
- `FAILURE-MATRIX.md` maps every earlier R1-R3 failure, both R4 failures and the
  unchanged admission/process contracts to their preservation rules and tests.
  All 49 previous contracts are retained. The four new tests cover three failure
  selections plus a Unicode SQL literal/comment control.

**Validation pending:** no Cargo/test/Clippy invocation has run for this candidate.
The parent owns the current shared lane queue and must explicitly grant it before
this job runs `verify-round4-fail-first.ps1`, the restored focused formatter suite,
workspace Clippy and fmt. Source formatting used only standalone rustfmt (PID
20856, exit 0); `git diff --check` passes. No full suite or native run started.
This source candidate is not ready for merge or a passing claim.
