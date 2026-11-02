# File-type registration updates

Settings registers checked types without removing existing types. Unregister selected
restores only those types; Remove all restores everything still owned by the receipt.
The current-file palette action adds that extension to the existing set.
After restart, Windows status includes custom extensions found in the retained
receipt, then verifies their complete native bindings before reporting them registered.

Every update is a new immutable version-2 intent naming the preceding active receipt.
Surviving values retain their original `before` bytes and registry type. The native
plan contains only changed entries; it never implements an update by Undo followed
by Register. Version-1 intents remain readable and may be extended.
An incomplete Apply or failed Undo durability barrier still blocks new updates
until recovery; extending a completed registration does not weaken that gate.

The worker writes and syncs a private staging file before atomic publication. Windows
uses a non-replacing, write-through move; Unix uses a non-replacing hard link followed
by directory sync. An incomplete `.pending` file does not supersede the old intent.
Old intents are retained unchanged. A crash after publication retains the complete
rollback state, including preceding owned signatures for interrupted shared XDG-file
updates. Undo recognizes these recorded signatures while refusing foreign edits.

Removed entries remain in the new intent until their original values are restored,
the backend durability barrier succeeds, and an empty synced `.updated` marker exists.
Only then does the active receipt shrink. Restart retries an interrupted subset removal.
Later edits to a successfully unregistered type are outside the active receipt.
Full Unregister retires the latest intent; superseded intents cannot become active again.

Existing limits remain: 256 requested types, 768 combined rollback values, 1 MiB per
native value, 16 MiB per serialized record and 32 transactions per journal. Updates
consume that existing transaction allowance. Oversize combined updates fail before
native writes. The native adapters and operating-system protection of default apps
are unchanged; tests use fake adapters only.
