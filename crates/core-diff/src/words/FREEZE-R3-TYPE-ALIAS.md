# KW-28 begin result alias - source freeze R3

2026-10-25, shared E:/Development/kwil MAIN, base
`8ea7ec733dcd3a81d2a0bdb278fc127d6a8008dc` (read directly from HEAD and
refs/heads/main; no Git command, fetch, commit or push). Other shared dirt is
preserved. The original T3 worktree was not accessed or changed.

This receipt replaces only the worker.rs hash in historical FREEZE-R2.md.
FREEZE.md and FREEZE-R2.md remain untouched. This is source-only evidence;
ROOT owns compilation, strict lint rerun, review and commit.

## Mechanical delta

ROOT's strict log
`E:/AgentData/scratch/kwil-pr27-macos-20261024/kwil-word-sequence-strict-r1.stderr.txt`
reports one clippy::type_complexity at the original worker.rs:406 begin return
annotation. Add the documented public alias:

```rust
pub type WordBeginResult<L, R, C, A> = ControlFlow<WordRejected<L, R, C, A>, WordBuild<L, R, C, A>>;
```

Use `WordBeginResult<L, R, C, A>` as begin's return annotation. No lint
suppression, function body, owner carrier, allocation, pager, test, assertion,
manifest or gate changed. Inverting exactly this alias/documentation insertion
and return-annotation replacement reproduces the R2 worker bytes and hash.

| Artifact | SHA-256 |
| --- | --- |
| R2 worker before repair | 85791148F2604AB5C3024678DA91B123E4A0932FD0E7AB2EDF364FE79604E067 |
| R3 worker after repair | 8F7E50B14524A1AC98DF17B36D5EDF11BA030EA8A5B72FF37DE793FCC5980AE7 |
| Unchanged words/tests.rs | 3293435D37F73F5EB16B67A4D18A8DC053BEE6E4423F09517C8E52F3746C364F |

## Bounded verification

- Read both shared rule files, repository AGENTS.md/CLAUDE.md and full SPEC.md.
- Source inspected the exact reported return type and surrounding declarations.
- Direct pinned rustfmt 1.99.1, own PID 34952, exited 0 with empty stdout/stderr:
  `C:/Users/renne/.rustup/toolchains/1.99.1-x86_64-pc-windows-msvc/bin/rustfmt.exe --check --edition 2024 --config skip_children=true E:/Development/kwil/crates/core-diff/src/words/worker.rs`.
  Timeout 10000 ms; process exited and was disposed; no process remains.
- Exact inverse-delta SHA-256 equals the R2 worker hash above. All R2 worker
  code beyond the named textual delta is byte-preserved.
- words.rs, tokens.rs, tests.rs, similar_audit.md, README.md and historical
  FREEZE.md still match their R2-recorded hashes. All tests are unchanged.
- MAIN ref was rechecked after verification and remains the named base.
- Cargo, rustc, compilation, Clippy rerun, tests, native execution, network,
  Git commands/mutation, manifest changes and delegation: NOT RUN.
- No persistent environment changes. No compilation cache access; hit/miss
  is not applicable. Similar32 audit and all ROOT-owned gates remain open.

Only worker.rs and this new receipt were written by this repair lease.
The receipt's own hash is reported externally. No completion or lint-pass
claim is made beyond source inspection and formatter acceptance.
