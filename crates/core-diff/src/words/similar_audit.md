# Similar32 source audit - 2026-10-25

Status: **source candidate, no readiness claim**. No Similar call, compiler,
test, network fetch, thread launch, or manifest/feature change was executed by
this task. ROOT must qualify the pinned standard-library allocation layout and
compiled worker stack before admitting the Similar32 backend. The external
backend has no Similar call, recursion, token Vec or source-sized allocation.

`SIMILAR32_AUDIT_ACCEPTED` is false in the executable source. The build returns
Pending with `WordPendingReason::Similar32AuditRequired` before the indivisible
Similar call or its work-credit acquisition. No timeout, candidate heap charge,
or large work grant bypasses this gate. The legacy oracle fixture asserts that
gate and preserves its actual Similar oracle branch for ROOT's later qualified
validation; a passing gate assertion would not validate the oracle branch.

## Exact call and cached primary sources

`TextDiff::configure().diff_words(&left[..n], &right[..m])` on **borrowed `[u8]`**,
default Myers, exact whitespace, no timeout. Both full lines include EOL;
`n,m <= 4096`, containing hunk <=64 lines on each side, and an entire atom scan
has proved neither line contains opaque atoms. No owned input conversion,
`iter_all_changes().collect()`, inline, Unicode, Patience, Hunt, Histogram,
grouping, or rendered output API is invoked.

Cached registry root:
`C:/Users/renne/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.
These are the actual primary implementation files read, not documentation-only
complexity claims:

| File under registry root | SHA-256 |
| --- | --- |
| similar-3.2.0/src/text/abstraction.rs | 3C87E41C20A3034FA63E82D6CAD0F6046565BCB465B2ACF7E24A5B544DCF19B0 |
| similar-3.2.0/src/text/mod.rs | 6DB680A2F7F0D6416B1C75ECF96A68BCA264F966D6DED35FD863DE1AE59FAD47 |
| similar-3.2.0/src/algorithms/preflight.rs | 391496AC49E51B29C4E56941E1B0C5ACB137B8C7CF2DC9AB7874BA7996E0E976 |
| similar-3.2.0/src/algorithms/myers.rs | EAAB6B1B6AA5DA725A5481B5AEBFD18E279A2D3D3F46D9915BDB77B019910255 |
| similar-3.2.0/src/algorithms/utils.rs | 657B73D385D3383CA8196EF45957994C8AB9BD1DBAC4CFC3EB72CB1F9B1A602F |
| similar-3.2.0/src/algorithms/compact.rs | E1019010C27368FC62DFEEB1F17AD2BDD2B2116098661923854F359ABC9FFAD5 |
| similar-3.2.0/src/algorithms/capture.rs | 6CB607FEB48A7020C5BF55AC1D2C858C1A3445D67842F70C20E228BDAE9CD64D |
| bstr-1.13.1/src/utf8.rs | 2B40260CE316C466B223616FA27210E93D01B18BCB2F7838B88F5E2B759AFEF5 |

Also inspected: Similar `Cargo.toml`, `types.rs` (MapType feature selection),
`common.rs` (Compact -> Replace -> Capture), `replace.rs`,
`deadline_support.rs`, and Myers front-anchor, small-DP and conquer bodies.

The manifest enables only `text,bytes`, with default features off. MapType is
therefore alloc BTreeMap unless another dependency unifies a different feature.
ROOT must verify feature unification in its admitted check; this task did not
run Cargo. `duration_to_deadline` returns None without std/web-time, and
`deadline_exceeded` is false. The former 10ms timeout is **not a work bound**.
This task does not enable std to change that behavior.

## Concrete cardinalities and simultaneous state

Let N and M be actual maximal whitespace/nonwhitespace token counts, S=N+M,
B=n+m. Every nonempty token consumes at least one original logical byte, hence
N<=4096, M<=4096, S<=8192, B<=8192. Hash collisions never establish equality;
bucket searches compare full byte slices.

| Allocation/work site | Source-level upper cardinality and overlap |
| --- | --- |
| two borrowed token vectors | S fat slice references; tokenizer pushes. Old/new capacity growth may overlap old allocations during realloc. |
| integer remapping when either side >100 | S u32 values, at most S representative bucket entries, S bucket-tail values; remapping map drops before capture, integer vectors remain. Token buffers remain live. |
| equality pre-scan | At most min(N,M) full-token comparisons before remapping; no new collection. |
| preflight map | At most N hash keys and N representatives/count IDs; collision tails total <=N. New-side probes can visit all old representatives. Map is explicitly dropped before reduction. |
| reduction values/counts/actions | Two original ID vectors <=S usize values; two counts vectors <=2N usize values; reduced values + original indices <=2S usize values; action vector <=max(N,M) enum values at once. Old-side arrays are dropped as shown in preflight.rs. Conservatively charge all these together. |
| forward/reverse frontier | Two Vec<usize> each of length 2*(ceil(S/2)+1), at most 2*(S+2) usize values total. Shared across all recursion; not one pair per depth. |
| small-side exact DP | min(side)<=64 and other<=4096 here. A single u8 table <=65*4097 =266305 bytes. No recursive call occurs while that table is live. The dependency's general 64M work threshold is not the size bound for this admitted call. |
| raw emitted operations | Every nonempty equal/delete/insert consumes a positive amount of one input. At most S operations before cleanup; use 2S+2 to cover temporary split equalities. |
| Compact cleanup | Vec<DiffOp> retained while insert/remove/swap shift operations; possible realloc old/new overlap. Cleanup may add an equality by splitting an existing consumed span. |
| Replace and Capture | Replace holds three optional scalar triples. Capture's Vec<DiffOp> grows while Compact's entire vector remains live. Conservatively allow both at 2S+2, including realloc overlap. |
| word-owned output | Vec<[u64;8]> reserved exactly S+1 records, populated while TextDiff token vectors and captured ops remain live. Later spilled serially to HUNKS before dropping this bounded Vec. |
| source and page overlap | Two 4096-byte arrays in Small, two 256-byte scanner windows + four-byte lookaheads, equality windows, constructor/build-result moves, Records transfer, current disk event, caller query/page/publication clone and path capacity. Actual generic sizes and multiple overlap copies appear in word_charge plus sequence::transient_charge. |

For 64-bit targets the following **candidate heap envelope** is charged before
any file creation or Similar entry: **16 MiB** in addition to actual word-state,
page, path and kernel sizes. Its deliberately loose partition is 8 MiB for the
largest map, 1 MiB token/remap/collision arrays, 1 MiB reduction/frontiers/DP,
4 MiB Compact/Capture plus realloc copies, and 2 MiB word output and remaining
allocator slack. The map slot budget assumes <=1024 requested allocation bytes
per distinct key including BTree nodes and split overlap, and the vector budget
assumes capacities/growth copies fit the stated partitions.

**Those standard-library layout/growth assumptions are not yet proved by the
cached Similar/bstr files.** Searched the pinned toolchain's
`C:/Users/renne/.rustup/toolchains/1.99.1-x86_64-pc-windows-msvc/lib/rustlib/src/rust/library/alloc/src/collections/btree/node.rs`
and its rustlib directory: rust-src is not installed there. No download or
rustup modification was attempted. ROOT must provide primary allocator/BTree/
Vec layout evidence (including target widths and enabled feature set), then
confirm or increase this candidate envelope. A large-looking reservation is
not substituted for that proof.

## Work envelope acquired before the indivisible call

`similar_work` charges **256*(S+4)^3*(B+4)** byte-visit/work units and one
operation, in resumable credits. Overflow is a typed failure before the call.
The envelope includes original token bytes, equality pre-scan, repeated hashing,
adversarial collision-chain equality, map navigation, reduction scans (including
the fixed 100-neighbor confusing-record scans), prefix/suffix and front-anchor
replays, every recursive middle-snake search, small-side DP, Compact quadratic
move/compare work, Capture growth copies, iteration and byte endpoint mapping.

Conservative derivation: partition recursion has at most 2S+1 nodes, since a
proper split strictly reduces the sum of ranges and leaves consume input.
At each node, prefix/suffix/anchor work plus the full unbounded raw search fits
a constant times (S+4)^2 token operations. Each full byte-token comparison costs
at most B+4 visits. Compaction can move O(S) ops for O(S) edits and replay up to
O(S) tokens; remap/preflight collision chains fit S^2 full comparisons plus
tree navigation. The factor256 dominates these fixed source loops and element
widths. This is intentionally far above practical work and **not a timing or
cancellation-latency claim**. Credit acquisition observes cancellation; the
Similar call itself is indivisible and checks no caller cancellation flag.

## Recursion and ROOT acceptance blocker

Frontiers are reused but recursive frames are not. The source bound is at most
S+1 simultaneously live conquer frames (<=8193), plus bounded outer/helper
frames and one small-side DP call. There are scalar/range/hook references per
frame; optimized and debug compiled frame sizes, stack probes, instrumentation,
target ABI and worker stack capacity are **not available from source inspection**.

ROOT already instructed that compiled frame/physical stack qualification is a
later acceptance gate. Before production entry it must bound actual frames,
admit total physical worker-stack storage, and establish sufficient configured
stack capacity. No new thread or configured stack is created in this source
package. The 4 KiB line cap, 16 MiB heap candidate, cooperative credits and
ineffective timeout establish none of those stack facts. **Similar32 is not
ready for integration until ROOT closes both heap-layout and stack gates.**

## External path and handoff

The external path uses the identical byte-word boundary rule: bstr's
`char_indices` decodes maximal invalid UTF-8 subparts, classifies the resulting
scalar with is_whitespace, and keeps original start/end byte endpoints.
`tokens::decode` uses std Utf8Error's maximal invalid-subpart length on at most
four bytes and is covered against Similar's actual byte tokenizer in authored
fixtures. Opaque atoms are separate typed nonwhitespace tokens and compare
codec, full raw length and all original bytes; literal 0xff cannot equal one.
Preserved aliases compare logical bytes. The shared external Myers solver is
exact; its edit ties/compaction are not advertised as the legacy heuristic script.

All source tests, mutation/fail-first runs, compiler/strict-lint checks and
runtime/memory/stack checks: **NOT RUN by this task**. Standalone formatter
syntax acceptance is reported separately and is not compilation evidence.
