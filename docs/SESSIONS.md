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
opts out: it opens an independent editor with temporary worker storage and no
persistent session. It neither loads nor writes the primary session. Settings
reload cannot enable hot exit there. Closing dirty tabs requires saving, explicit
discard, or cancellation. Persistent per-window sessions are a follow-up to KW-20.

Hidden verification runs bypass the primary instance by default. Native IPC
checks may explicitly set `KWIL_INSTANCE_NAMESPACE` to a unique `kwil-test-...`
value; other namespaces are refused in hidden mode. These checks also require an
isolated absolute `KWIL_WORKSPACE_ROOT` and disable updates with `KWIL_NO_UPDATE=1`.
