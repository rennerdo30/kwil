# Desktop integration status — 2026-10-24

This records delivered behavior separately from prepared source. A scratch receipt is not a feature delivery.

## Integrated in the current desktop change

- Settings and welcome use separate owned native windows. Native decorations provide their only caption/close button; the settings canvas has no second fake caption or Close icon.
- Dark and Light change the painted editor and chrome. The theme setting is a dropdown; manually choosing it disables follow-system. Dark uses neutral charcoal surfaces.
- Resize events retain the latest size; surface allocation and viewport work happen once on paint. The GUI compositor is optimized in debug builds without disabling debug assertions.
- Windows Open, untitled Save, and Save As use the Shell common-item chooser. Native paths enter the existing fenced save engine directly; no Windows inline-path fallback is used. Named Save keeps its destination.
- Dirty tab and app close use Windows Save / Don't save / Cancel confirmation, including when session restore is enabled. Acceptance is bound to the document, revision and workspace incarnation. Failure or cancellation retains the dirty document.
- Find and Go to Line are connected through the effective keymap, menu and palette. Find has desktop fields/buttons, worker results, Next/Previous navigation and visible hit markers. User binding overrides/unbinds retain precedence.
- Shutdown closes external-open admission while preserving accepted save completion drainage.
- Windows GPU presentation uses a disabled child HWND while the parent remains compatible with CPU presentation. Backend transitions transfer a complete authoritative image; failed GPU retirement retains resources on the hidden child and immediately repairs the CPU parent.
- Markdown Text/Preview/Split is connected, with a bounded native layout for tables, code, task lists, local PNG/JPEG images and the documented Mermaid subset. The mode controls occupy the upper toolbar and remain reachable through View in narrow windows.
- GUI Replace/Replace All submit authoritative worker transactions and restore editor ownership before acknowledging completion. Inline recipes release replacement payloads; no-op operations preserve history and recovery identity.
- Gzip signature detection precedes text decoding. Open, edit, atomic compressed save and recovery use the compressed original identity. Inline log level/time filters replace the editor rows while preserving original source coordinates.
- SVG context-menu actions, I-beam text cursors, About diagnostics/licenses, and an embedded Windows executable icon are connected.
- Editor and owned dialogs are centered and clamped to usable monitor areas. Editor normal bounds, DPI and maximized state use one bounded background persistence lane; late loading cannot override a window already manipulated by the user.
- Repository SVG icons compile to cached DPI masks for command buttons, tabs, settings categories and field controls. Windows tooltips show effective shortcuts; SVG parsing performs no runtime file I/O.
- GUI and TUI expose physical-byte hex views with bounded worker pages, byte editing, shared undo/redo, raw saves, and clean transitions back to decoded text. Dirty transport changes are refused. Recovery changes authority only after the replacement journal seed is durable; the fixed staged sibling is never selected merely because it exists.
- Binary detection now offers Hex, Text or Cancel through worker-retained source ownership before decoding/indexing. Both frontends fence input while permitting vetted read-only previews before final completion. Cancel removes an unaccepted placeholder without evicting closed history. Cold startup uses the same checked worker route.
- GUI and TUI language pickers use document-scoped choices and worker-validated extension mappings. Mapping reloads invalidate syntax without editing the document. Raw-byte views defer syntax work.
- TUI in-file Find/Replace uses the shared search and document workers. Cleanup drains accepted edits before releasing history pins and waiting for search-spool retirement; terminal cleanup failures retain their ownership and cannot become success on retry.
- GUI and TUI Find in Files use the shared bounded worker, include/exclude filters, streaming counts and revisitable result pages. Results preserve native paths and open at decoded line/column coordinates. Painted geometry and document/cursor tickets reject stale clicks. Exact match-range highlighting and folder replacement remain unfinished.
- Captured editor IME input follows its original document across ordinary text-tab activation. Document incarnation, source, cursor/revision and modal ownership fences reject stale commits. Language detection remains stable while viewport highlight spans are refreshed.

## Binary-open and journal integration evidence

The binary-open package has 20 worker/model, six GUI, ten TUI, and one once-only startup-delivery regression passes on the frozen source. The unchanged raw-file selection passes all nine cases. Four journal scheduler tests pass; restoring the earlier seed-overtaking behavior fails with the same receipt-generation gap, and restoring FIFO passes again. Seed and finish admission have separate bounded credits, so seed pressure cannot consume finish capacity.

The hidden native binary and cold-start scripts both exit successfully on artifact `AD1918DED94CB99D07A2475A6989548043921F8146F776211BC3AA86986C5187`. They cover early text painting while final completion is held, cold Cancel without reoffering, and explicit reopen as physical Hex. The early preview and binary-choice captures were visually inspected. These scripts use the hidden fallback card; actual Windows TaskDialog interaction remains unqualified.

Strict checks for the affected app, model, GUI, TUI and workspace targets, formatting, and the retained chooser/dirty-close selections pass. This is focused Windows evidence; the independent scheduler source review found no blocking issues. Hosted cross-platform checks remain a separate gate.

## Evidence

Focused Windows checks passed: 13 native/workspace tests, 11 chooser-coordination tests, one dirty-close identity test, 13 search-related GUI tests (including seven actual host-dispatch cases), 27 search-related model tests, 21 settings GUI tests, renderer CLI parsing, and warnings-denied GUI/model lib/tests Clippy.

Hidden native App search probes opened Ctrl+F, queried `value`, received exactly 60 matches and selected the match's five bytes on Enter. The second capture uses the desktop Find card. These inject events into App; they are not OS keyboard automation.

Resize measurements use six finite synthetic App resize events and real native surface painting. The earlier unoptimized candidate painted in 27–37 ms; the optimized/coalesced candidate's latest sequence was 2.6, 2.7, 2.8, 4.7, 14.8 and 3.0 ms. This does not establish real OS drag latency or GPU performance.

Actual Shell dialog appearance, overwrite UI, and native dirty-close button interaction remain unqualified. Their focused coordination tests use mocked chooser outcomes with the real workspace/save worker. Agent hidden mode refuses Shell presentation rather than leaking a visible window onto the owner's desktop.

## Still unfinished

- Placement has focused geometry/parser/file-replacement checks and a two-process hidden exact-bounds save/restart check. Real OS maximize/restore and display hot-plug remain owner/platform checks.
- Native macOS/Linux GPU execution remains unqualified. Their wgpu surface path performs full-surface presentation and does not yet meet the damage-only platform requirement. Windows retained-texture equality does not establish DWM/RDP/VNC compositor behavior.
- Complete viewport hit pages, exact folder-result range highlighting, folder replacement, and regex completeness beyond the backend's bounded match contract remain open.
- Native dialog implementations on macOS/Linux; those retain their explicit inline workflow.
- General compression beyond gzip, live log follow/rotation, compare hosts, word-wrap layout, verified external grammar loading, and signed update delivery still require implementation and acceptance. Binary offers have focused worker/host checks; real Windows TaskDialog interaction and all-platform native qualification remain separate.

No full-v1, platform, performance or release acceptance is implied.
