# KW-28 P1 foreign payload ownership - source freeze R4

2026-10-25, shared `E:/Development/kwil` MAIN base
`8ea7ec733dcd3a81d2a0bdb278fc127d6a8008dc`, plus shared dirt. HEAD and
refs/heads/main were read directly and rechecked; no Git commands, fetch,
commit or push. The explicit source-only/no-Git brief overrides the generic
footer's fetch/commit/push steps. The original T3 worktree was untouched.

This receipt supersedes the worker/tests/README hashes in historical R2/R3
receipts and adds the narrowly approved core-text accessor. FREEZE.md,
FREEZE-R2.md and FREEZE-R3-TYPE-ALIAS.md remain unchanged. ROOT owns RED/GREEN,
compilation, strict lint, independent review and commit. Prior 15PASS/strict
evidence predates P1 and is not acceptance of this source.

## Cause and exact API

InputStore identifies a retained payload read by space/offset/length, not caller
ownership. A foreign Logical/0/9 read on `old word\n` therefore matches the
scanner's initial read. Previously Tracked could consume it or mark its Pending
as word-owned and abandon it. A len2 fixture exposed only coordinate mismatch.

The sole core-text change is an inherent cached scalar observation:

```rust
impl<A: Admission> InputStore<A> {
    pub fn has_pending_payload_read(&self) -> bool {
        self.payload_read.is_some()
    }
}
```

The word-local WordInput trait requires
`fn has_pending_payload_read(&self) -> bool;` with no default. Implementors
must read cached state without I/O, mutation or allocation. InputStore,
Tracked and the Source fixture wrapper explicitly implement it. InputSource
is unchanged; no other core-text file, manifest or export changed.

`begin` retains the existing active-comparison-query rejection, then checks
both inputs for pending payload reads before request validation, word nonce,
admission, file creation or source queries. Any active payload read returns
QueryChanged and the exact owners unchanged, regardless of coordinates.
Exclusive comparison ownership after this idle claim excludes legal new
foreign reads. Tracked's Pending ownership and terminal/into_parts reset
logic remain unchanged and operate only after the idle claim. No foreign
read is borrowed, drained or cancelled.

## Preserved regression and source-only coverage

ROOT's isolated RED artifact was read:
`E:/AgentData/scratch/kwil-pr27-macos-20261024/kwil-word-matching-foreign-red-r1.stdout.txt`.
It reports actual 1FAIL, 0PASS at the constructor refusal assertion. Its frozen
snapshot is `E:/AgentData/scratch/kwil-sequence-37eb7c9-r1`, inventory CSV
SHA-256 `88BB2756235CD093984D17267CAF944DADF2619F6BAF09594A6E22B91D941578`.
No new RED or GREEN run was performed by this lease.

- Adopted the final scratch test
  `matching_foreign_payload_query_is_refused_before_word_ownership` without
  changing tokens, literal strings, assertions or expected constructor phase;
  only pinned rustfmt whitespace differs.
- All 15 original fixture identities remain. Fourteen original test blocks
  match scratch source exactly after newline normalization, including the R2
  80-edit/[64,16]/DP/EOF310 pager assertions and terminal cancellation/reuse.
- The existing len2 foreign fixture now expects constructor QueryChanged,
  retains zero-reset/exact `ol` resume/cleanup assertions, and additionally
  checks unchanged reads, output, budget and absent word directory.
- One new fixture covers each side at Logical/0/9, Logical/0/2 and Logical/1/8
  with partial pending work credit. Its six cases assert cached state, unchanged
  output/both reset counts/both read counts/live and peak budgets, no word
  directory, exact input/admission owners and successful original foreign
  resume followed by cleanup. Idle is observed before and after that read.
- Total: **17 source-authored fixtures; execution NOT RUN**.

## Hash freeze

| File relative to repository | SHA-256 |
| --- | --- |
| crates/core-diff/src/words/worker.rs | 1A051CCA9097E5C3304CB72840B3D859E7BDD915C74661B25CD19A77D32D3516 |
| crates/core-diff/src/words/tests.rs | 3D324A1E85C1CCE04A92AF343AE3F155894B1F3CAC8252F29FCB5ED7CCD2344C |
| crates/core-diff/src/words/README.md | 78A1B19BB1E2B8EED06B0221F8663A0D5F7A60B0A775213B1392F09C386F3CA2 |
| crates/core-text/src/compare_input/store.rs | 1AF6D1E9E685BDFAEA41F3F21D276FD39639E932BC205C90245F6506BA7A9D2E |

The worker's exact inverse delta reproduces pre-P1 R3 SHA-256
`8F7E50B14524A1AC98DF17B36D5EDF11BA030EA8A5B72FF37DE793FCC5980AE7`.
Removing only the accessor and its documentation reproduces the complete
scratch core-text store bytes, SHA-256
`C5BF42398CE333DF8A1FA1E2C75AE6D8E8E2C37A32C637391703FA011B68AA32`.
This confirms no pager, solver, reset, gate, charge or other worker/store delta.

Protected words.rs/tokens.rs/similar_audit.md/FREEZE.md hashes still match R2:
`D0A18862535ADBFF4CFB51B79713A14836CA8173CB10B24A283B1B730DAE86FF`,
`69B119C872A98EA254B8500DFE31FDB2DB18AAD4FC846201332751DFCF913354`,
`392DD5AD18A13AE0E74B96255CB57351ED4DFC2F63F87AA2059715F9AE896E28`,
`CA683AF89D0B34EFAEB71E124A27DA6694EC922E57B3E2878DA8FC907BB2F249`.
R2 and R3 receipt hashes are respectively
`569C8E7C283480C60CEF7C13ED760051498585C05B3B0A68B013F736FEC07EA9` and
`BA14E6E3C7B0FE710D97E9B5B7706EEF47BF8E3A2D51C7558628D66F8411AA55`.
This receipt's own hash is reported externally.

## Bounded verification and remaining gates

Read shared/repository rules, full SPEC, footer, R2/R3 and similar_audit.
Direct pinned rustfmt 1.99.1 ran only worker.rs/tests.rs/compare_input/store.rs:
`--edition 2024 --config skip_children=true`. Own PID39380, 10000ms timeout,
exit0, empty stdout/stderr; process exited and was disposed. This establishes
formatter acceptance only. Source preservation/hash comparisons performed
no Rust execution.

Cargo, rustc, compiler, Clippy, tests, mutations/fail-first execution, native,
network, Git commands/mutation and delegation: **NOT RUN**. No persistent
environment changes; no compilation-cache access (hit/miss not applicable).
No fixture files were created or executed by this lease. Only the four named
source/document files and this new receipt were written.

WordBeginResult remains intact. Similar32 audit gate remains false; its 16MiB
candidate heap charge, Pending/Similar32AuditRequired, allocator-layout and
physical worker-stack gates are unchanged. No boxing, suppression or heap-cap
change. No full-v1 completion or acceptance claim. ROOT's next step is its
admitted exact regression and focused 17-fixture GREEN/strict/review work.
