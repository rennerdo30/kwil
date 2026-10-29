# Formatting

Format Document and Format Selection are available in the GUI and TUI palette.
Formatting runs on the document worker and is a single undo step. The document
is temporarily busy while formatting; switching tabs is allowed.

JSON, XML/HTML, YAML, TOML, CSS and SQL use the existing conservative built-in
formatters. Other tokenized types use safe tidy where supported. Rust defaults
to `rustfmt`, Python to `black`, and JavaScript, TypeScript, TSX and Markdown to
`prettier`. These tools must be installed separately. An explicit language entry
overrides the default or built-in formatter:

```toml
[formatters.python]
executable = "black"
args = ["--quiet", "--stdin-filename", "{file}", "-"]
range_args = ["--line-ranges", "{start_line}-{end_line}"]
timeout_ms = 5000
```

Use canonical language ids from the language picker. `executable` can be an
absolute path; `args` are separate arguments, without shell parsing. The tool
receives UTF-8 on stdin and must return the complete formatted input on stdout.
Missing-tool messages name `formatters.<language>.executable` and `args`.

Selection formatting expands one selection to complete lines, excluding a line
whose start is exactly the selection's end. Multiple selections require choosing
one selection first. External tools need `range_args`; an empty list means the
tool does not support ranges. Rustfmt's default has no range support. Prettier
uses `--range-start {start_utf16} --range-end {end_utf16}`. Python's default uses
Black's line ranges. If a tool expands its edits outside the selected lines,
Kwil refuses the result and suggests selecting the entire statement or formatting
the document. Unselected bytes are always preserved.

Argument placeholders (in both `args` and `range_args`):

- `{file}`: document path, or an untitled filename with the language's extension.
- `{start_byte}`, `{end_byte}`: zero-based UTF-8 byte offsets, end exclusive.
- `{start_utf16}`, `{end_utf16}`: zero-based UTF-16 offsets, end exclusive.
- `{start_line}`, `{end_line}`: one-based inclusive line numbers.

The existing formatter limits apply: 16 MiB input, 64 MiB output and at most
120 seconds per process. Invalid UTF-8, process errors, timeouts and unsupported
selections preserve the document and undo history. Unsupported built-in ranges
report that limitation. Settings reloads apply to the next formatting command.
