# Core-text fuzzing and properties (KW-34)

The stable workspace runs `crates/core-text/tests/properties.rs`, with 32
proptest cases by default. Set `PROPTEST_CASES=8` for a small local check:

```text
cargo test -p kwil-core-text --test properties --locked -j 4
```

The properties compare random Unicode edits, undo/redo, retained snapshots,
line starts, byte columns and line iteration against String/Vec models. Sparse
line indexes are checked both fresh and extended. Encoding checks cover all
40 supported canonical codecs, Unicode/BOM variants, unmappable rejection,
decode/transcode and untouched malformed UTF-8 preservation. Unicode codec
oracles use std UTF-16/32 conversions; legacy oracles use encoding_rs.

CR-only line semantics remain a known gap owned by a separate job. The
`cr_only_lines` property is explicitly ignored. Active raw line models count LF
(CRLF is one break); random CR bytes still exercise edit/save preservation.
Re-enable and update those line models when the separate CR fix lands.

`fuzz/` has its own workspace and lockfile. It is not a main workspace member;
normal builds do not require cargo-fuzz or nightly. The three targets reuse the
same differential oracles as the properties:

```text
cargo +nightly fuzz run edits -- -max_total_time=60 -max_len=768
cargo +nightly fuzz run line_index -- -max_total_time=60 -max_len=4099
cargo +nightly fuzz run encodings -- -max_total_time=60 -max_len=513
```

Run these on CI or a dedicated machine with cargo-fuzz installed, not as long
local runs on the owner's PC. Each invocation is bounded internally as well.
The scheduled workflow runs each target for 60 seconds and increases proptest
cases to 256; PR CI retains the modest default. Crashes go to `fuzz/artifacts/`
and are uploaded by the workflow. Reproduce a crash with
`cargo +nightly fuzz run <target> <artifact-path>`; preserve a minimal regression
as a focused test before fixing a bug. Local compilation alone is not a fuzz run.
