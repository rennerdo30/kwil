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

## YAML safety evaluation (KW-43, 2026-11-02)

Keep the current byte-preserving YAML formatter. The current stable
[`pretty_yaml` 0.6.0](https://crates.io/crates/pretty_yaml/0.6.0) is
[MIT-licensed](https://github.com/g-plane/pretty_yaml/blob/main/LICENSE), but its
default `format_text(input, &FormatOptions::default())` changes scalar values.
An isolated evaluation compared all document values before and after using
`serde_yaml` 0.9.34 (an independent libyaml parser, evaluation only):

| Input (escaped newlines) | Scalar before | Scalar after |
| --- | --- | --- |
| `a: \|+\n  text` | `"text"` | `"text\n"` |
| `a: \|+\n  text\n\n\n` | `"text\n\n\n"` | `"text\n"` |
| `a: \|-\n  text  \n  more\n` | `"text  \nmore"` | `"text\nmore"` |
| `# windows\r\na: \|+\r\n  keep  \r\n\r\n` | `"keep  \n\n"` | `"keep\n"` |

Of 16 valid fixtures, 12 retained parsed values and four did not. Passing cases
covered anchors/aliases/merge keys, multiple documents, YAML directives and
explicit/custom tags, folded scalars, flow collections with comments, multiline
quoted strings, implicit scalar types, explicit complex keys, comment-only and
empty input, a root scalar, and nested mappings/sequences. All fixture comment
texts and anchor/alias spellings were retained. Merge keys were compared as
parsed nodes, without resolving merge semantics. This finite sample is not a
proof for every YAML dialect or graph; the scalar counterexamples already fail
the adoption requirement. The deprecated semantic parser stays outside the
workspace, as does the rejected candidate; neither adds shipping dependencies.

No candidate large-file benchmark was needed after semantic rejection. Existing
formatting still runs on the document worker, checks the 16 MiB input ceiling
before collecting text, and caps output at 64 MiB. YAML retains every input byte,
including EOF, comments, indentation, trailing spaces and line endings, and
reports the existing structural-formatting limitation. An explicitly configured
external YAML formatter remains an opt-in override with the existing process
timeout and size limits.
