# Sessions and separate windows

The primary window restores tabs in their previous order, including the active
tab, caret, selection and scroll position. With `files.restore_session = true`,
quitting waits for session metadata and unsaved-buffer journals to be durable.
Untitled buffers are restored too. If recovery storage fails, quitting falls back
to asking the user to save or explicitly discard unsaved work.

Complete encoded-journal transitions can be undone after recovery, within the
normal undo memory limit. The seed is the oldest available state. Recovery uses
journal transaction boundaries, rather than original typing groups or historical
selections. Old raw journals and transitions involving added spill blocks restore
their text without restoring undo history. An undo spanning an unrecorded typing
group drops that unavailable history prefix. New edits remain undoable normally.
Recovered undo and redo changes are journaled again.

A second launch sends its file arguments to the primary window. `--new-window`
opts out and creates a fresh, persistent window session. Every window keeps its
own tab order, active tab, caret, selection, scroll position and unsaved journals.
Document IDs are local to that window. A lifetime OS lock is acquired before any
session metadata or journals are read or written; a contested store is refused,
never opened concurrently or replaced with an empty session.

The primary keeps the existing `workspace/session.kws` and journals in place.
Secondary stores live under `workspace/windows/window-<identity>/`. A normal GUI
start restores the primary and launches each saved, inactive secondary as a
separate process/window. Discovery runs on the existing document worker after
first paint. Restored children bypass primary IPC and restore only their own
store; they do not recursively launch other windows. Already running secondary
windows are skipped. `--new-window` always starts fresh and does not trigger
restoration of other windows. An ordinary terminal start restores only the primary.

**Window lifecycle rule (KW-20 follow-up, 2026-11-02):** closing one window, even
while others remain open, saves its session for the next normal GUI start.
Closing the primary leaves other processes running; subsequent ordinary file
launches create a primary again. Closing a tab is still an explicit tab action
with the existing save/discard rules. The normal `files.restore_session` setting
controls hot exit in every window, including after settings reload; unavailable
recovery storage falls back to Save / Don't Save / Cancel. As in the existing
single-window path, disabling hot exit does not delete retained recovery data.

Windows reopen in sorted identity order; no foreground-window history is stored
and focus follows the platform window manager. Up to 128 secondary sessions are
retained; reaching the limit refuses a new persistent store and retains all old
data. If a child launch fails, its store remains available for the next start.

Hidden verification runs bypass the primary instance by default. Native IPC
checks may explicitly set `KWIL_INSTANCE_NAMESPACE` to a unique `kwil-test-...`
value; other namespaces are refused in hidden mode. These checks also require an
isolated absolute `KWIL_WORKSPACE_ROOT` and disable updates with `KWIL_NO_UPDATE=1`.
Persistent windows still read journal/undo preferences from their settings
profile when the recovery root is overridden; hidden checks isolate that profile too.

The Windows/Linux CI process-kill check builds with
`kwil-ui-gpu/frame-harness` and runs `scripts/test-crash-recovery.py <kwil>
<output-directory>` (under `xvfb-run -a` on Linux). It edits named and untitled
buffers through the existing hidden GUI script, waits for journal acknowledgements
and persisted session metadata, then uses TerminateProcess or SIGKILL on its own
process. Restart checks exact unsaved bytes, dirty markers, tab order, active tab
and selections. Both unchanged and externally changed disk files retain their
bytes; unverified named recovery shows its existing warning and requires Save As.
Logs, owned PIDs, isolated profiles and retained compositor captures stay in the
output directory. `--negative-control` removes only the isolated named journal
after killing the seed and must fail the recovery assertions. These checks do not
establish physical disk durability under power loss or macOS behavior.
