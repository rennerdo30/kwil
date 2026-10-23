# Desktop integration status — 2026-10-24

This records delivered behavior separately from prepared source. A scratch receipt is not a feature delivery.

## Integrated in the current desktop change

- Settings and welcome use separate owned native windows. Native decorations provide their only caption/close button; the settings canvas has no second fake caption or Close icon.
- Dark and Light change the painted editor and chrome. The theme setting is a dropdown; manually choosing it disables follow-system. Dark uses neutral charcoal surfaces.
- Resize events retain the latest size; surface allocation and viewport work happen once on paint. The GUI compositor is optimized in debug builds without disabling debug assertions.
- Windows Open, untitled Save, and Save As use the Shell common-item chooser. Native paths enter the existing fenced save engine directly; no Windows inline-path fallback is used. Named Save keeps its destination.
- Dirty tab close uses Windows Save / Don't save / Cancel confirmation. Acceptance is bound to the document, revision and workspace incarnation. Failure or cancellation retains the dirty document.
- Find and Go to Line are connected through the effective keymap, menu and palette. Find has desktop fields/buttons, worker results, Next/Previous navigation and visible hit markers. User binding overrides/unbinds retain precedence.
- Shutdown closes external-open admission while preserving accepted save completion drainage.

## Evidence

Focused Windows checks passed: 13 native/workspace tests, 11 chooser-coordination tests, one dirty-close identity test, 13 search-related GUI tests (including seven actual host-dispatch cases), 27 search-related model tests, 21 settings GUI tests, renderer CLI parsing, and warnings-denied GUI/model lib/tests Clippy.

Hidden native App search probes opened Ctrl+F, queried `value`, received exactly 60 matches and selected the match's five bytes on Enter. The second capture uses the desktop Find card. These inject events into App; they are not OS keyboard automation.

Resize measurements use six finite synthetic App resize events and real native surface painting. The earlier unoptimized candidate painted in 27–37 ms; the optimized/coalesced candidate's latest sequence was 2.6, 2.7, 2.8, 4.7, 14.8 and 3.0 ms. This does not establish real OS drag latency or GPU performance.

Actual Shell dialog appearance, overwrite UI, and native dirty-close button interaction remain unqualified. Their focused coordination tests use mocked chooser outcomes with the real workspace/save worker. Agent hidden mode refuses Shell presentation rather than leaking a visible window onto the owner's desktop.

## Still unfinished

- Centering/clamping to monitor work areas and persisted normal/maximized geometry.
- Actual onscreen GPU presentation. The existing GPU path maintains a backing texture; CPU still presents. Renderer CLI/settings selection is preparation, not GPU completion.
- Markdown Text/Preview/Split host integration.
- Repository SVG icon system throughout the editor.
- Search replacement transaction execution, complete viewport hit pages, and regex completeness beyond the backend's bounded match contract. Find navigation is working; these gaps are not full search/replace acceptance.
- Native dialog implementations on macOS/Linux; those retain their explicit inline workflow.
- General compression/log/highlighting and other retained worker packages require their own integration checks.

No full-v1, platform, performance or release acceptance is implied.
