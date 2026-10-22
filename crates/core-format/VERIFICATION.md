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
