# Editing model

`Document` owns indexed byte storage and logical revisions. `Editor` adds
cursors, selections, motions, edit transactions and damage. Construction,
indexing, saving, explicit whole-document trim and other whole-document bulk
commands belong on workers. Drawing and OS key/clipboard handling remain with
the frontends. Unicode columns currently count scalars and tab stops; byte
positions are also exposed. Encoding remains an explicit UTF-8 seam for KW-10.

Each multicursor command is one transaction. Typing bursts and Vim insertion
sessions coalesce while their replacement spans remain compatible. Cursor
history and replacement metadata count against the core undo memory limit.
Large line operations retain immutable piece ranges rather than copying their
contents. Literal occurrence search, bracket matching, cursor count, column
sampling and repeat payloads have explicit bounds.

Dot repeat admits once and stages its sub-operations in one transaction. One
completed command publishes one revision/undo entry/receipt; a failure restores
text, cursors, modes, registers, repeat state and history. Damage merges across
every sub-operation, transporting dirty rows through later viewport scrolls.
Replacement composition merges touching/overlapping net spans and retains at
most 4096 disjoint spans. A command exceeding that explicit metadata limit is
refused and rolled back in full. Repeat counts through 4096 are supported;
larger counts are refused explicitly, never silently truncated. There is no
file-byte cap and no UI-thread spool I/O.

## Save and undo

Call `save_request`, send the returned immutable snapshot to
`kwil_core_workspace::save_snapshot` on a worker, then call `finish_save` with
its result. Request creation does no document scan. The worker streams a
same-directory temporary, preserves metadata, syncs it, atomically installs it
and reopens/indexes the new original. Matching live versions rebase in constant
time; older worker snapshots and undo roots retain their original file objects.
No saved version is loaded into a file-sized resident buffer.

On Unix, `rename` installs the new file and directory `fsync` persists its name.
The old inode stays mapped until references end. Owner/group and mode are
preserved; failure aborts before replacement. Extended ACLs/xattrs are not
copied by this implementation. On Windows, originals use delete sharing;
`ReplaceFileW` atomically installs the temporary and renames the mapped original
to a reserved hidden sibling. ACL/owner preservation additionally attempts an
explicit security descriptor copy and reports best-effort failures. New files
use `MoveFileExW` with write-through and refuse a concurrently created target.
No application-level rename-away gap is introduced. An OS partial failure
retains ambiguous recovery files and reports their presence.

Windows backing names use `.kwil-old-<pid>-<counter>` and a companion `.owner`
marker containing Kwil magic, process ID, original name and file identity.
Last-reference cleanup closes the mapping/file before deleting the backing and
marker. Opening workers clean only marker/identity-matching backings whose PID
is gone, and only while the destination exists. Live PID, foreign, mismatched
and ambiguous recovery files remain untouched. A crash may leave these siblings
or `.kwil-save-*` temporaries; an original missing after an OS partial failure
requires recovery before reopening. Recognizable backing cleanup never removes
the only surviving original.

Trim-on-save runs during the existing streaming worker pass and always skips
Markdown. It spools exact deletion offsets to a small-record temporary, with
constant resident memory. The completed mapped trimmed root is installed as an
undoable edit only if its input revision is still current. A stale transformed
completion leaves the live document dirty and emits no edit receipt.

## Journal receipts

Install `EditObserver` before edits. A callback only admits a receipt to an
ordered bounded worker queue; it performs no file I/O. Return `Err(receipt)`
unchanged when full. The model retains exactly one rejected receipt; subsequent
document mutations return `WouldBlock` until `flush_edit_receipt` admits it.
The already completed edit succeeds and must not be retried. Sequence numbers
are contiguous producer admission IDs, while before/after revisions are actual
model IDs and can decrease on undo. Initialize consumers using
`next_edit_sequence` and the current document revision.

`replacement_count` is constant time. On the journal worker,
`for_each_replacement` yields exact intermediate-coordinate byte replacements
in serialized order. `SnapshotInsert::write_to` streams insertion spans. Commit
one outer journal batch only after every replacement succeeds; a partial batch
must never become durable. No prefix/suffix diff or file-sized recipe array is
required. Recipes survive undo, redo, save and history eviction through their
snapshot references. Bulk row recipes regenerate lazily; worker-trim recipes
read the Arc-owned deletion spool, deleted after its last consumer drops.

A matching transformed save completion that encounters backpressure is retained
by the model. After admission, call `finish_pending_save` and present its returned
damage. New edits are blocked until that retained completion is handled.
Ordinary and stale completions still settle without publishing fake edits.
