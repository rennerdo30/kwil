# Row word refinement integration

Source candidate on `E:/Development/kwil`, base main `8ea7ec7` plus shared dirt.
ROOT owns module integration, compiler/strict checks, independent review and
hosted/native acceptance. The P1 repair lease edits worker/tests/README, its
freeze receipt, and only the cached pending-read accessor in core-text's
compare_input/store.rs. Historical receipts remain unchanged; see
`FREEZE-R4-PAYLOAD-IDLE.md` for the current hashes.

`request` remains a source-free applicability probe. For a modified paired row
it reports Pending/BackendRequired and empty incomplete coverage. It never
touches an input or claims executable-backend completion.

The executable worker API is:

1. Prepare an `AlignmentAdmission<A>` with its own five-file path owner and the
   same invocation aggregate budget as the retained comparison and inputs.
2. Construct `WordRequest { row, left, right, hunk, backend }`. `left/right` are
   complete `LineSpan`s including terminators, and are the full refinement
   target. Pass the exact `DiskComparison<L,R,C>` to `begin`. L/R must implement
   local `WordInput`; `InputStore` has explicit cached-pending and inherent-abandon
   adapters. Wrappers must implement `has_pending_payload_read(&self) -> bool`
   from cached state without I/O, mutation or allocation, and bounded
   `abandon_word_read(&mut self)` themselves. Neither method has a default.
3. `begin` returns ControlFlow::Break(WordRejected) with every exact owner on
   pre-admission refusal, or Continue(WordBuild). A live comparison query is
   refused unchanged. Both inputs must also have no pending payload read:
   even matching space/offset/length belongs to a foreign owner before this
   idle claim. Refusal returns QueryChanged before word admission, nonce/state
   acquisition or any source query, without resetting or draining that read.
   Cached observations perform no I/O. The exclusively owned comparison then
   prevents legal new foreign reads for the invocation, so Tracked can own
   subsequent Pending reads and reset only those on termination/into_parts.
4. Consume `WordBuild::step(ReadWork,cancel)`. The fixed carrier exposes
   `state`, `pending_reason`, and tagged ControlFlow extraction of the retained
   build or terminal owners. Wrong-tag extraction preserves the whole carrier.
   Pending/WorkExhausted is resumable; Pending/Similar32AuditRequired is the
   explicit unresolved legacy audit gate, not a claim of future completion.
5. On Ready, use `WordPageQuery::new(backend,continuation)` and `build.page`.
   A page has <=64 absolute logical range edits with explicit empty insertion/
   deletion anchors. Equal events advance contiguous coverage without edits.
   A Ready page may have incomplete target coverage; only `coverage.complete`
   certifies both full-line EOF endpoints. Resume its private continuation
   with a fresh query. Query and continuation ownership bind the unique word
   invocation and comparison, immutable request/options/full input identities,
   generation and page position. Outputs are unchanged on non-Ready results.
6. `abandon_page` abandons only word-owned record transfers. Its old query lease
   cannot steal a subsequent query. `into_parts` and cancelled/failed builds
   reset only tracked pending word payload reads and return comparison,
   request, original optional error, and `WordRetirement`. Comparison/input
   files and budgets retain their original owners. Retirement is explicitly
   scheduled on a worker; Drop is not cleanup.
7. `WordRetirement::step` returns pending/cancelled/failed retirement owners
   intact, or the exact original `AlignmentAdmission<A>` on successful close/
   unlink. Failure does not release aggregate charges or delete extra children.
   The original comparison can be reused for another row independently.

The external backend is `Similar32TokensExternalExact`: one 64-byte descriptor
per maximal text word/whitespace run, independent of token length, and typed
opaque descriptors. Two 256-byte scanning windows plus four-byte lookaheads
produce original logical endpoints; equality uses two 256-byte windows.
Opaque equality compares codec, raw length and every raw payload byte.
Preserved aliases compare logical text. EOL is part of the target and tokens.
The callback adapter reuses `crate::sequence::{Sequence,SequencePair,Event,Layout}`
and its exact Myers traversal; no second solver was copied.

Five `Records` banks are word-owned: FORWARD/REVERSE frontiers, STACK DFS,
RUNS contiguous left then right descriptors, HUNKS ordered equal/edit coverage
events. `word_charge` checks worst-case linear byte-count ceilings, all fixed
generic state/transient/constructor/carrier/query/publication/path overlaps and
five handles. Begin precharges fixed resident state plus initial five 64KiB
disk blocks. Later disk-block growth goes to the same aggregate before access
and cannot exceed the checked ceiling. It does not reserve byte-bound worst
disk capacity for a giant word that needs only one token descriptor. All actual
charges remain held through explicit successful retirement.

`Similar32` retains the actual 3.2 legacy borrowed-byte `diff_words` call for
<=4096-byte full lines, <=64-line hunks and no opaque atoms. Its entry is gated
by `SIMILAR32_AUDIT_ACCEPTED=false`. See `similar_audit.md`: the 16MiB heap
candidate and source work/recursion counts are not allocator/physical-stack
proof. ROOT must close those gates before changing the gate. The external
backend preserves token boundaries and exact equality, and makes no promise
of matching the legacy heuristic script or compaction ties.

Source-authored fixtures cover actual Similar oracle (deferred by the audit
gate), independent DP cost, cross-window/invalid UTF-8 boundaries, giant token,
EOL/anchors, opaque codec/length/full payload and literal-marker separation,
preserved aliases, >64 paging, one-byte quanta, unchanged output, foreign input
and comparison queries, cross-word and abandoned query ownership, original read
refusal payload, fourth identity word, cancellation during Pending then reuse,
admission refusal and nonrecursive cleanup failure ownership. The P1 repair
retains all 15 fixture identities (the len2 foreign fixture now refuses at
begin), adopts ROOT's exact matching-coordinate regression with formatting
only, and adds one fixture with six left/right matching/mismatched cases:
17 source-authored fixtures, not executed by this lease.

Compiler, Cargo, tests, fail-first mutations, benchmarks, native launches,
network and Git mutation: **NOT RUN**. Standalone pinned rustfmt syntax/layout
acceptance is separate evidence. Existing nine stream assertions are untouched.
