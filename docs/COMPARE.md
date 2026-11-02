# Compare worker

Both frontends use `view-model::compare_session`. The existing bounded
imara/similar backend remains the fast path. Its size refusal is an internal
backend selection signal: the worker retries with immutable disk-backed inputs
and the external exact Myers solver. There is no user-facing line-count,
line-length or total-text cap on the fallback.

The worker captures both logical bytes and original-byte provenance using
`SnapshotLease` and `InputStore`. Right-hand files use encoding detection and
`DecodedNativeLease` over a duplicate of that exact file handle. This avoids
constructing a resident text index or decoded document for the right side. Capture validates the sources;
a changed source fails explicitly instead of publishing a partial comparison.
After capture, whitespace toggles reuse the immutable stores.

Alignment runs and hunk ranks stay in the engine's disk records. A fixed-width
host projection stores collapsed/expanded ranges on disk, with binary lookup
for display positions. F7 navigation queries disk hunk ranks. Expanding a
region changes the projection without repeating alignment.

Only a 128-row page crosses to the GUI/TUI. Text is clipped to 4,096 bytes per
side per row. Word refinement uses the full logical lines, streams its output
in 64-record pages, and retains only nonempty ranges intersecting those visible
bytes. The fallback selects `Similar32TokensExternalExact`, including for small
rows inside a large comparison. It uses Similar's byte-word boundaries with an
exact external solver; edit ties can differ from the bounded Similar script.
The experimental `Similar32` backend's heap/stack audit gate remains closed;
production fallback never waits on `Similar32AuditRequired`.

One aggregate admits the external engine's fixed state against 64 MiB, including
a 40 MiB host reservation for two maximal published pages, word-range capacity,
path/query/decode scratch and worker stack. Input, alignment, word and projection
scratch grow on disk. Resource/IO/source-change failures report a clear error;
none is presented as complete equality. This admission limit is not a measured
whole-application RSS claim. Source snapshots remain subject to the editor's
existing storage budgets.

Builds, reads and explicit retirement stay on the compare worker. Cancellation
is checked between cooperative work quanta; dropping the worker interrupts its
active external operation. Cleanup is nonrecursive and touches only the exact
private paths created for that invocation. Failed cleanup reports the retained
path and preserves the core reservation rather than releasing its capacity.

The all-size host integration does not establish multi-GB performance, the
100k-line timing gate, remote-desktop damage budgets or other-platform native
acceptance. Those remain separate hosted/owner checks.
