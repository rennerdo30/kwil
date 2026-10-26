# Beta candidate

**0.1.0-beta.1** is prepared as an unpublished prerelease draft, not an RC.
V1 remains the full scope in [SPEC.md](SPEC.md).

## Current candidate

- Tag `v0.1.0-beta.1` points to
  `904e5d586f18291ea5a0a0f2451d689b119e2f27`.
- [Hosted CI 40297828466](https://github.com/rennerdo30/kwil/actions/runs/40297828466)
  passed on Windows, macOS and Linux, including full workspace tests, strict
  checks, formatting and the configured smoke checks.
- [Release workflow 40299088884](https://github.com/rennerdo30/kwil/actions/runs/40299088884)
  built all six targets and created the draft with eight raw executables and
  `SHA256SUMS`. It is marked prerelease and excluded from latest.
- All eight checksum entries match the uploaded asset digests. The downloaded
  Windows x64 executable pair was independently hashed against that file.
- Guarded save/quit fixtures and model cleanup migrations are committed and
  included in the green candidate CI. Incomplete metadata work is excluded.
- Packaged native checks are not clear. The Windows x64 GUI driver exercised
  edit/save/undo/redo against exact disk bytes, but normal quit exited with code
  1. Earlier driver failures are preserved; correcting its key-message ordering
  exposed this separate exit failure. Reopen qualification was not reached.
- An actual ConPTY TUI session displayed the editor and live search, but quit
  retained `WrapThreadExitPending` and required owned-process cleanup. These are
  failed native exit gates, despite green hosted CI. The draft must remain
  unpublished pending diagnosis and disposition.
- The draft contains no qualified MSI, ZIP installer, signatures or notarization.
  Beta installation/replacement is manual. Stable installers/updater do not
  select betas; beta copies do not automatically advance to stable.

The release remains unpublished. Only the owner publishes, after reviewing the
concrete draft and final packaged checks. See [release notes](BETA_RELEASE_NOTES.md).

## Setup preparation remains unqualified

A private source-only schema-2 installer candidate is being prepared for a future
beta.2. It retains the full beta identity, maps beta.N to Windows Installer's
numeric 0.0.N, and retains the existing Programs/Kwil family/path and manual
update policy. This changes neither the immutable beta.1 tag/raw assets nor the
current draft's publication state. No new setup build, native acceptance or
release/publish wiring is established by this source work.

Explicit-parent-desktop diagnostic run 40312316490 at 908515e failed on x64 and
ARM64 with 0xC0000142, absent sentinel and complete cleanup. No further diagnostic
run is approved. High-IL restricted-token evidence is not ordinary Medium-IL
standard-user qualification. All five lifecycle gaps remain open; KW-66 native
exit fixes are separate and excluded from this package.

## Historical evaluation snapshot

A Windows x64 portable evaluation snapshot has been prepared from commit `e74f1b890c2b5128dd61569f0999a3dce74eab13` (binary version `0.1.0`). It is not a published beta release or a tagged prerelease. It excludes later committed cooperative-worker changes and uncommitted integration candidates.

### Evidence for the historical snapshot

- [Hosted CI 40196643705](https://github.com/rennerdo30/kwil/actions/runs/40196643705) passed formatting, full workspace tests and Clippy on Windows, macOS and Linux, plus the workflow's configured smoke checks.
- The portable ZIP and all eight entries were checked against the committed-source distribution manifest, including names, sizes and SHA-256 values.
- ZIP SHA-256: `e8b7e439b8e65771f7598a941950672cf1e19b7846ee975713791e544c1bc53f`.
- This is unsigned. Checksums establish consistency, not publisher authentication.
- No current native GUI, full installer lifecycle, large-file memory or performance acceptance is claimed for this snapshot.

Extract the portable ZIP into a new user-writable folder and run `kwil.exe`. Use `kwil.com --tui` for the terminal. Keep both entry points and the marker files together. `kwil.portable` keeps settings beside the executable; `kwil.no-update` disables automatic replacement of this managed snapshot.

## Known incomplete areas

- Comparison session/panes, unchanged-region collapse and final word-refinement qualification.
- Complete any-size opening/indexing and regex/long-line paths, and folder replacement.
- Formatter, log-watcher, custom theme and TUI integration.
- Further save/recovery qualification and complete metadata pre-growth admission.
- Accessibility, remaining platform behavior, native/remote performance and enforcing budget gates.
- Installer lifecycle, signing and release/update trust qualification. The MSI is not offered as a qualified beta installer.

The source contains backend work and other candidates for several of these areas. Their presence is not evidence that the feature is integrated or accepted.

## Before a public beta

1. Freeze the exact candidate commit and publish its tested-feature and known-limitations checklist.
2. Verify actual GUI open/edit/save/undo/reopen and terminal operation against the packaged binaries; retain failure evidence.
3. Keep installer qualification separate from portable packaging. Do not call static MSI inspection an installation pass.
4. Set and validate the prerelease version across package metadata, artifacts and update policy. The beta tag is frozen; do not retag a published candidate.
5. Obtain the owner's publication approval. Agents may prepare the candidate; only the owner publishes.

An RC is appropriate only after the complete v1 feature and acceptance checklist is closed, leaving release-level regression testing rather than major implementation gaps.
