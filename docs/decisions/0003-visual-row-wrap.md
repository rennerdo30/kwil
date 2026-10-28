# Shared visual-row wrapping

Status: implementation contract, 2026-10-24. Independent Sol design review
accepted the corrected architecture; implementation and performance are unverified.

Document bytes, logical lines, cursors, undo, saving, recovery and logical Vim
operators retain their existing meaning. Wrapped text uses an explicit prepared
visual-row API. Existing unwrapped `line` and `line_source` contracts remain.

## Representation and admission

Each request carries immutable source ownership, document/source/incarnation and
revision identity, geometry/font/tab/width-policy/settings generations, and an
intent generation. Each exact row has logical-line metadata, complete source
coverage, continuation/EOL metadata and separately bounded prepared paint atoms.
Coverage is coordinate authority, never an allocation, read or syntax extent.

Atoms carry absolute source and cell spans, a bounded display payload and kind
Normal/InvalidByte/Opaque. Oversized clusters use a neutral marker with exact
endpoint mapping after resolution. An unresolved cluster end remains pending;
it never authorizes an invented interior position. Opaque atoms request no
syntax source. Hosts, search and syntax consume explicit prepared mappings;
they do not reconstruct source offsets by laying out truncated display strings.

These are new internal admission choices, not previously established budgets:

- Geometry: at most 4096 columns and 4096 visible rows; zero extent does no work.
- Source windows: at most 64 KiB each, with bounded resumable segmentation state.
- Display payload: at most 256 bytes per atom, matching the existing grid policy.
- One prepared page: at most 8 MiB retained allocation and 262144 paint atoms.
- Overscan: at most eight rows on either side; never silently omit visible rows.
- Checkpoints: at most 4096 records and 1 MiB retained allocation, evictable.
- Syntax: at most 128 windows and 256 KiB total source including context per page;
  individual windows also respect the existing syntax engine limit.
- Relative input: 128 scalar records, without source/page ownership per record.
- Navigation: at most the existing 4096 cursors, applied as one ordered set.
- All wrap-owned retained allocations together: at most 32 MiB per host session.

Charge actual retained capacities and transient construction before allocating.
Counts and checked byte arithmetic apply before building vectors or queues.
Source handles additionally have fixed ownership slots: running, latest desired
fill/reveal, ready result and two retirement slots. Refusal returns the complete
unaccepted owner. Shared source storage is not copied or exempted from existing
document retention rules. Exhaustion returns a typed error and explicit feedback.

Greedy wrapping uses a fitting whitespace opportunity, otherwise a complete
cluster boundary. Whitespace bytes remain covered. Tabs restart at each visual
row. An oversized atom consumes one clipped row so width one still advances.
Chunk boundaries are not EOL. Deep giant-line seeks and giant-cluster endpoints
can require resumable worker scans with progress/cancellation; this contract
does not claim constant-time first content or arbitrary seeks.

## Input, results and atomic publication

Only fill and absolute reveal use latest desired-state replacement. Relative
motions and scrolls use a lossless FIFO; initially there is no compression.
Dispatch its head against the actual current cursor set and geometry. Busy keeps
the exact head. Full refuses only the newest input with visible feedback. Failure
retains an explicit failed/retryable lane rather than skipping accepted inputs.

Offscreen movement returns independent `ResolvedNavigation`, not just a page.
It includes ordered cursors/primary, cursor-set generation, intent generation
and complete layout/source identities. Apply all targets together, normalize
endpoint affinity/goals, then reveal the primary. Pop only after publication.

While relative movement is unresolved, mutating input is explicitly refused
with progress/busy feedback. It must not silently cancel accepted movement and
edit the old caret, buffer typing for replay, or edit speculative targets.
Escape explicitly cancels. Source/tab/raw/geometry changes invalidate intents
with explicit state feedback; page motions never adopt a different page height.

Cursor publication uses two phases. Prepare all fallible source validation,
normalization/admission, reveal, damage and retirement work in temporary state.
Recheck cheap generations and commit infallibly, ending the typing burst only
there. Existing `set_cursors` is insufficient by itself. Precommit failure leaves
cursor/primary/affinity/goals, box-column, viewport, burst/undo grouping, document
dirty/revision and undo/redo unchanged. Fault-injection tests cover each phase.

## Worker and host ownership

One worker, one active operation, one latest desired fill/reveal and one bounded
result slot; relative input remains separately ordered. No UI source reads,
polling loop, whole-line buffer or whole-document visual-row index. Check
cancellation between bounded chunks and fence every installed result.

Stop cancels admission and transfers an owned settlement ticket. Settle source
holders off the UI before document cleanup. Shutdown is bounded and retryable;
failure retains exact owners and remains failed until successful acknowledgement.
Detach or UI destruction is not successful cleanup.

Raw hex, rendered Markdown and filtered log projections retain body ownership.
Their input/paint gates precede wrapping and reject late wrap results. The text
side of Markdown split uses its actual width. Search/highlight integration is
required: prepared atom mappings, bounded syntax windows and painted-row identity
change together. Scroll within a logical line still uses copy plus exposed rows;
reflow damages the required region. There are no idle wakeups/presents.

Implementation proceeds model first, then GUI and TUI adapters under separate
file leases. ROOT owns exports and integration. Each phase requires focused
behavior checks, controlled failures and independent review before completion.
