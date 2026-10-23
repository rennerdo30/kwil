# Worker formatting

Call `check_size` on snapshot metadata before reading any bytes. `format` is a
synchronous worker-pool job, with no drawing, thread creation, UI polling or
implicit external processes. Pass a UTF-8 snapshot and its exact file-type key.
Retain the snapshot version outside this crate and reject stale results. Apply
`replacement` and `bytes` as **one** core-text transaction (KW-18); this crate
does not own the document or undo history. `None` means no edit.

The current `core-text::Snapshot` exposes `len()` and `chunks(range)` using
`u64` offsets. Reject over-limit requests before collecting chunks; use checked
conversions to this API's `usize` offsets. The document owner carries the
revision/cursor-generation fence, applies a single replacement transaction and
journals it through its normal worker path. Encoded snapshots can carry opaque
invalid-byte provenance; callers must preserve/refuse those regions rather than
treat display sentinels as ordinary text. This crate adds no core-text dependency.

For a selection in a huge file, read only the selected bytes and call
`format_selection` with their original byte offset and line. JSON requires a
complete value; markup requires one complete element. XML selections at nonzero
offsets retain all bytes with a context notice: an ancestor's `xml:space` cannot
be inferred from the fragment. Complete root selections at offset zero still
format. Generic selections require
the full admitted snapshot (`format` with a range) so the tokenizer can observe
the opening literal. A partial final selected line retains its trailing separator
bytes so applying the replacement cannot join tokens to following text.
The following document byte distinguishes a physical line ending from a clipped
line. A CRLF split by the selection remains byte-exact; shell CR stays argument data.
`format_selection` lacks literal/ancestor context and returns no edit with a
clear notice for generic or nonroot XML selections. Selection results add no final newline. Other languages and
external commands report “not supported for a selection”.

Input defaults to at most 16 MiB and output to at most 64 MiB. Callers can lower
these caps, never raise them. Nesting is capped at 128, markup at 262,144 tokens.
Output growth is checked before append. This is an explicitly size-guarded
small-document formatter, never a whole-file materialization path for huge
files. TOML's bounded syntax tree and serialization also fit under the fixed
input ceiling; a lower output limit is checked after its serialization.

## Preservation and limits

- JSON validates using borrowed `RawValue`, then lays out raw tokens. Key order,
  duplicate keys, strings, escapes and numbers stay verbatim. Comments produce a
  JSON status error with a line number rather than being silently deleted.
- XML/HTML retain attribute spelling, mixed text, whitespace-only text elements,
  `xml:space` subtrees and HTML raw `pre`/`script`/`style`/`textarea` blocks.
  XML changes only element-only layout whitespace. HTML reindents only existing
  whitespace between known block elements from the HTML Standard's rendering
  list. Adjacent tags never gain whitespace. Inline, custom, unknown and mixed
  containers stay verbatim, as do containers with inline style attributes.
  This is a conservative explicit-tag formatter, not an HTML5 repair
  parser: omitted end tags and DTD internal subsets report errors. It does not
  validate namespaces, entities or complete XML/HTML semantics.
- TOML uses `toml_edit` and changes whitespace-only decoration around `=` and
  inline tables. Comments, values, ordering and multiline strings stay intact.
  Structural endings are restored separately from raw string content.
- YAML reports **"tidy only, no structural YAML formatting yet"** in its result
  notice (coordinator-approved scope, 2026-10-23). No YAML dependency is added.
  `pretty_yaml` 0.6.0 (MIT) and `yaml-edit` 0.3.5 (Apache-2.0) exist; a follow-up
  will evaluate semantic safety on anchors, multi-documents, block scalars and
  comments before adoption. Leading and
  trailing whitespace are preserved because indentation and block scalars are
  semantic. EOF bytes and all line endings are retained too: without a YAML
  parser there is no proof that even those tidy operations preserve scalars.
  It does not parse or validate YAML; malformed YAML is retained rather than
  repaired or claimed valid.
- CSS splits blocks and declarations outside strings/comments/parentheses.
  Character and hex escapes (including their terminator whitespace), strings,
  URLs and comments are atomic; escaped punctuation never becomes structure.
  Function recognition follows [CSS Syntax 3 ident-like tokens](https://www.w3.org/TR/css-syntax-3/#consume-ident-like-token):
  escapes are decoded only for recognition, while original identifier/URL bytes
  are emitted. Other function spans remain verbatim too.
  SQL uppercases a small reserved-keyword set and separates top-level clauses.
  Both preserve quoted text/comments and validate balanced delimiters, not the
  full language grammar. SQL also preserves bracket and dollar-quoted content.
  SQL retains original whitespace separators so newline-concatenated literals
  remain concatenated, and supports nested block comments.
  Unproven SQL constructs outside literals/comments are refused with a dialect
  explanation and no returned edit: `#` has dialect-specific comment/operator
  meanings, and `:`, `@`, `?` can introduce parameters or variables. Complete
  atoms adjoining quotes (optionally through `&`) are refused too, because their
  literal delimiters/escapes require an explicit dialect. This includes Oracle
  alternative quoting; an ordinary quoted-string scanner must not guess its end.
  A configured external formatter can handle that dialect explicitly.
  A code-level `--` also requires a following ASCII whitespace/control byte;
  otherwise MySQL can parse minus operators where another dialect hides the
  rest of the line. The whole request is refused before any quote can be hidden.
  `--` inside an already-proven string, identifier, dollar quote or block comment
  stays literal data. Without a following byte, the boundary is unproven too.
  Backslashes inside SQL quoted tokens are refused too: standard-conforming and
  escape-enabled dialects disagree on which quote ends the token. Dollar-quoted
  spans retain their known delimiters and are not subject to this ambiguity.
  SQL keyword classification operates on complete identifier/number atoms. An
  unquoted non-ASCII atom is refused before keyword classification because this
  formatter has no selected dialect's Unicode identifier rules. Unicode quoted
  identifiers, literals and comments remain supported and byte-exact.
  These are lexical formatters, not dialect rewrites or syntax repair tools.
- Generic tidy runs **only** for tokenized types: Python, JavaScript/TypeScript,
  Rust, Go, C/C++, C#, Java, Shell (sh/bash/zsh/ksh), Ruby, PHP, Lua, Kotlin and
  Swift, including their extension aliases. Tokenizers identify quoted, raw,
  triple-quoted, template, heredoc/nowdoc and long-bracket literals and comments.
  Literal/comment interiors, line endings and uncertain suffixes stay byte-exact.
  Tokenization keeps one span at a time, with bounded heredoc/delimiter storage.
  It never retains a document-sized token list or mask.
- Outside those spans, optional tidy steps trim trailing spaces, normalize line
  endings (first **structural** ending wins), add a final newline, and convert
  tab stops while preserving visual columns. Reindent is off by default and is
  always skipped for Python's semantic indentation and for selections. Final
  newline is skipped if EOF is inside an uncertain/unterminated span. YAML always
  retains its exact EOF bytes. Structured formatters never globally trim literals.
- These are conservative lexical scanners, not full language parsers. Nested
  interpolation, regex/division ambiguity, JSX, continuation syntax and other
  unproven constructs protect the remaining suffix, rather than guessing a closing
  delimiter. Code before that suffix can still be tidied. PHP inline HTML and
  attributes and Ruby trailing data are protected. PHP heredoc spans stop at the
  closing label, so following expression literals are tokenized normally.
  PHP's `__halt_compiler` introducer is ASCII-case-insensitive; every case
  protects its trailing data through EOF. Text inside an already-proven string
  or comment does not activate that halt boundary, and ordinary code still tidies.
  Heredoc body scanning requires a proven simple header made only of heredoc
  operators, simple delimiter words (optionally quoted) and spaces/tabs. Any
  other header token protects the suffix through EOF: a physical LF inside a
  quoted argument, continuation or substitution must never start a body scan.
  JS/TS comments terminate at U+2028/U+2029 as well as CR/LF. Shell uses only LF
  as a structural ending: bare CR and the CR in CRLF retain their argument bytes.
  C/C++ block comments encountering backslash or trigraph syntax protect the
  suffix: translation-phase line splicing can change where the comment closes
  and expose later raw-string openers that a raw-byte scan would otherwise hide.
  Java compilation units containing a literal `\u` introducer retain all bytes:
  Unicode escapes are translated before comments and literal delimiters, so a
  raw-source tokenizer cannot establish their lexical context safely.
  Comments are preserved too (trimming them is allowed by policy, but not needed).
  Boundary references: [PHP heredoc expressions](https://www.php.net/manual/en/language.types.string.php#language.types.string.syntax.heredoc),
  [PHP attributes](https://www.php.net/manual/en/language.attributes.syntax.php),
  [ECMAScript comments](https://tc39.es/ecma262/multipage/ecmascript-language-lexical-grammar.html#sec-comments),
  [MySQL comments](https://dev.mysql.com/doc/refman/8.4/en/comments.html).
- Types without a tokenizer, including unknown extensions, CSV/data, Markdown,
  reStructuredText, Makefiles and patches, receive **no edits**, including EOF.
  Status: "No safe formatter for this file type. Configure an external formatter,
  or use 'Trim trailing whitespace' explicitly."
- `trim_trailing_whitespace` is the separate `edit.trimTrailingWhitespace` worker
  operation. Its palette title and result notice warn that it **may change
  literals**. It trims even literal/heredoc whitespace only when explicitly
  invoked; it preserves indentation, every line ending and final-newline style.

## Configured external formatter

Add an exact file-type entry to `Settings.external` with executable, literal
argument vector and timeout in `(0, 120s]`. There is no shell, interpolation or
auto-discovery. A caller may explicitly configure a shell if it wants one.
The child receives stdin and writes stdout through anonymous private temporary
files, so neither blocked pipes nor inherited pipe handles can hang the worker.
The exact launched child PID is tracked and reaped on timeout or output overflow.
Windows starts it suspended, assigns a private kill-on-close Job Object, then
resumes only its owned initial thread. Unix establishes a new process group
before exec, observes exit without reaping, kills the group and then reaps its
leader (preventing process-group ID reuse). Descendants are stopped on success
as well as failure. Errors include the child PID. No windows are shown on Windows.
On macOS, XNU can return EPERM for a zombie-only group. On that error only,
bounded libproc enumeration verifies that our reserved group has no live members
before accepting it. Inspection errors or live members remain fatal. ESRCH means
the group is empty. The leader is reaped after this decision and the consumed group
ID is never signalled again after reap.
References: [XNU group zombie filter](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c),
[libproc enumeration](https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.c).

Stdout is capped, stderr is checked at 64 KiB, and only bounded stdout is read
into memory. Disk size checks occur every 10 ms on the worker; a process can
temporarily write beyond the limit between checks. Unix commands that deliberately
detach into another session/group are outside process-group containment. The timeout
covers the child execution after spawn, not OS process-creation latency.
Successful stdout is retained verbatim. The optional final newline applies only
to known-safe structured types; a generic external formatter owns its EOF bytes.

## Dependencies (crates.io checked 2026-10-23)

| Crate | Version / release | Why |
|---|---|---|
| serde_json | 1.0.154 / 2026-10-11 | Tested JSON validation without reserializing values; MIT OR Apache-2.0 |
| toml_edit | 0.25.17+spec-1.1.0 / 2026-10-09 | Required TOML comment/value preservation; MIT OR Apache-2.0 |
| tempfile | 3.27.0 / 2026-03-11 | Secure anonymous cross-platform subprocess spools and cleanup; MIT OR Apache-2.0 |
| windows-sys | 0.61.2 / 2025-10-06 | Windows-only Job Object and suspended-thread APIs; MIT OR Apache-2.0 |
| libc | 0.2.191 / 2026-10-16 | Unix-only waitid/killpg; MIT OR Apache-2.0 |

No derive macros, async runtime or UI dependencies. Existing workspace lockfile
already contains these versions; enabling TOML display adds `toml_writer`.
Compile cost is recorded with the verification evidence in the PR.
The two platform dependencies were already locked by the workspace; this adds
no crate versions. Current releases were checked through the crates.io API on
2026-10-23. Process API references: [Windows jobs](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
[owned thread enumeration](https://learn.microsoft.com/en-us/windows/win32/toolhelp/traversing-the-thread-list),
[Rust process groups](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.process_group).

## Verification

`cargo test -p kwil-core-format --offline --locked -j 4 --tests` runs golden/idempotence, malformed input,
selection, limits and external-process contracts. Each external fixture starts
a fresh helper process and uses independent anonymous temporary files.
`verify-fail-first.ps1` temporarily substitutes an identity formatter and disabled
admission guard, proves all contracts fail, and restores the implementation in
`finally`. Run it only with no other reader of this worktree.
`verify-review-fail-first.ps1` copies the broken PR base's implementation into
this owned worktree, runs the review regressions (12 failures and one idempotence
control), and restores sources in `finally`. The wrapper/grandchild fixture has
a six-second natural lifetime, so even the broken-copy run leaves no live fixture.
`verify-round2-fail-first.ps1` restores broken CSS recognition, disables literal
protection, makes explicit trim an identity and disables descendant containment.
Nine defect tests must fail; the unknown-type control passes. Sources are restored
in `finally`, and the success descendant fixture is allowed to exit naturally
before its mutation assertion fails.
`verify-round3-fail-first.ps1` runs the six round-3 boundary regressions against
the unchanged `f9dff6f` tokenizer and SQL implementation. All six must fail; both
sources are restored in `finally`. Obtain the shared Cargo lane before running it.
`verify-round4-fail-first.ps1` runs against unchanged `9ade72a`: Unicode SQL
identifier and uncertain-header tests must fail (three failures); the Unicode
SQL literal/comment control must pass. It restores sources in `finally` too.
`verify-round5-fail-first.ps1` requires all three parameter/prefixed-literal/C++
comment-splice regressions to fail on unchanged `4540af2`, then restores sources.
`verify-round6-fail-first.ps1` requires all four exact source-derived R6 review
fixtures to fail on unchanged `72f87e0`, then restores all three source files.
`verify-round7-fail-first.ps1 -Case Base` requires all four R7 tests to fail on
unchanged `c8a173e`; `-Case Sql` and `-Case Php` select independent compiling
guard faults and require the corresponding two tests to fail. Each invocation
must fit the parent's finite grant. It restores exact source bytes/SHA/mtime,
then logs an explicit timestamp advance for only those restored source files so
the next positive run rebuilds repaired source without reusing a mutant binary.

All harnesses require a direct parent Cargo grant and no concurrent reader of
this package. `verify-support.ps1` uses the configured shared cache without
environment edits, runs each child for at most 180 seconds with hidden windows,
and records the exact owned PID, lifetime, source hashes and timestamps outside
the repository. Each mutation must compile and fail named behavioral tests;
compiler errors and zero-case runs are rejected. Exact bytes, hashes and mtimes
are restored in `finally`. `verify-wave-fail-first.ps1 -Case <case>` admits one
independent fault for sql, separator, xml, java, crlf or physical-eol. The two
selection-boundary tests cover clipped CRLF and actual physical line endings.

One-shot measurement (run only when authorized):

```powershell
cargo run -p kwil-core-format --offline --locked --example measure_json --release -j 4 -- fixtures/format-10mb.json
```

The example generates an ignored ~10 MB JSON fixture on disk, reads it only
after generation, formats once, and reports input/output bytes and elapsed time.
