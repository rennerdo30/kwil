# Formatter preservation audit after ten review rounds

Base audited: exact validated R9 candidate at HEAD `05edc5f8df69b4fa02b5ae9b5fb48f68b920a4df`, plus the R10 source
candidate. Every prior failure class below retains its safety contracts.
R6 corrects two unsafe legacy selection output assertions with explicit parent
approval; JSON and root/document XML positive selection checks remain. No test
is removed. This is a source audit and test map, not a claim
that local tests replace macOS/Linux CI or prove every language grammar.

| Failure class | Preservation boundary / current approach | Existing contracts |
| --- | --- | --- |
| R0 structured formatting, idempotence, malformed syntax and diagnostics | Raw/value-preserving JSON, markup, TOML and lexical CSS/SQL formatting; malformed input returns a line-numbered error | `contracts::{json_golden,json_indent_and_file_style,xml_golden,html_golden_and_raw_blocks,toml_golden,css_golden,sql_golden,malformed_input_has_line_number}` |
| R0 bounded admission, invalid UTF-8, output/nesting ceilings, one replacement and worker subprocess bounds | Admit size before materializing; one byte-range payload, no implicit command; explicit timeout/exit/output checks and owned process containment | `contracts::{memory_and_settings_guards,selection_is_one_replacement_without_touching_surroundings,unsupported_and_invalid_selection_errors,huge_document_small_selection_is_admitted_without_document_copy,external_stdin_stdout_on_worker,external_timeout_reaps_child,external_failures_are_messages}` |
| R1 Markdown hard breaks, fenced/indented code | No generic tokenizer: no edit, including EOF | `review_regressions::{markdown_hard_break,markdown_code_bytes}` |
| R1 HTML adjacent inline elements, images, unknown/mixed containers; XML mixed whitespace | Markup changes only admitted structural whitespace; inherited XML space is retained | `review_regressions::{html_inline_labels,html_inline_images,html_unknown_mixed_and_existing_spaces,xml_mixed_content_and_inherited_space}`; markup contracts |
| R1 CSS escaped punctuation; R2 escaped/case-insensitive URL identifiers | Escape-aware atomic identifiers; decoded recognition with original URL/function bytes emitted | `review_regressions::css_escaped_semicolon`; `round2_regressions::css_decoded_url_identifiers_keep_original_bytes` |
| R1 SQL newline-concatenated literals and comment boundaries | Keep original separators and complete proven spans; R8 refuses nested/non-nested ambiguity instead of choosing universal nesting | `review_regressions::{sql_newline_literal_concat,sql_nested_comments}`; dialect literal contract |
| R1 YAML keep-chomping and EOF | Preserve all YAML bytes; no final newline insertion | `review_regressions::yaml_no_final_break_literal`; YAML contract |
| R1 TOML mixed structural/literal endings | Structural ending restoration is separate from raw multiline literal contents | `review_regressions::toml_crlf`; TOML golden |
| R1 Windows descendant cleanup; R2 macOS zombie-only success | Owned job/group containment precedes leader reap, retaining Unix PID reservation; EPERM needs no-live-member proof | `review_regressions::external_timeout_terminates_grandchild`; `round2_regressions::successful_external_formatter_reaps_and_stops_descendant`; external contracts |
| R2 Python triple/raw/f-strings; JS templates; common language raw/text-block/long-bracket literals | Only tokenized types tidy; literal interiors/comments are protected, uncertain syntax protects EOF; Python reindent stays disabled | `round2_regressions::{python_triple_raw_and_f_strings,javascript_template_literals,common_language_multiline_literals,only_proven_code_whitespace_changes,uncertain_literals_include_eof_and_contextless_selections}` |
| R2 shell/Ruby/PHP heredocs and nowdocs | Body spans stay exact; R4 now validates the whole simple header before searching for body delimiters | `round2_regressions::shell_ruby_php_heredocs` |
| R2 unknown/data types, explicit trim, contextless selections | Unknown types receive no edits/status; explicit trim remains a warned separate operation; selections without context stay exact | `round2_regressions::{unrecognized_types_never_tidy_or_add_newline,explicit_trim_is_separate_warned_and_preserves_endings,uncertain_literals_include_eof_and_contextless_selections}`; selection/admission contracts |
| R3 PHP heredoc terminator followed by code | Stop at the label token, then tokenize following expression strings | `round3_regressions::php_heredoc_closes_before_following_expression` |
| R3 PHP attributes mistaken for hash comments | Preserve uncertain `#[...]` suffix, including multiline literal and EOF | `round3_regressions::php_attributes_are_not_hash_comments` |
| R3 JS/TS Unicode line-comment terminators | Comments stop at CR, LF, U+2028 or U+2029, exposing following literal openers to the scanner | `round3_regressions::javascript_unicode_line_terminators_end_line_comments` |
| R3 Shell CR argument bytes / false hash-comment boundary | Only LF is structural; CR is neither a line ending nor shell word separator | `round3_regressions::shell_carriage_returns_are_argument_bytes` |
| R3 SQL hash-comment/operator/temp-name dialect ambiguity | Refuse unquoted/uncommented `#` with an explicit dialect error and no applied edit | `round3_regressions::sql_hash_dialects_are_preserved_or_refused` |
| R3 opt-in reindent amplifies hidden literals | Preservation also applies with reindent, trim, normalization and final-newline options enabled | `round3_regressions::affected_boundaries_remain_safe_with_all_tidy_options` |
| R4 Unicode SQL name exposes an ASCII keyword suffix | Consume the complete atom first; refuse non-ASCII dialect rules before any keyword classification; never split a name | `round4_regressions::sql_unicode_identifier_atoms_are_not_split_into_keywords` |
| R4 physical LF in an unparsed heredoc header mistaken for lexical NEWLINE | Changed approach: only a fully validated simple header admits body scanning; any other token protects the entire suffix, independent of fixture text | `round4_regressions::{shell_heredoc_header_requires_a_proven_lexical_newline,uncertain_heredoc_headers_preserve_literals_with_every_tidy_option}` |
| R4 conservative SQL admission must retain proven Unicode literal/comment support | Refusal applies only outside recognized quote/comment spans | `round4_regressions::sql_unicode_literals_and_comments_still_format` |
| R5 SQL parameter/variable names expose reserved words | Refuse ambiguous `:`, `@`, `?` introducers outside literal/comment spans before they can bind a keyword-looking name | `round5_regressions::sql_parameter_names_are_not_keyword_tokens` |
| R5 Oracle alternative quoting mistaken for ordinary quotes | Refuse atoms adjoining quote delimiters (optionally via `&`) without inferring a dialect; no formatted replacement is returned | `round5_regressions::sql_prefixed_literals_are_preserved_or_refused` |
| R5 C++ spliced block-comment terminator hides a later raw string opener | Backslash/trigraph syntax in a C/C++ block comment protects the entire uncertain suffix through EOF; raw-byte comment termination is no longer guessed | `round5_regressions::cpp_spliced_block_comments_cannot_hide_raw_string_openers` |
| R6 SQL backslash quote boundaries depend on standard-conforming vs escape-enabled dialect | Refuse backslashes inside quoted tokens without returning an edit; R9 also refuses unproven dollar boundaries | `round6_regressions::sql_backslash_quote_ambiguity_is_preserved_or_refused` |
| R6 generic selection ends before a physical line ending and joins tokens | Preserve trailing selected separator bytes at that boundary; use actual following bytes to permit physical-EOL trim; preserve clipped CRLF components too | `round6_regressions::generic_selection_does_not_trim_token_separator_at_partial_line_end`; `selection_boundaries::{selected_crlf_prefix_does_not_normalize_into_an_extra_line_break,selection_uses_actual_following_bytes_to_allow_physical_line_end_trim}`; corrected selection contract |
| R6 nested XML selection loses inherited xml:space | Nonzero-offset/rootless fragment selections preserve bytes with a context notice; complete root/document selections retain positive formatting coverage | `round6_regressions::xml_selection_does_not_lose_inherited_space_context`; corrected root XML/JSON selection positives |
| R6 Java Unicode preprocessing changes comment/literal delimiters before raw-source scanning | A Java Unicode escape introducer preserves the entire compilation unit when preprocessing context is unproven | `round6_regressions::java_unicode_preprocessing_cannot_hide_text_block_openers` |
| R7 SQL -- without following whitespace/control hides a quote under a different dialect | Refuse the entire request when a code-level -- boundary is unproven; shared comment boundaries and atomic quoted/comment spans remain supported | `round7_regressions::{sql_mysql_minus_operators_cannot_hide_a_literal_opener,sql_comment_boundaries_are_proven_without_entering_literals_or_comments}` |
| R7 PHP mixed-case halt keyword exposes opaque trailing data to tidy | Match the halt introducer ASCII-case-insensitively outside already-proven comments/literals; preserve the suffix through EOF, with all tidy settings | `round7_regressions::{php_mixed_case_halt_compiler_preserves_payload_eof,php_halt_case_variants_protect_data_without_disabling_code_tidy}` |
| R8 SQL universal comment nesting hides a literal opener under SQLite-style first-close rules | Refuse a nested opener inside a SQL block comment without choosing either grammar; nonnested comments and marker text inside proven literals remain supported | `round8_regressions::{sql_sqlite_comment_closure_cannot_hide_a_literal_opener,sql_nested_boundaries_are_refused_without_disabling_proven_atoms}`; corrected R1/R7 cases |
| R8 PHP prefix without an opening-tag boundary exposes inline output | Require case-insensitive long tag followed by proven space/tab/CR/LF, or the existing short echo tag; otherwise protect all inline bytes and EOF | `round8_regressions::{php_prefix_without_a_tag_boundary_is_inline_output,php_requires_proven_opening_whitespace_and_retains_benign_code_tidy}` |
| R9 SQL dollar parameters/quotes and bracket identifiers/array expressions hide literal openers | Refuse code-level dollar/bracket syntax and dollar-bearing complete atoms before keyword rewriting; marker bytes inside proven quotes/comments remain supported | `round9_regressions::{sql_sqlite_dollar_parameters_cannot_hide_literal_openers,sql_boundary_proof_retains_common_quotes_comments_and_complete_atoms}`; retained/corrected contracts, R1, R7, R8 cases |
| R9 PHP physical heredoc label occurs inside interpolated script string/comment/braces | Track nowdoc vs heredoc; unescaped dollar syntax refuses the entire edit before trusting a later label; escaped dollar/plain heredoc and noninterpolating nowdoc remain positive | `round9_regressions::{php_heredoc_terminators_require_outer_literal_context,php_heredoc_interpolation_uncertainty_is_bounded_and_nowdoc_stays_literal}` |
| R10 SQL CR versus LF-only line-comment termination hides literal context | Refuse a CR encountered before LF/EOF in an admitted line comment; LF-only/ordinary EOF and quoted/block CR forms remain positive | `round10_regressions::{sql_cr_comment_terminator_cannot_hide_literal_context,sql_comment_boundaries_preserve_common_lf_eof_and_quoted_cr}` |
| R10 shell physical marker is continued body data, or joined fragments synthesize a logical marker | Stream logical delimiter comparison in constant space, with escape parity, quote mode, logical leading-tab stripping and per-heredoc FIFO state; raw bytes stay protected | `round10_regressions::{shell_continued_physical_delimiter_is_body_data,shell_logical_delimiters_keep_parity_tabs_queue_cr_and_eof_contracts}` |

The existing malformed-input, UTF-8, input/output cap, nesting, selection offset,
external timeout/error and one-replacement contracts remain part of the focused
formatter suite. R8 changes neither the process-tree code nor the public admission
ceilings. The parent retains independent review, cross-platform CI and merge.

All 66 previous named tests and counts are retained. With direct ROOT approval
(2026-10-23, message `081f417a-202e-45be-b811-9f7b40312a20`), two unsafe positives
now assert dialect refusal before a replacement is returned:

- R1 `sql_nested_comments` formerly required `SELECT /* outer /* inner */ 'ignored */ 1;\n`.
  That expectation assumed universal nesting. Its original input remains, now
  requiring an explicit ambiguity error, plus positive nonnested/literal cases.
- R7's benign atom array formerly required formatting `/* outer /* --2 */ 'from' */ 1`.
  The exact atom remains in the same test as an explicit refusal case; every other
  atom and assertion retains its original formatting expectation.

This corrects the contract instead of selecting SQLite, PostgreSQL or a fixture
special case. All other prior test files are unchanged. Four R8 tests add exact
counterexamples, dialect-boundary/tag variants and positive safe formatting.
Historical de1ef148 plus independent universal-nesting/prefix faults must compile
and fail behaviorally before this source-only candidate is accepted. No Cargo
execution has run yet; ROOT retains the finite grant and independent review.

R9 retains all 70 earlier named tests and their original inputs. ROOT approved
only these additional unsafe SQL expectation corrections (2026-10-23 R9 brief,
plus bracket extension message `0e71f0d8-503e-4311-bd6d-4e82a859aeaf`):

- `contracts::sql_preserves_dialect_literals_identifiers_and_comments`: original
  combined dollar/bracket input now refuses; independent bracket refusal and
  common quote/comment formatting positives are added. Malformed-input line
  assertion is unchanged.
- `review_regressions::protected_literals_idempotent`: only its SQL dollar case
  now refuses; original input stays and common SQL idempotence is added. All
  other languages keep their original assertions.
- R7/R8 atom matrices: only original `$$`, `$tag$` and bracket atoms change to
  explicit dialect refusal. The original arrays, nested/comment/quote cases and
  named-test counts stay intact. No other prior expectations change.

ROOT additionally approved the exact retained R1 historical harness accounting
correction (message `b63d0312-2d0a-4613-99f6-56b5bff6bb60`): 13 behavioral
failures / 0 passes, formerly 12 / 1. The original SQL-dollar idempotence input
now detects that historical defect, while independent common-SQL idempotence
remains positive in the same named test. This changes no input/name or assertion
beyond the approved R9 SQL expectation; the harness has not been re-executed in
this source-only round and is outside the proposed six-process R9 grant.

Four new tests put exact dollar/ARRAY literal and PHP raw-body byte assertions
before broad variants. Historical 9dcc8e0 and independent universal SQL quotation
and PHP interpolation-admission faults must compile and fail those assertions.
Positive formatting/selection cases include ordinary SQL atoms/quotes/comments,
plain heredoc, nowdoc, escaped dollars and all 16 combinations of tidy switches.
Nested interpolation, script quotes/comments/braces, escaped/open headers and
EOF are covered by explicit uncertainty refusal or the retained suffix guard.

Remaining work is explicit: no SQL dialect is selected/proven by this scanner;
it does not validate every vendor grammar. PHP interpolation needs a proper
bounded script/interpolation lexer to format those constructs rather than refuse.
Refusals establish interim preservation safety only, not complete any-file
formatting, worker adoption, production integration, native or v1 acceptance.

R10 retains all 74 earlier named tests AND their exact source bytes: no earlier
expectations, names, inputs or harness counts change in this round. Four new
tests require actual significant-byte preservation before broader refusal/positive
matrices, plus full-context/contextless shell selections and all16 tidy masks.
The shell audit covers odd/even escape parity, multi-fragment logical markers,
quoted markers, tabs/<<-, multiple queued heredocs, CR/backslash-CRLF, open
headers, unknown delimiter syntax and incomplete logical EOF. Unknown headers
continue protecting the suffix; a backtick command substitution cannot prove a
literal delimiter. Complete proven logical delimiters expose positive code tidy.

Historical proof uses exact pre-edit R9 source bytes, SHA-pinned outside the
repository (R9 was uncommitted on HEAD05edc5f8). Independent SQL CR and shell
logical-boundary bypasses must compile and fail two byte assertions each. The
finite proposal is freshfour, historicalfour fails, SQLtwo fails, Shelltwo fails,
restored fresh78 integration tests, strict all-target Clippy; own fmt separate.
No Cargo execution has run for R10. Earlier source/runtime receipts remain
separate; PHP interpolation, full any-file formatting and parent adoption gates
remain open. No dialect setting, dependency, API or manifest change is added.
