# 0002: Text engine: our own (`core-text`), not `qem`

Status: accepted by the owner 2026-10-23 (M0 spike, Pair Desk KW-1).

## Decision

Kwil keeps its own text engine in `crates/core-text`: a mapped file (read fully below 64 MB), the first screen from the raw bytes, and a parallel sparse line index built in the background. The M2 piece tree is built on top of it. `qem` stays the benchmark baseline only.

## Numbers

1 GB / 20,353,527-line log fixture, warm page cache, owner PC; each engine in a fresh process, 5 runs after a warm-up (`spike/textbench`).

| Measure | core-text | qem 0.8.0 | Budget (spec §3) |
|---|---|---|---|
| Open + first screen | **0.09 ms** | 60 ms | ≤ 40 ms |
| Line index | **~105 ms (≈ 10 GB/s, 8 threads)** | 536 ms (2.0 GB/s) | ≥ 2 GB/s |
| Jump to line 15,000,000 (60 lines) | **0.003 ms** | 0.012 ms | ≤ 1 frame |
| Private memory | **4 MB** | 85 MB | ≤ 64 MB |
| Under a hard 64 MB working-set cap (KW-40) | open 0.1 ms, index ~3.9 GB/s, correct | not run | — |

Both engines agree on the line count and on the text at line 15M.

## Caveats

- `qem` already edits (piece table, sidecar edit log). The `core-text` prototype only views. The M2 piece tree must keep these numbers: an edit is O(log n), snapshots are free, and no file-sized allocation (KW-16, KW-40).
- The 64 MB-cap test still had Windows' standby cache behind it. A real low-RAM machine reads at disk speed; CI needs a small-RAM job with a 4 GB file (KW-33, KW-40).
- Warm cache only; cold-cache numbers need an admin flush or a fresh boot.

## Design rules carried into M1/M2

- Sparse checkpoints (one per 1,024 lines, plus chunk borders); lookup = binary search + `memchr` over at most ~2,048 lines.
- The index must extend on append without a rescan, and detect truncation and rotation (KW-41 log watcher, KW-9).
- Mapped files need the watcher and size/mtime re-check from spec §5 before region reads (KW-9).
