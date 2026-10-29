# Releasing Kwil

Windows release entry points use a static C runtime and are checked for forbidden
runtime DLL imports before upload. See [installation](INSTALL.md) for the
one-line installers and exact platform dependency boundaries. Release-download commands
are not advertised as available until the scripts are on main and the owner has
published a stable release with both Windows entry points and checksums.

Every publication needs the owner's yes. Agents prepare releases; only the owner publishes them.

## Steps

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo build` so `Cargo.lock` follows, and commit.
2. Tag and push the tag: `git tag vX.Y.Z && git push origin refs/tags/vX.Y.Z`.
3. The **Release** workflow (`.github/workflows/release.yml`) checks that the tag matches the version, builds `kwil` for:
   - Windows x64 and ARM64;
   - macOS Apple silicon and Intel;
   - Linux x64 and ARM64.

   It generates `THIRD-PARTY-NOTICES.txt` with cargo-about (`about.toml`; an unaccepted dependency license fails the release), embeds it in the About page, and packages:
   - Windows: the NSIS setup and a zip per architecture (`scripts/package-windows.ps1`); the x64 setup is installed, run and uninstalled on the runner first;
   - macOS and Linux: a tar.gz per target;
   - the bare binaries for the updater (`kwil-<target>` on Unix; `kwil-<target>.exe` and `kwil-<target>.com` on Windows).

   It then creates a **draft** GitHub Release with all of them, `THIRD-PARTY-NOTICES.txt` and `SHA256SUMS`.
4. The owner reviews the draft and publishes it.
5. Copies without `kwil.no-update` find the new version through auto-update (`crates/update`): they check once a day after start-up, verify the SHA-256 and swap the binary for the next start. `kwil --update` checks at once.

The exact tagged commit must first pass the `CI` workflow. Release builds refuse
to start without that successful run. A prerelease version such as
`0.1.0-beta.1` uses the matching tag `v0.1.0-beta.1`; the workflow creates a
**draft prerelease**, sets `latest=false` and uses `docs/BETA_RELEASE_NOTES.md`.
Review the notes for the specific prerelease before tagging. Only the owner
publishes the resulting draft.

Prereleases do not enter the stable updater or the download scripts. Builds after beta.1 can opt into prerelease
updates and can advance from beta to stable; see [update settings](UPDATES.md).
Frozen beta.1 copies still require manual replacement. See [beta gates and limitations](BETA.md).

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

- Code signing (Windows Authenticode, macOS notarization) and an update signature (for example minisign). Each needs keys from the owner (KW-4, KW-5).
- Packages beyond the Windows setup and archives: winget, dmg/Homebrew and AppImage/Flatpak (KW-31).

## Windows development build

Build the pair with `cargo build --locked -j 4 -p kwil --features windows-console`, then copy `kwil-console.exe` to `kwil.com` beside `kwil.exe`. Non-Windows builds and releases use only `kwil` without this feature. The release workflow performs the Windows rename during staging. Both entry points accept `--help` and `--version` without creating a window.

## Windows paired updates (KW-13)

Owner-approved 2026-10-23: Windows updates are **transactional with recovery, not crash-atomic**. True crash atomicity would require permanent launchers plus version folders, a possible later packaging change.

The updater requires both assets in new releases. It verifies their `SHA256SUMS` entries, writes synced staging copies beside the installed pair, then takes the exclusive `.kwil-update.lock`. It makes synced, checksum-verified backups and publishes `.kwil-update-journal` before changing either installed file. The journal records the staging folder and old/new hashes. Only a verified new pair receives a commit marker. A replacement failure restores both old files; an older exe-only install also restores the original absence of `.com` on rollback. When adding `.com` for the first time, it is installed before retiring `.exe`, so one entry point remains available through each rename gap.

Both entry points check the same lock before argument handling or frontend startup. Ordinary startups take a shared check; an active exclusive update makes startup fail with a clear retry message. Startup with an interrupted journal takes the exclusive lock, keeps a fully verified committed pair or restores the verified backups, and asks for a restart before running an editor. This avoids continuing an image that Windows may have loaded during an interrupted replacement. If recovery cannot verify/restore the pair, startup fails closed and retains the journal/backups for repair. Start the surviving entry point if interruption left the other executable path temporarily absent. Rollback restores a missing executable path before retiring the surviving path, so an interrupted recovery also retains an entry point. A new install without any update metadata can start from a read-only folder without writing a lock file.

The `.exe` asset name stays unchanged for older single-file updater clients. Those clients can still download and replace their `.exe`; once running the new version, a later update installs both files. New updater clients never silently downgrade to a single-file Windows update if `.com` or its checksum is missing. Non-Windows targets retain the existing single-file replacement.

A running Windows image can keep its retired staging file locked after a successful update. Cleanup removes the other staging files and leaves the locked file/folder; it does not spawn a deletion helper or discard recovery evidence for an unfinished transaction. Neither updates nor recovery touch edited documents or unsaved buffers.
