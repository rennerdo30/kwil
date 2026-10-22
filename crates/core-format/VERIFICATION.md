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

## Review round 4 (2026-10-23)

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

Source candidate: `e8072e01c1112889b2fc0b7a1bb152247b0384a7`. After the parent’s
serial lane handoff, validation ran once per required check on exact source:

- `verify-round4-fail-first.ps1`: broken `9ade72a` source copy produced **3 failed,
  1 passed**, owned cargo PID **18220**, expected exit 101. SQL split `éfrom` into
  a name prefix plus `FROM`, the shell fixture lost literal trailing spaces, and
  opt-in reindent changed those literal bytes. The Unicode literal/comment control
  passed as required.
- Both source files restored byte-identical to the candidate, verified against
  the pre-run SHA256/mtime receipt before testing the restored implementation.
  The test build log records a fresh compile after restoration.
- `cargo test -p kwil-core-format --locked -j 4`: **53/53 passed**, including all
  **49 prior contracts** and all four round-4 tests, PID **67740**, exit 0.
- `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`: passed,
  PID **23424**, exit 0. `cargo fmt --all --check`: passed, PID **51424**, exit 0.
  `git diff --check` passed; source worktree was clean before this evidence edit.

All owned Cargo processes exited. Existing lock/reap tests prove the formatter
descendants stopped; no native/app launch was performed. The lane was released
directly to `codex-t3-a7ed711a` immediately after the finite block, and the parent
received the exact result/PID copy. No further Cargo was run for this docs update.

Cache: the brief-specified `E:/Development/_cache/cargo-target/kwil-kw-7-format`
was retained; only changed inputs compiled. No target/cache bypass, clean,
benchmark or full workspace test suite ran locally. Exact input provenance is in
`round4-source-provenance.json`; `round4-red.log`, `round4-green.log` and
`round4-clippy.log` identify their source commit on the first line, all beneath
that target directory. Native macOS/Linux and full workspace evidence belongs to
CI; the parent still owns independent review and merge.

## Review round 5 (2026-10-23)

Base: `4540af270c4e3b08387549f6f66c0d2ebbde9360`, same isolated `fix-7b`.
Three new P1 findings extend the conservative admission policy:

- Unquoted SQL `:`, `@`, `?` syntax is refused with the same explicit dialect
  explanation as `#`. Names bound to parameter/variable introducers can never
  be mistaken for standalone keywords such as `from`.
- Complete SQL atoms adjoining quote delimiters, optionally through `&`, are
  refused before ordinary string scanning. This covers Oracle alternative
  quoting and other unproven prefixed literal grammars without inferring each
  dialect's delimiters or escapes. The error returns no edit for the document.
- C/C++ block comments containing backslash/trigraph syntax protect the entire
  uncertain suffix through EOF: translation-phase splicing can change where
  the comment closes and expose raw strings hidden from a physical-byte scan.

All **53 earlier contracts** remain unchanged. Three new tests and
`verify-round5-fail-first.ps1` run against unchanged `4540af2`.
`FAILURE-MATRIX.md` now maps all R1-R5 findings to their preservation boundaries
and tests. The pending finite block is fail-first, restored focused formatter
tests, workspace Clippy and fmt, with exact hash/mtime provenance.

Exact source candidate: `84365ebfe9df7104e536db76facf3d882ec53154`. After the
explicit parent lane grant, the finite block completed serially:

- Fail-first against `4540af2`: **0 passed, 3 failed**, PID **43016**, expected
  exit 101. The exact `:from`, Oracle alternative-quote and spliced-comment
  fixtures demonstrate keyword exposure or removed literal spaces on old code.
  The script inherited `CARGO_NET_OFFLINE=true` and used `--locked -j 4`.
- Both sources were restored byte-identical, checked against the pre-run SHA256
  receipt before testing the fixed implementation. The test log records a fresh
  compile after restoration; no source timestamp/cache bypass was used.
- `cargo test -p kwil-core-format --offline --locked -j 4`: **56/56 passed**,
  including all53 earlier contracts, PID **72492**, exit 0.
- Per the parent's bounded R5 grant, targeted crate Clippy replaced another local
  workspace rerun: `cargo clippy -p kwil-core-format --all-targets --offline
  --locked -j 4 -- -D warnings` passed, PID **48944**, exit 0. Package fmt passed,
  PID **34880**, exit 0. `git diff --check` passed; source worktree was clean
  before the evidence update.
- Confirmed source CI for `84365eb`: fmt plus Linux, Windows and macOS all passed
  in [run 39848161191](https://github.com/rennerdo30/kwil/actions/runs/39848161191).
  Those jobs run the full workspace Clippy/tests and include the macOS external
  formatter success/descendant contract; green CI is not independent approval.

All owned Cargo processes exited; existing lock/reap contracts prove descendants
stopped. The lane was returned directly to parent `codex-kw-48-editor-ui` with
exact PIDs/results before this docs-only update; no more Cargo runs followed.

The brief-specified target/cache was retained without wipe or bypass. Logs
`round5-red.log`, `round5-green.log`, `round5-clippy.log` identify their tested
commit on the first line; `round5-source-provenance.json` records input hashes and
timestamps under `E:/Development/_cache/cargo-target/kwil-kw-7-format`.
No local full workspace suite, native GUI/TUI/macOS/Linux execution, benchmark or
owner acceptance ran. The parent retains independent review and merge.
