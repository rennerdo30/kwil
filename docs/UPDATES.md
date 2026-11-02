# Updates

Source after beta.1 supports both update channels. The immutable beta.1 binaries
do not contain this change; replace those manually to obtain a later build.

Kwil checks after its first frame on a background worker, at most once a day per
channel. `kwil --update` checks immediately. The existing settings file controls
both automatic checks and that command:

```toml
[update]
check = true
channel = "stable"
```

Beta and other prerelease builds default to `prerelease`; stable builds default
to `stable`. An explicit `update.channel` always overrides the build's default,
in both the settings UI and `kwil --update`. Set `channel = "stable"` as above to
receive only published stable releases. A beta build using this channel can
advance to a newer stable release. The `prerelease` channel receives published
betas as well as stable releases and chooses the
highest newer semantic version, regardless of publication order: beta.10 follows
beta.9, and 0.1.0 follows every 0.1.0 prerelease. Neither channel installs drafts,
the same version, or a downgrade. Invalid versions or inconsistent prerelease
labels are excluded. Build metadata does not make a version newer.

The updater reads the settings on its next check. Set `check = false` to disable
updates, including `--update`. Invalid or unreadable settings stop the check.
`KWIL_NO_UPDATE`, a `kwil.no-update` file beside the executable, development
version 0.0.0, and debug builds also disable updates. Package managers that
ship the marker upgrade Kwil themselves. The Windows setup and the zip/tar.gz
archives do not ship it: they update in place, and an installed Windows copy
also updates the version shown in Installed apps.
The existing stable download/install scripts do not install prereleases.

Release discovery is bounded to ten pages of 100 releases and 8 MiB of metadata
per page under one 60-second discovery deadline. An incomplete or failed scan
does not install a candidate. Stable discovery uses GitHub's latest-release
endpoint; a repository with no published stable release is up to date.

Downloads are verified against `SHA256SUMS`. Windows replaces the matching
`kwil.exe`/`kwil.com` pair transactionally, with verified backups and interrupted
update recovery. Other platforms replace their executable. The running editor
keeps running; restart to use the new version. Updating does not edit documents
or recovery data.

Signature verification is not implemented. Checksums detect mismatched bytes
but do not authenticate the publisher independently of GitHub. Signing and a
published-release end-to-end update remain release qualification gaps. Unit
tests and mocked release discovery do not establish native updater acceptance.

## Manually verify release provenance

Releases built with the provenance-enabled release workflow attest every asset,
including the raw updater binaries, packages, notices and `SHA256SUMS`. Before
installing a manually downloaded asset, use an authenticated GitHub CLI:

```sh
gh attestation verify ./kwil-x86_64-pc-windows-msvc.exe --repo rennerdo30/kwil --signer-workflow rennerdo30/kwil/.github/workflows/release.yml --source-ref refs/tags/vX.Y.Z
```

Substitute your downloaded filename and exact release version, including its
beta suffix if present. Verify packages before extraction and each Windows
entry point if downloading the pair directly. Require a successful exit before
using the asset; older releases may not have attestations.

This verifies the bytes against this repository's release workflow and tag.
See [release verification](RELEASING.md#verify-a-downloaded-asset) and the
[GitHub CLI verification reference](https://cli.github.com/manual/gh_attestation_verify).
The built-in updater does not perform this attestation check. This manual check
does not add Windows code signing, Apple notarization or independent update
signatures.
