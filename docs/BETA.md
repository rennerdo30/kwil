# Beta readiness

V1 remains the full scope in [SPEC.md](SPEC.md). Betas are for evaluation and
feedback; see the [release notes](BETA_RELEASE_NOTES.md) for what each one
changes and its known limitations.

## Published

- **0.1.0-beta.4**, published 2026-10-29 from main `8ea5eab`: every palette
  command runs; beta.3 updated itself to it.
- **0.1.0-beta.3**, published 2026-10-29 from main `62a814e`: Windows setup and
  zip, macOS/Linux tar.gz, license notices; beta.2 updated itself to it.
- **0.1.0-beta.2**, published as a GitHub prerelease on 2026-10-29 from main
  `6dff1ba`: bare executables for six targets plus `SHA256SUMS`. A build
  versioned 0.1.0-beta.1 updated itself to it on Windows x64, and the replaced
  files match the published checksums.
- **0.1.0-beta.1** was never published; its tag stays as history.

## Next candidate: 0.1.0-beta.5

Overwrite-or-reload choice when a save finds the file changed on disk; tab
context menu.

## In beta.3

Adds the per-user NSIS setup and zip for Windows, tar.gz archives for macOS and
Linux, third-party license notices in every package and on the About page,
uninstall cleanup of "Open with" entries, and the Installed apps version update
by the updater. The bare binaries stay, so beta.2 copies update to it.

The release workflow installs, runs and uninstalls the x64 setup on a hosted
runner before the draft is created. The ARM64 setup is built the same way but
not run (no ARM64 NSIS runner check).

## Known incomplete areas

- Comparison session/panes, unchanged-region collapse and final word-refinement qualification.
- Complete any-size opening/indexing and regex/long-line paths, and folder replacement.
- Formatter, log-watcher, custom theme and TUI integration.
- Further save/recovery qualification and complete metadata pre-growth admission.
- Accessibility, remaining platform behavior, native/remote performance and enforcing budget gates.
- Code signing, macOS notarization and update signatures (keys from the owner, KW-4, KW-5).

## Before each beta

1. Merge to main with green CI on the exact commit.
2. Bump the version, tag `v0.1.0-beta.N`; the release workflow builds the draft.
3. Check the draft's Windows binaries (version, update, GUI opens a file).
4. The owner publishes. Agents prepare the candidate; only the owner publishes.

An RC is appropriate only after the complete v1 feature and acceptance checklist is closed, leaving release-level regression testing rather than major implementation gaps.
