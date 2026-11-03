# Releasing Kwil

Windows release entry points use a static C runtime and are checked for forbidden
runtime DLL imports before upload. See [installation](INSTALL.md) for the
one-line installers and exact platform dependency boundaries. Release-download commands
are not advertised as available until the scripts are on main and the owner has
published a stable release with both Windows entry points and checksums.

Every publication needs the owner's yes. Agents prepare releases; only the owner publishes them.

## Steps

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo build` so `Cargo.lock` follows, run `cargo update -w --offline` in `crates/bench` (its separate lock file pins `kwil-core-text`; a stale one fails the Bench budgets workflow), and commit both lock files.
2. Tag and push the tag: `git tag vX.Y.Z && git push origin refs/tags/vX.Y.Z`.
3. The **Release** workflow (`.github/workflows/release.yml`) checks that the tag matches the version, builds `kwil` for:
   - Windows x64 and ARM64;
   - macOS Apple silicon and Intel;
   - Linux x64 and ARM64.

   It generates `THIRD-PARTY-NOTICES.txt` with cargo-about (`about.toml`; an unaccepted dependency license fails the release), embeds it in the About page, and packages:
   - Windows: the NSIS setup and a zip per architecture (`scripts/package-windows.ps1`); the x64 setup is installed, run and uninstalled on the runner first;
   - macOS and Linux: a tar.gz per target;
   - the bare binaries for the updater (`kwil-<target>` on Unix; `kwil-<target>.exe` and `kwil-<target>.com` on Windows).

   It generates GitHub build provenance attestations for every staged release asset,
   including `THIRD-PARTY-NOTICES.txt` and `SHA256SUMS`, then creates a **draft**
   GitHub Release with those files. Attestation failure prevents draft creation.
4. Message the Mac coordinator (`Dominics-MacBook-Pro.local/claude-Development`)
   with the draft tag and request signing using the owner's Mac recipe. Signing
   and notarization run **only on the owner's Mac**, never in CI. No Apple keys
   or notary credentials belong in GitHub secrets.
5. The Mac coordinator runs the following from a checkout containing this script
   (authenticated `gh`, Xcode command-line tools and Python 3 are required):

   ```sh
   tag=vX.Y.Z
   bash scripts/macos-sign.sh "$tag" "$HOME/kwil-signed/$tag"
   gh release upload "$tag" "$HOME/kwil-signed/$tag/"* --repo rennerdo30/kwil --clobber
   ```

   The script downloads and checks all four original macOS assets against the
   draft's `SHA256SUMS` before modifying anything. It signs without entitlements,
   verifies the signature, submits one ditto zip per architecture and requires
   both notarizations to be `Accepted`. It rebuilds the existing tar.gz layout
   with the signed executable at mode 755, retaining the licence and notices.
   It writes only the four signed assets and prints their SHA-256 hashes; it
   never uploads. Use a fresh output directory for each attempt. Defaults are
   `KWIL_MACOS_SIGN_IDENTITY='Developer ID Application: Dominic Renner (73L5ZHST4Q)'`
   and `KWIL_MACOS_NOTARY_PROFILE=renner-notary`; these environment variables can
   override the Mac's identity/profile, but CI still requires team `73L5ZHST4Q`.
   A failed submission prints the `xcrun notarytool log` command for diagnosis.
6. After uploading all four replacements, dispatch **Verify signed macOS release**
   (`.github/workflows/release-macos-signed.yml`) from `main` with input `tag`:

   ```sh
   gh workflow run release-macos-signed.yml --ref main --repo rennerdo30/kwil -f tag="$tag"
   ```

   Wait for success on this tag. It checks the downloaded replacements, rebuilds
   `SHA256SUMS` over the same asset set and in the same format as `release.yml`,
   attests the four signed assets and new checksums, and replaces `SHA256SUMS`.
   It refuses published releases. Keep the draft unchanged throughout signing
   and verification; if any asset changes afterwards, run verification again.
7. The owner reviews the draft and successful verification run, then publishes
   it (or explicitly authorizes the coordinator to publish it).
8. Copies without `kwil.no-update` find the new version through auto-update (`crates/update`): they check once a day after start-up, verify the SHA-256 and swap the binary for the next start. `kwil --update` checks at once.

### What macOS verification proves

The dispatch workflow checks **each bare executable and each tar.gz executable**:

- `codesign --verify --strict` checks signature integrity. Signature metadata
  must name team `73L5ZHST4Q`, a `Developer ID Application` authority and the
  hardened runtime flag. `codesign -d --entitlements -` must contain no
  entitlements, including `com.apple.security.get-task-allow`.
- `codesign --verify --strict -R=notarized --check-notarization` checks the
  notarization ticket online. This is [Apple DTS's assessment for other code](https://developer.apple.com/forums/thread/130560),
  including a bare executable. `spctl -t install` assesses installer packages;
  `syspolicy_check distribution` assesses app bundles. Neither matches our assets.
- `lipo -archs` checks the exact target architecture. `--version` must print
  `kwil X.Y.Z` matching the tag; Intel execution uses the macOS runner's Rosetta.
  The archived executable must have mode 755 and match the bare asset byte for byte.

These command-line checks do not replace testing a quarantined download on a
fresh Mac. Bare binaries **cannot be stapled**: Gatekeeper needs an online
first-launch check. Stapled `.app`/`.dmg` distribution remains follow-up KW-31.

Source inspection: `release.yml` uses plain `cargo build --release`; the Cargo
configuration and build scripts supply no macOS entitlement file, signing step
or `get-task-allow` linker option. The signing script supplies no entitlements
and checks the resulting signature; the dispatch checks the actual shipped bytes.

Before any prerelease build, `docs/BETA_RELEASE_NOTES.md` must start with
`# Kwil X.Y.Z`, using the exact tagged version without its leading `v`
(including any prerelease suffix). A stale or missing heading stops the workflow
before notices or builds run. The exact tagged commit must first pass the `CI`
workflow. Release builds refuse to start without that successful run. A prerelease
version such as
`0.1.0-beta.1` uses the matching tag `v0.1.0-beta.1`; the workflow creates a
**draft prerelease**, sets `latest=false` and uses `docs/BETA_RELEASE_NOTES.md`.
Review the notes for the specific prerelease before tagging. Only the owner
publishes the resulting draft.

Prereleases do not enter the stable updater or the download scripts. Builds after beta.1 can opt into prerelease
updates and can advance from beta to stable; see [update settings](UPDATES.md).
Frozen beta.1 copies still require manual replacement. See [beta gates and limitations](BETA.md).

## Verify a downloaded asset

For releases built with this workflow change, use an authenticated
[GitHub CLI](https://cli.github.com/manual/gh_attestation_verify) with
`gh attestation verify` support. Download the asset from the intended release,
keep its original bytes, and run (replace `X.Y.Z` with that release's version):

```sh
gh attestation verify ./kwil-x86_64-pc-windows-msvc.exe --repo rennerdo30/kwil --signer-workflow rennerdo30/kwil/.github/workflows/release.yml --source-ref refs/tags/vX.Y.Z
```

Replace the path with any unchanged release asset, including an installer, archive,
the Windows `.com` or `THIRD-PARTY-NOTICES.txt`. Verify an archive
before extracting it. A successful exit verifies that the downloaded bytes match
an attestation from this repository's release workflow for the selected tag.
Stop if verification fails; older releases may have no attestation.

For signed macOS replacements and the regenerated `SHA256SUMS`, the original
`release.yml` attestations cover the **unsigned bytes** (and old checksums).
They do not cover the replacements. The dispatch workflow creates fresh build
provenance attestations for the signed bytes it verified, not a rebuild or proof
that the Mac's signing operation ran in CI. Verify these using the dispatch
workflow and its source ref (normally `refs/heads/main`), for example:

```sh
gh attestation verify ./kwil-aarch64-apple-darwin --repo rennerdo30/kwil --signer-workflow rennerdo30/kwil/.github/workflows/release-macos-signed.yml --source-ref refs/heads/main
```

The dispatch run checks the requested tag's version; its provenance source ref
names the workflow invocation, not the release tag. Check that run's `tag` input
when auditing a particular release.

Attestations are stored by GitHub and need no owner signing key. They establish
GitHub workflow provenance; Apple signing/notarization is the separate Mac step
above. Windows Authenticode and the updater's independent signature verification
remain separate work. The built-in
updater currently checks `SHA256SUMS`, without running this manual command.

## Asset contract (do not change without updating `crates/update`)

- Unix: one raw executable per target, named `kwil-<target triple>`.
- Windows: two regular PE executables from the shared application library: `kwil-<target>.exe` (Windows subsystem) and `kwil-<target>.com` (console subsystem).
- Install the Windows pair together as `kwil.exe` and `kwil.com` in the same folder. Explorer, shortcuts and Open with use `.exe`; shells resolve `.com` first with the standard `.COM;.EXE` PATHEXT order. Custom PATHEXT order is respected; use `kwil.com` explicitly if necessary.
- The console entry point runs terminal work itself. For a GUI it forwards arguments unchanged to `kwil.exe`, waits for its first-frame acknowledgement, then returns. Before acknowledging readiness, the GUI detaches from the shell console and releases standard handles. Startup diagnostics use launcher-owned pipes, so redirected shell streams reach EOF when the launcher exits. Startup failure is handled while it still owns the terminal; CLI and TUI paths keep their console.
- Keep the existing `.exe` asset name so older single-file updater clients can still find it.
- The setup, zip and tar.gz assets are for people; the updater never downloads them. The NSIS setup writes the per-user Installed apps entry `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil`; the updater updates its `DisplayVersion` when `InstallLocation` is the updated folder.
- `SHA256SUMS` in `sha256sum` format (`<hex>  <file name>`).
- Drafts are never offered. Stable-channel updates exclude prereleases;
  `update.channel = "prerelease"` explicitly opts into published prereleases.

## Not yet in place

- Windows Authenticode and an update signature (for example minisign). Each needs keys from the owner (KW-4, KW-5).
- Packages beyond the Windows setup and archives: winget, dmg/Homebrew and AppImage/Flatpak (KW-31).

## Windows development build

Build the pair with `cargo build --locked -j 4 -p kwil --features windows-console`, then copy `kwil-console.exe` to `kwil.com` beside `kwil.exe`. Non-Windows builds and releases use only `kwil` without this feature. The release workflow performs the Windows rename during staging. Both entry points accept `--help` and `--version` without creating a window.

## Windows paired updates (KW-13)

Owner-approved 2026-10-23: Windows updates are **transactional with recovery, not crash-atomic**. True crash atomicity would require permanent launchers plus version folders, a possible later packaging change.

The updater requires both assets in new releases. It verifies their `SHA256SUMS` entries, writes synced staging copies beside the installed pair, then takes the exclusive `.kwil-update.lock`. It makes synced, checksum-verified backups and publishes `.kwil-update-journal` before changing either installed file. The journal records the staging folder and old/new hashes. Only a verified new pair receives a commit marker. A replacement failure restores both old files; an older exe-only install also restores the original absence of `.com` on rollback. When adding `.com` for the first time, it is installed before retiring `.exe`, so one entry point remains available through each rename gap.

Both entry points check the same lock before argument handling or frontend startup. Ordinary startups take a shared check; an active exclusive update makes startup fail with a clear retry message. Startup with an interrupted journal takes the exclusive lock, keeps a fully verified committed pair or restores the verified backups, and asks for a restart before running an editor. This avoids continuing an image that Windows may have loaded during an interrupted replacement. If recovery cannot verify/restore the pair, startup fails closed and retains the journal/backups for repair. Start the surviving entry point if interruption left the other executable path temporarily absent. Rollback restores a missing executable path before retiring the surviving path, so an interrupted recovery also retains an entry point. A new install without any update metadata can start from a read-only folder without writing a lock file.

The `.exe` asset name stays unchanged for older single-file updater clients. Those clients can still download and replace their `.exe`; once running the new version, a later update installs both files. New updater clients never silently downgrade to a single-file Windows update if `.com` or its checksum is missing. Non-Windows targets retain the existing single-file replacement.

A running Windows image can keep its retired staging file locked after a successful update. Cleanup removes the other staging files and leaves the locked file/folder; it does not spawn a deletion helper or discard recovery evidence for an unfinished transaction. Neither updates nor recovery touch edited documents or unsaved buffers.
