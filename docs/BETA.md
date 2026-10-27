# Beta readiness

## Next candidate: 0.1.0-beta.2

Beta.2 source includes native shutdown waiting, GUI notification lifetime,
update channels, guarded fixture/IPC repairs and final closed-journal durability
acknowledgement. There is no beta.2 tag or draft yet. V1 remains the full scope
in [SPEC.md](SPEC.md).

- Commit `5007980da3b961a891a6ba1965a9c21388f25854` passed
  [hosted CI 40458026047](https://github.com/rennerdo30/kwil/actions/runs/40458026047)
  on Windows, macOS and Linux: full workspace tests, strict lint and formatting.
- Native smoke exposed a further Quit hang despite that green CI. Commit
  `77d09c1e33247393f58d9094791462edec8da277` publishes the exact successful
  journal Finish durability boundary before closing it. Its real save/close
  regression fails on the old source at its unchanged ten-second deadline and
  passes after the fix. All seven save/quit controls, the journal-boundary
  control and strict model lint pass. Successor hosted CI is still required.
- A static-CRT Windows x64 release pair built from that exact source passes
  native CPU GUI edit/save/undo/redo, clean Quit (exit 0), then reopen/edit/undo
  and clean Quit (exit 0). Disk bytes are checked exactly. Both runs use the
  original thirty-second deadline and cloaked owned windows.
- The same release pair passes hidden ConPTY TUI opening/search (four matches)
  and natural Quit (exit 0), without termination or deadline expiry. This is
  Windows x64 functional evidence; other packaged platforms, visual quality
  and performance remain separate gates.
- Release pair SHA-256: GUI
  `f3a0e197bcbc641871f32d8328af62adb1bbc6e39886051d5c28678e1d79d3ee`;
  console
  `22b4e6dc8d8dd92086e31677f06c209474f46eccac5282ddc5a34c12c5e02bba`.
- Installer lifecycle qualification and release setup wiring remain open below.
  These local binaries are not qualified setup assets or published downloads.

## Frozen beta.1 draft

**0.1.0-beta.1** remains an unpublished prerelease draft, not an RC.

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

Schema-2 installer identity is committed (26da691). It retains the full beta
identity and maps beta.N to MSI 0.0.N, with the existing family/path and manual
update policy. Hosted run 40341998956 generated MSI/portable ZIP artifacts for
x64 and ARM64 but failed worker admission before installation. Those generated
packages are not qualified setup assets or release attachments.

The ordinary-user environment proof is committed (4192666) with 176 passing
inert controls. It has not been provisioned or dispatched and remains subject
to the owner's concrete approval documented in
[the proof runbook](ORDINARY_USER_PROOF.md). All five native lifecycle gaps
remain unwaived. Release workflow setup wiring remains pending qualification.

The older restricted-token explicit-desktop diagnostic 40312316490 failed on
both architectures with 0xC0000142. It does not establish ordinary-user coverage,
and no unchanged diagnostic retry is proposed. Later native-exit/GUI fixes are
committed, but new packaged acceptance must use the exact next candidate.
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
