# KW-28 word paging repair - source freeze R2

2026-10-25, `E:/Development/kwil`, main base `3092b71` plus shared dirt.
ROOT authorized reopening the word files after its focused R1 run reported
14 passing fixtures and one failure: expected 80 edits, received 81 in
`more_than_64_changes_page_with_one_byte_quanta`.

This replacement freeze supersedes the source hashes in historical FREEZE.md
(SHA-256 CA683AF89D0B34EFAEB71E124A27DA6694EC922E57B3E2878DA8FC907BB2F249).
Historical FREEZE.md is preserved unchanged. Full v1 remains incomplete.

| File relative to crates/core-diff/src | Current SHA-256 |
| --- | --- |
| words.rs | D0A18862535ADBFF4CFB51B79713A14836CA8173CB10B24A283B1B730DAE86FF |
| words/tokens.rs | 69B119C872A98EA254B8500DFE31FDB2DB18AAD4FC846201332751DFCF913354 |
| words/worker.rs | 85791148F2604AB5C3024678DA91B123E4A0932FD0E7AB2EDF364FE79604E067 |
| words/tests.rs | 3293435D37F73F5EB16B67A4D18A8DC053BEE6E4423F09517C8E52F3746C364F |
| words/similar_audit.md | 392DD5AD18A13AE0E74B96255CB57351ED4DFC2F63F87AA2059715F9AE896E28 |
| words/README.md | 3D9CAD7544159D7B1F4F490FBA271766EAD1757BB290A10701718B208AB29D2E |

This document's own hash is reported externally. No later source edits are
included in this freeze.

## Cause and repair

The fixture is correct. The pager stopped its event loop as soon as opening
the deletion half of replacement 64 filled the 64th output slot. Its adjacent
insertion still had an empty left-side anchor and was emitted as a separate
record on the following page. That yielded 64+17 records instead of 64+16.

Source-derived coordinates for this fixture make the split concrete:
replacement 64 is left/right 242..245. The original boundary published deletion
left242..245/right242..242, then the next page began with insertion
left245..245/right242..245. The following equal space is 245..246; replacement
65 starts at 246. These coordinates are derived from the authored fixture and
pager source, not a newly executed trace.

`page_inner` now continues adjacent mergeable edit events even at capacity,
and advances equal coverage. It stops only before the next nonmergeable edit.
The existing fixed current-event slot retains that lookahead without consuming
its event index or byte coordinates. The next page begins at that exact edit;
no 65th output slot is used. One-byte query credits and publication remain
resumable, and outputs remain unchanged until Ready. No solver/token policy,
record layout, admission charge, query lease or cancellation path changes.

## Preserved and strengthened regression

All 15 fixture identities, all earlier assertions, and all loop deadlines are
preserved. The expected total remains **80**, with additional assertions for:

- Exactly two page lengths [64,16].
- Actual Similar byte-word tokenizer counts [160,160]. All 80 nonwhitespace
  words on one side differ from every word on the other side; only the 80
  single-space tokens match. Independent DP checks LCS=80 and edit cost160.
- Every ordered absolute edit equals the independently constructed word byte
  interval, with both sides nonempty; no space is edited and no replacement
  is split at an empty anchor.
- First page ends at replacement65's start (246 on both sides), successive
  coverage is contiguous, and final complete coverage ends at both exact
  full-line EOF offsets (310), without a continuation.

The assertions and DP are source-authored; they have not been executed by this
repair lease. ROOT owns the rerun. Its reported R1 run is recorded at
`E:/AgentData/scratch/kwil-pr27-macos-20261024/kwil-word-backend-focused-r1.stdout.txt`.
That run predates this replacement source and is not a pass for R2.

## Verification and open gate

- Standalone pinned rustfmt 1.99.1 exited0, own PID64988, with a 10000ms bound;
  the process exited. This proves formatter acceptance only.
- Compiler, Cargo, tests, fail-first/mutations, strict lint, native, network,
  Git mutation and delegation: **NOT RUN by this repair lease**.
- Only worker.rs/tests.rs and this new replacement-freeze document changed in
  this repair. No protected files or existing stream assertions were edited.
- Similar32 audit gate remains false and Pending/Similar32AuditRequired. The
  pinned allocator-layout and compiled worker-stack/frame evidence are still
  ROOT-owned gates; no legacy readiness or full-v1 completion claim.
