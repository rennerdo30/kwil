# W2-A1 evidence

Base: `419823207a8fb31f9e09c21ccdc69debc02e62e3`.
Branch: `kw/w2-editing-model`. PR: https://github.com/rennerdo30/kwil/pull/12.
Commands use `--locked -j 4` and the branch-specific external Cargo cache.
No local full workspace test run or stress loop was used.

## Measurements

One-shot release measurements at `d608576e6f1300a17b3ff6f71fadb075de82d296`:

| Operation | Result |
| --- | --- |
| Insert one character, 1 cursor, 1,073,741,835-byte fixture | 0.0730 ms |
| Insert one character at 100 cursors, same fixture | 2.5805 ms |
| 100 MB edited file save, reopen and index | 85.759 ms |
| Largest observed save heap allocation | 65,536 bytes |

Typing retained 1/100 inserted bytes, damaged 1/60 visible rows respectively,
and requested no full redraw. The initial 100-cursor implementation naturally
failed the frame assertion at 22.4751 ms before batched source validation.
These numbers precede the later source-ownership, inverse-index and receipt
changes; they are not presented as measurements of those later commits.

## Focused checks

Before the receipt commit, with the committed inverse fix plus final receipt
sources (logs identify preceding HEAD `70012aa`; mutation manifests fingerprint
the exact copied Rust sources):

- View-model tests: 33 passed, one one-shot measurement ignored.
- Workspace save tests: seven passed, one one-shot measurement ignored.
- Core editing contract tests: eight passed.
- Core undo history oracle: one passed.
- Indexed inverse/rank lookup: six passed at the inverse-fix source snapshot.
- Workspace all-target strict Clippy and all-workspace formatting: passed.
- Previous command contract selection: 21 passed.
- Earlier debug viewer build, hidden GUI smoke and ConPTY TUI smoke passed at
  `d608576`; GUI PID 78840 closed after three seconds, TUI PID 23928 exited zero
  after Escape. No native editing UI acceptance is claimed.

All cargo/test child processes were bounded by the helper timeout and exited.
No UI/app source adaptation was needed. Platform save tests include CRLF,
mapped original replacement, pre-save worker snapshots, undo/save again,
read-only errors, injected write/replacement failures, and backing cleanup.
Unix mode/owner coverage and its compiling mutant run on hosted Unix runners.

## Fail-first proof

`scripts/fail-first.ps1` copies sources outside the worktree, changes one defect
at a time, requires successful compilation and named failing assertions, then
restores that copy. It never mutates the actual worktree.

External cache `E:/Development/_cache/cargo-target/kwil-w2-editing-model-mutant`:

- `proof-044800b864594db5abd73dfa454efb35`: 26 initial compiling defects rejected.
- `proof-e05449722d454419afff0d1e2fcc89ee`: four source ownership/accounting
  defects rejected, including actual model whole-document move/cut/paste reuse.
- `proof-2691df2e0ac94ff598c896313209a52a`: inverse prefix-scan defect rejected
  by scanned-byte count, without timing or a gigabyte scan.
- `proof-72efafd9e8bc47bf842f40df9260dc08`: five receipt/backpressure/spool/
  metadata/selection defects rejected; every new test failed its named mutant.

Hosted run `39838342209` at `d608576` passed Linux, Windows, macOS and formatting,
including the Unix permission mutant. Latest-head CI is handed to the parent
coordinator; that older green run does not certify later changes.

Not rerun locally: whole workspace tests, Linux/macOS runtime checks, native
editing UI, updated-head performance measurements, legacy encoding integration
and integrated journal frontend/backend tests. Their owners retain those gates.
