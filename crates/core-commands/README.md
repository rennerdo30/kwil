# Commands, keymaps and settings

Frontend-neutral data and input grammar for both frontends. No document text,
UI dependencies, threads or timers. `COMMANDS` lists stable ids, palette titles
and categories for all v1 areas, including actions whose executors arrive later.

## Input adapters

Convert a winit **logical** key or crossterm `KeyCode` into `Key`, copy the four
modifier flags into `Modifiers`, then call `KeyChord::new`. Never construct a
chord directly from a platform scan code. Handle presses (and repeats if the
frontend wants them), ignore releases. Multi-scalar winit strings and IME/text
commits belong to text input, not keymaps. Preserve Ctrl and Super separately.

ASCII letters are stored lowercase with Shift; logical punctuation already
encodes Shift (`$`, `(`, `+`), so its Shift flag is removed. In settings,
`Ctrl+K Ctrl+C` is case-insensitive shortcut notation; bare `G` means Shift+g.
Write `Plus` for the plus key and `Space` for a space. Ctrl+Plus and Ctrl+=
are different logical keys. Terminal protocols cannot represent every modified
key; frontend adapters should report those limitations rather than invent keys.

```rust,ignore
let settings = Settings::load(&Settings::path(&std::env::current_exe()?)?)?;
let mut input = KeymapEngine::new(settings.settings.keymap, &settings.settings.bindings)?;
input.set_context(Context::Editor);
match input.feed("ctrl+s".parse()?) {
    VimResult::Command(invocation) => { /* enqueue editor command */ }
    VimResult::Pending => { /* wait for the next input event */ }
    VimResult::Unbound => { /* editor text input, if applicable */ }
    VimResult::Error(error) => { /* display grammar error */ }
}
```

No frontend wiring is part of this package. A frontend must not insert a
Normal-mode Vim key merely because it is unbound. Insert-mode character/text
input is handled by the editor. Ex/search text is held by the parser; use
`input.vim().and_then(Vim::command_line)` for its current text.

## Presets and overrides

All three presets are embedded TOML. VS Code is the default; its Ctrl+D selects
the next match. Notepad++ Ctrl+D duplicates a line. The Vim preset has explicit
global/palette/diff bindings and delegates modal grammar to `Vim`.

```toml
keymap = "vscode" # also "notepad++", "vim"

# Unbind the old key and add a new key to move a command.
[[bindings]]
keys = "ctrl+s"
command = ""

[[bindings]]
keys = "ctrl+alt+s"
command = "file.save"

# Rebind only in this context and mode. Omit either for all contexts/modes.
[[bindings]]
keys = "z"
command = "motion.wordForward"
context = "editor" # editor | palette | hex | diff
mode = "normal"   # normal | insert | visual | visualLine | command
```

The last binding with the same sequence/context/mode replaces its predecessor.
User override layers win over preset data. Within one layer, resolution selects
the most specific applicable **context**, then **mode**, then the last entry.
A global user override therefore replaces an Editor-specific preset binding.
Context/mode overrides leave other contexts/modes on the preset. Empty commands
are tombstones and mask less-specific bindings, including the Vim fallback.
Invalid keys/ids reject the entire replacement map.

`Keymap::resolve(sequence, context, mode)` returns Command, Pending for a proper
prefix, or Unbound. Exact bindings win immediately over longer sequences. To
expose a longer sequence with the same prefix, unbind the shorter command. There
is no sequence timeout. `KeymapEngine` owns the pending sequence, retries an
invalid continuation as a fresh key, and cancels on context change/Escape.
`cancel_pending()` is also available for window focus changes. Replacing the
engine on reload cancels old partial input.

## Vim execution contract

`Vim` is a state machine with Normal, Insert, Visual, VisualLine and Command
modes. It handles h/j/k/l, w/b/e, 0/$, gg/G, counts, d/c/y motions and doubled
operators, x/p/P/u/Ctrl+R, i/a/o/O/I/A, v/V, dot, slash search, n/N, ex w/q/wq/s,
and inner/around word, double-quote and parenthesis objects. Register prefixes
such as `"add`, `"Ap` and `"_dw` attach a register name; the editor owns storage,
uppercase append semantics, and platform clipboard access.

Invocations carry a stable id, total count, explicit-count flag, optional motion,
register and typed argument. For example, `2d3w` emits operator.delete, count 6,
motion WordForward. `dd` uses Line; visual operators use Selection or Line.
Explicit count distinguishes `1G` from `G`. Standalone motions keep Visual mode;
visual delete/yank return to Normal and change enters Insert. Plain insert input
is unbound. Explicit rebinding of a motion/operator/insert command retains the
same modal semantics through `Vim::invoke`.

The editor interprets motions (including linewise/inclusive ranges), applies
edits, maintains registers and records each **completed** change, including
inserted text and its initiating command/count. `edit.repeat` asks it to replay
that complete transaction; no explicit count means reuse its original count,
an explicit count replaces it. This parser intentionally cannot replay edits
without the document executor. Substitution arguments preserve regex escapes,
accept escaped delimiters and alternate separators, and carry `global` and
`whole_file` flags (`:s/a/b/g`, `:%s#a#b#g`). Full Vim compatibility is outside v1.
Counts reject overflow. Command/search input is bounded to 4096 UTF-8 bytes.

## Settings and live reload hook

All groups have typed defaults. The default values are declared together in
`src/settings.rs`; the following overrides illustrate the supported schema:

```toml
[render]
remote = "auto" # auto | on | off; frontends enforce remote-mode restrictions
smooth_scrolling = false
cursor_blink = true
[update]
check = true
channel = "stable" # stable | prerelease
[font]
family = "monospace"
size = 14.0
line_height = 1.4
ligatures = true
[theme]
name = "dark"
follow_os = false
[theme.colors]
background = "#101010"
[editor]
word_wrap = false
tab_size = 4
insert_spaces = true
auto_indent = true
show_whitespace = false
trim_trailing_whitespace_on_save = false
[files]
small_file_threshold_bytes = 67108864
backup = false
restore_session = true
journal_interval_seconds = 3
[undo]
memory_limit_bytes = 67108864
persist = false
[search]
regex = false
case_sensitive = false
whole_word = false
in_selection = false
include = []
exclude = []
[compare]
ignore_whitespace = false
collapse_unchanged = true
[syntax]
full_parse_limit_bytes = 20971520
[log]
follow = false
detect_levels = true
[log.highlight_rules]
"ERROR|FATAL" = "#ff6060"
[formatters.rs]
executable = "rustfmt"
args = ["--edition", "2024"]
timeout_ms = 5000
```

Unknown keys are returned as dotted paths in `SettingsReport::unknown_keys` and
are nonfatal. Invalid known types/values, commands and keys are errors. Settings
files have a 1 MiB read cap; this is configuration, never document content.
Formatting configuration is data; workers own execution and timeout enforcement.
Themes, syntax and log settings are also data for their respective executors.

`Settings::watch(path, callback)` returns an **inactive reload hook**, without
adding notify again. Register `handle.path().to_path_buf()` with the parallel
core-workspace `FileWatcher::new(&[path], debounce, callback)`. It watches parent
directories internally, filtering to this file. Keep that watcher alive. On
Modified/Removed/Renamed, or a backend error requesting a rescan, invoke
`handle.reload()` on a worker. It suppresses duplicate reports and calls back
only after parsing and keymap validation succeed; failures retain the last
valid report. A missing file yields defaults. Use a channel/event proxy to move
reports to the UI and wake it. FileWatcher wiring and frontend application are
integration work, not implemented here.

`Settings::path(current_exe)` checks for a **file** `kwil.portable` beside the
executable, then chooses `settings.toml` there. Otherwise it uses APPDATA/kwil
on Windows, HOME/Library/Application Support/kwil on macOS, or absolute
XDG_CONFIG_HOME/kwil (falling back to HOME/.config/kwil) on Unix. It creates
nothing. Missing required environment directories are errors, without silently
falling back to the current working directory. `SettingsLocation` injects those
inputs for tests without process-wide environment changes.

## Verification

Run `cargo test -p kwil-core-commands -j 4 --locked` with the branch target dir.
`scripts/fail-first.ps1` proves all 16 contract tests reject deliberate defects
in an isolated copy. It never mutates the source worktree and records each
owned PID with a 120-second timeout. See `VERIFICATION.md` for the receipt.
