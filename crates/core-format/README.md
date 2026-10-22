# Worker formatting

Call `check_size` on snapshot metadata before reading any bytes. `format` is a
synchronous worker-pool job, with no drawing, thread creation, UI polling or
implicit external processes. Pass a UTF-8 snapshot and its exact file-type key.
Retain the snapshot version outside this crate and reject stale results. Apply
`replacement` and `bytes` as **one** core-text transaction (KW-18); this crate
does not own the document or undo history. `None` means no edit.

For a selection in a huge file, read only the selected bytes and call
`format_selection` with their original byte offset and line. JSON requires a
complete value; markup requires one complete element; generic tidy works on
arbitrary text. Selection results add no final newline. Other languages and
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
  Only element-only layout whitespace is changed. HTML inline containers stay
  verbatim. This is a conservative explicit-tag formatter, not an HTML5 repair
  parser: omitted end tags and DTD internal subsets report errors. It does not
  validate namespaces, entities or complete XML/HTML semantics.
- TOML uses `toml_edit` and changes whitespace-only decoration around `=` and
  inline tables. Comments, values, ordering and multiline strings stay intact.
- YAML reports **"tidy only, no structural YAML formatting yet"** in its result
  notice (coordinator-approved scope, 2026-10-23). No YAML dependency is added.
  `pretty_yaml` 0.6.0 (MIT) and `yaml-edit` 0.3.5 (Apache-2.0) exist; a follow-up
  will evaluate semantic safety on anchors, multi-documents, block scalars and
  comments before adoption. Leading and
  trailing whitespace are preserved because indentation and block scalars are
  semantic. Optional line-ending normalization and final newline still work.
  It does not parse or validate YAML; malformed YAML is retained rather than
  repaired or claimed valid.
- CSS splits blocks and declarations outside strings/comments/parentheses.
  SQL uppercases a small reserved-keyword set and separates top-level clauses.
  Both preserve quoted text/comments and validate balanced delimiters, not the
  full language grammar. SQL also preserves bracket and dollar-quoted content.
  These are lexical formatters, not dialect rewrites or syntax repair tools.
- Generic tidy has independently optional trailing-whitespace trimming,
  line-ending normalization (first file ending wins), final newline, and
  reindent. Reindent is off by default: when enabled, it converts tab stops and
  spaces while preserving visual columns, avoiding guesses at language nesting.
  Syntax-aware formatters never globally trim/normalize literal content.

## Configured external formatter

Add an exact file-type entry to `Settings.external` with executable, literal
argument vector and timeout in `(0, 120s]`. There is no shell, interpolation or
auto-discovery. A caller may explicitly configure a shell if it wants one.
The child receives stdin and writes stdout through anonymous private temporary
files, so neither blocked pipes nor inherited pipe handles can hang the worker.
The exact launched child PID is tracked, killed and reaped on timeout or output
overflow. Errors include that PID. No windows are shown on Windows.

Stdout is capped, stderr is checked at 64 KiB, and only bounded stdout is read
into memory. Disk size checks occur every 10 ms on the worker; a process can
temporarily write beyond the limit between checks. Commands must terminate
their own descendants; only the directly owned child is killed. The timeout
covers the child execution after spawn, not OS process-creation latency.
Successful stdout is retained verbatim (with the optional final newline).

## Dependencies (crates.io checked 2026-10-23)

| Crate | Version / release | Why |
|---|---|---|
| serde_json | 1.0.154 / 2026-10-11 | Tested JSON validation without reserializing values; MIT OR Apache-2.0 |
| toml_edit | 0.25.17+spec-1.1.0 / 2026-10-09 | Required TOML comment/value preservation; MIT OR Apache-2.0 |
| tempfile | 3.27.0 / 2026-03-11 | Secure anonymous cross-platform subprocess spools and cleanup; MIT OR Apache-2.0 |

No derive macros, async runtime or UI dependencies. Existing workspace lockfile
already contains these versions; enabling TOML display adds `toml_writer`.
Compile cost is recorded with the verification evidence in the PR.

## Verification

`cargo test -p kwil-core-format -j 4` runs golden/idempotence, malformed input,
selection, limits and external-process contracts. Each external fixture starts
a fresh helper process and uses independent anonymous temporary files.
`verify-fail-first.ps1` temporarily substitutes an identity formatter and disabled
admission guard, proves all contracts fail, and restores the implementation in
`finally`. Run it only with no other reader of this worktree.

One-shot measurement (run only when authorized):

```powershell
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-7-format'
cargo run -p kwil-core-format --example measure_json --release -j 4 -- fixtures/format-10mb.json
```

The example generates an ignored ~10 MB JSON fixture on disk, reads it only
after generation, formats once, and reports input/output bytes and elapsed time.
