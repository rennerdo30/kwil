# Beta candidate

The next proposed public milestone is an early beta, not an RC. V1 remains the full scope in [SPEC.md](SPEC.md); this checkpoint does not reduce it.

A Windows x64 portable evaluation snapshot has been prepared from commit `e74f1b890c2b5128dd61569f0999a3dce74eab13` (binary version `0.1.0`). It is not a published beta release or a tagged prerelease. It excludes later committed cooperative-worker changes and uncommitted integration candidates.

## Evidence for this snapshot

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
- Guarded save/retirement integration and complete metadata pre-growth admission.
- Accessibility, remaining platform behavior, native/remote performance and enforcing budget gates.
- Installer lifecycle, signing and release/update trust qualification. The MSI is not offered as a qualified beta installer.

The source contains backend work and other candidates for several of these areas. Their presence is not evidence that the feature is integrated or accepted.

## Before a public beta

1. Freeze the exact candidate commit and publish its tested-feature and known-limitations checklist.
2. Verify actual GUI open/edit/save/undo/reopen and terminal operation against the packaged binaries; retain failure evidence.
3. Keep installer qualification separate from portable packaging. Do not call static MSI inspection an installation pass.
4. Set and validate the prerelease version across package metadata, artifacts and update policy. No beta tag has been created yet.
5. Obtain the owner's publication approval. Agents may prepare the candidate; only the owner publishes.

An RC is appropriate only after the complete v1 feature and acceptance checklist is closed, leaving release-level regression testing rather than major implementation gaps.