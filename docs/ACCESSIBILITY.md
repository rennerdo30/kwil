# GUI accessibility

The GUI uses AccessKit 0.25.2 and accesskit_winit 0.34.2 (MIT/Apache-2.0 and
Apache-2.0). The editor and owned desktop dialog each install an adapter before
showing their native window. Assistive technology activates the tree lazily;
ordinary launches do not construct accessibility snapshots until requested.

State-changing input, worker results, menu navigation and geometry changes mark
the tree dirty. Before the event loop sleeps, a dirty active tree compares its
bounded snapshot with the last published snapshot and submits only changed
nodes and the current focus. Expose/redraw events do not rebuild the tree.
Accessibility does not schedule timers, frames, disk reads or document scans.

The tree includes the menu bar and open menu commands, document tabs (including
unsaved state), a multiline editor, status text, and labeled lists for the
palette, Go to Line, file prompts and settings. Settings subpanels expose a
bounded window around their selection. Actions are dispatched on the GUI
thread and checked against the current document and displayed list identity.

Editor TextRuns come from the existing logical viewport slices or the current
prepared wrap page. Each run carries UTF-8 character lengths and a bounded map
to source positions. Only the primary caret/selection is exposed; endpoints
outside the viewport are clipped to the exposed boundary. The accessible text
is a viewport, not a virtual copy of the full document. Moving the viewport
replaces those runs. While a prepared page is pending for the same document,
input epoch and revision, the last published bounded runs and source mappings
remain available. Replacing them with an empty placeholder would invalidate
live UIA ranges even though the document text has not changed. Without a prior
matching viewport, one empty run keeps the native text provider present during
initial layout. Changed documents never reuse the retained text.
The TUI is unchanged.

## Windows inspection

Build with `cargo build -p kwil -j 4 --locked`, then run from Windows PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/inspect-accessibility.ps1 -Executable <absolute-path-to-kwil.exe>
```

Add `-WordWrap` to inspect the prepared wrapped-text path too. Add
`-WholeDocument` to use a small two-line Unicode file without a final newline
and compare TextPattern's complete text exactly with the file. Without it,
the larger fixture verifies that the exposed viewport is an exact prefix and
does not contain the full document.

The inspection uses isolated workspace/settings storage, launches only
`--hidden --gui --renderer=cpu` with updates disabled, records its owned PID,
and closes that process in `finally`. It checks UI Automation's TextPattern,
Unicode viewport text, bounded text length, selection actions and selection
events, menu actions, palette/prompt lists and the owned settings window.
It collapses a selection at document offset 4, posts a Left key to the owned
HWND, and asserts that UIA reports a collapsed caret at offset 3, emits a new
selection event, and leaves the text unchanged. It checks the exact names and
UIA roles of the Documents tab list, file tab, editor, command palette and its
query, and settings list and its query. Palette commands and settings rows must
be named ListItems. Settings are exposed as navigable list rows with their
title, value and description, rather than individual native checkbox widgets.
Both HWNDs must remain DWM-cloaked. A synthetic WM_SETFOCUS notification to the
owned hidden HWND exercises UIA focus events without calling SetFocus or
changing the owner's foreground window. This is not a real NVDA listening
test. A PrintWindow capture and the discovered UIA tree go outside the repo
under `E:/AgentData/reports/`.
Debug builds always open Welcome; the script closes that owned hidden modal
before inspecting editor actions.

## Automated Windows qualification

The hosted `windows-latest` test job runs `inspect-accessibility.ps1` against
the real debug `kwil.exe` after the smoke build. Three fresh hidden processes
check the complete short document with wrapping off and on, then a larger
bounded viewport. Each case uses a separate Windows PowerShell UIA client.
The step has a three-minute timeout and uploads available trees, logs and
PrintWindow captures as the `windows-uia-qualification` artifact, including on
failure.

The exact-text regression caught an extra newline at EOF in both the logical
and prepared text providers. Exposed text now preserves whether the final line
has a newline, using existing line-count/page metadata without reading more
document data. The focused regression also covers an empty document and one
that ends with a newline.

Coverage is limited to the Windows UIA provider contract above on the CPU
renderer. It does not complete screen-reader listening, normal foreground
keyboard/IME navigation, all settings subpanels, GPU rendering, or the native
macOS/Linux checks below. The beta accessibility limitation remains for those
unverified cases.

## Remaining native verification

- Windows: NVDA listening with real user focus and normal keyboard navigation.
- macOS: VoiceOver/Accessibility Inspector activation, text/selection and focus
  announcements, owned settings-window navigation, and non-ASCII text ranges.
- Linux: AT-SPI activation over the session bus and Orca text/selection/focus
  announcements on both Wayland and X11. The adapter's default Unix backend is
  enabled; it uses AccessKit's async-io service internally.

Hosted cross-platform compile/test checks do not replace these listening tests.
