# Theme backend (KW-37)

This package provides data, worker and cache APIs. GUI/TUI selection commands,
palette hover wiring, settings persistence, OS events, syntax scope adapters and
one-full-redraw damage handling remain coordinator integration work. No native
rendering or theme-switch speed claim follows from these contracts.

`theme::Theme::builtin(Scheme)` constructs static data without source parsing.
The corresponding native files are `crates/core-commands/src/themes/{dark,light}.toml`;
focused tests require them to equal those constructors. Dark editor, gutter and
status colours preserve the existing frontend constants, including background
`#0d1424` and foreground `#d6e4f0`. No light palette existed in the provided base;
the light data is new and needs parent visual acceptance. Geometry, fonts,
shaping, settings and palette files are outside this package.

## Native format

```toml
version = 1
name = "Example"
base = "dark" # dark or light; missing properties inherit this built-in

[ui]
"editor.background" = "#0d1424"
"editor.foreground" = "#d6e4f0"
"selection.background" = "#30588088"

[syntax.string]
foreground = "#a8d7a0"
font_style = "italic"

[syntax."string.quoted"]
font_style = "" # explicitly clear inherited font flags
```

Unknown fields and roles, duplicate TOML keys, invalid colours, unsupported
versions, control characters in names and complex scope selectors are errors.
`UiRole::ALL` is the authoritative fixed UI role list. Colours accept `#RGB`,
`#RGBA`, `#RRGGBB` and `#RRGGBBAA`; bytes are straight sRGB RGBA. Font flags are
bold, italic, underline and strikethrough. Absent properties inherit; empty
font style explicitly resets. A scope is an ASCII dotted name, not a TextMate
selector expression. Ancestors cascade at dot boundaries. `string` matches
`string.quoted`, never `stringish`. Scopes are preserved by name, not flattened
into the core syntax engine's broad token classes; the frontend syntax adapter
must provide the actual scope (or explicitly disclose a reduced class mapping).

Each UTF-8 source is at most 1 MiB; a theme has at most 1,024 scopes, each at most
128 bytes, and a printable name of at most 128 bytes. The catalogue includes two
built-ins plus at most 254 user `.toml` files. A directory scan visits at most
1,024 entries, without recursion. Oversized catalogues reject the whole reload
and keep the previous catalogue; malformed changed files keep their last good
theme and report a diagnostic. Removed files leave the catalogue. An unavailable
selected theme retains the last valid committed theme/cache with an error.
That committed fallback is separate from the displayed hover preview; cancel
always publishes the cancelled session, even after its source has been removed.

## Startup and worker seams

1. Before first frame, use a built-in or call
   `theme_cache::read_active_cache(explicit_path)`. Missing cache returns `None`.
   Invalid cache returns an error; use a built-in and report it after first frame.
   The reader has no source fallback and invokes no TOML, JSON or XML parser.
2. After first frame, start `theme_worker::ThemeWorker` with explicit absolute
   `ThemeWorkerConfig` paths, a `ThemeSelection`, OS `Scheme` and an event-loop
   wake callback. The cache must be outside the watched directory. This code
   does not discover owner settings paths, read the registry or alter environment.
   If a valid cache supplied the displayed theme, use `start_cached` to seed the
   worker with it; invalid or deleted source files retain that active theme.
3. `request()` accepts typed Preview, Cancel, Apply, Select, OsChanged, Reload and
   Import actions. Preview changes only display state. Cancel restores the
   committed selection. Apply commits the preview and stops OS follow. Select
   can choose Named or FollowOs with explicit dark/light IDs. OS changes update
   the committed side of FollowOs even during a preview; cancel resolves the
   latest OS side. The host persists `snapshot.session.selection()` to its own
   settings system when appropriate; this backend never edits that system.
4. `snapshot()` clones one immutable Arc. The host compares active theme content
   to its displayed theme: one actual change means one full redraw, while a
   catalogue-only diagnostic update does not require a full present. The worker
   callback only wakes; it never draws. Both frontend adapters are still needed.

The theme worker has one desired-state slot and one snapshot slot. Requests
reduce to complete selection state before coalescing, so Preview then Apply
cannot lose Preview. A newer Import replaces an unstarted Import payload.
Already-started imports may finish creating their explicitly requested new
theme; their bounded diagnostics remain visible. Parsing, imports, source I/O
and cache writes occur only on the worker. Native `FileWatcher::directory`
invalidations trigger a bounded rescan, including creates, removes and renames;
overflow/errors invalidate everything. It uses the existing watcher service's
fixed helper thread, plus one theme worker, without a polling loop or idle timer.
Obsolete results cannot publish or wake the UI. Cache writes are serialized;
a request arriving during an atomic write is handled next and replaces that
committed cache before shutdown drains the latest pending state. Dropping joins
the worker and its watcher; host shutdown must account for an in-flight OS I/O.
Paths should refer to a local configuration directory rather than slow network
storage. No cancellation of blocked OS disk I/O is claimed.

`Theme::for_remote()` returns a renderer copy with every UI and syntax background
composited to alpha 255 over an opaque editor base. Stored RGBA and foreground
alpha remain unchanged. GUI and TUI must explicitly use that adapter in remote
mode. The adapter changes colours only, with no geometry, effects or animation.

The binary cache is capped at 256 KiB. It has magic, a fixed schema version,
exact payload length and an FNV-1a corruption checksum (not an authenticity
signature), followed by bounded UTF-8 strings, IDs, colours and syntax records.
All lengths, tags, duplicate scopes and trailing bytes are checked before use.
The startup reader opens without following final symlinks, uses nonblocking
Unix opens, and validates the opened handle before reading. FIFO/device/symlink
cache paths cannot cause a blocking FIFO open before the type check.
The worker writes committed state, never hover previews. It uses an exclusive
same-directory temp file, sync and atomic replacement (MoveFileExW on Windows,
rename plus directory sync on Unix). Failures are surfaced; no target is
deleted as a replacement strategy. Import output uses atomic hard-link
create-if-absent, so an existing/racing user theme cannot be overwritten.

## Imports and dependencies

Import takes captured bytes, an explicit base Scheme for unspecified properties,
an ImportFormat and a new safe ID. It never opens a supplied source path,
include, remote URL, owner file, settings file or registry key. Imported files
are worker-written native TOML; IDs allow only 1..64 ASCII alphanumerics, `_` and
`-`. Source reads reject symlinks and nonregular files; no-follow file opens
also protect final-component replacement. Parent directory policy belongs to
the host's explicit location resolver.

The accepted subset is deliberately conservative:

- VS Code: strict JSON objects, `name`, `type` dark/light, string metadata
  `$schema`/`uuid`/`author`, mapped UI `colors` and inline `tokenColors` arrays.
  Colour mappings are explicit in `theme_parse.rs`. Rules support dotted scopes,
  comma unions or arrays, foreground/background and fontStyle. Repeated scopes
  merge properties in source order. Includes, external token files, JSONC
  comments/trailing commas, semantic tokens, unknown workbench colours, per-token
  font size/family/line height, contextual/exclusion/wildcard selectors and
  unknown properties error rather than disappear. Duplicate JSON keys error.
- TextMate: UTF-8 XML 1.0 plist dictionaries/arrays/strings, name/uuid/author and
  settings rules. Global colours support background, foreground, caret,
  selection, invisibles and lineHighlight. Scoped styles use the same simple
  scopes/flags. XML nesting is capped at 16 and value nodes at 16,384. DTDs
  (including the common plist DOCTYPE), external/undefined entities, unsupported
  value types and attributes, duplicate keys and trailing documents error.
  XML predefined/numeric character references are decoded without an entity
  resolver. A DTD must be removed from a trusted source before importing.

Primary format references checked 2026-10-23:
[VS Code theme colours](https://code.visualstudio.com/api/references/theme-color),
[VS Code token schema](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/services/themes/common/colorThemeSchema.ts),
[TextMate themes](https://macromates.com/manual/en/themes).
These specify broader formats; the unsupported cases above remain explicit.

Existing serde/TOML/watcher dependencies are reused. Additive direct dependencies:

- `serde_json = 1.0.154` (latest stable checked 2026-10-23; released 2026-10-11),
  [primary release metadata](https://docs.rs/crate/serde_json/1.0.154),
  [MIT licence](https://github.com/serde-rs/json/blob/master/LICENSE-MIT).
- `quick-xml = 0.41.0`, no default features, MIT, reuses the base lock and local
  published crate's `Cargo.toml`/`LICENSE-MIT.md`. Primary upstream
  [source](https://github.com/tafia/quick-xml) and
  [licence](https://github.com/tafia/quick-xml/blob/master/LICENSE-MIT.md).
  Latest stable is 0.42.0 (2026-08-22); the parent explicitly chose locked 0.41.0
  to avoid an unnecessary graph update for this package. No 0.42-only API is used.

Both are already in the workspace dependency graph. This adds direct edges,
not a new runtime or proc-macro dependency; no build-cost benchmark was run.
Cargo.lock normalization belongs to the parent until it explicitly grants it.

## Focused verification

Only run after the coordinator grants the shared Cargo lane:

```powershell
pwsh -NoProfile -File crates/core-commands/tests/check-theme-mutations.ps1
pwsh -NoProfile -File crates/core-commands/tests/check-theme-r1-mutations.ps1
cargo test --locked --offline -j 4 -p kwil-core-commands --lib theme -- --test-threads=1
cargo test --locked --offline -j 4 -p kwil-core-workspace --lib watcher::tests -- --test-threads=1
```

The mutation script compiles each broken source independently and requires its
named test to assertion-fail (compiler errors cannot count). It restores owned
source bytes in `finally`; logs name the commit first and record the launched
Cargo PID and timeout. It never mutates a lockfile or cleans shared caches.
Fixtures use only isolated test directories and inline/checked-in source data.
The worker contracts include a suspended obsolete request before cache admission
and a separate suspension before snapshot admission, so one guard cannot mask
the other's defect. Native watcher events in these focused filesystem tests are
not editor/player/renderer validation. Full suites, benchmarks, native GUI/TUI
captures, OS follow UI behavior and remote present budgets are parent gates.

R1 adds native partial-style inheritance and committed-source-deletion during
preview contracts. Separate mutants target the committed colour fallback and
cancelled-session publication. A Unix-only subprocess contract checks direct
FIFOs, symlink FIFOs and symlinks to valid regular caches; each owned child has
a two-second timeout and is killed/reaped on timeout. Two independent Unix
mutants remove nonblocking and no-follow flags respectively. Windows execution
cannot validate those Unix cases; the runner reports them as NOT RUN there.
