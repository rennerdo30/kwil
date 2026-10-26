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

   It then creates a **draft** GitHub Release with the binaries (`kwil-<target>` on Unix; `kwil-<target>.exe` and `kwil-<target>.com` on Windows) and `SHA256SUMS`.
4. The owner reviews the draft and publishes it.
5. Copies without `kwil.no-update` find the new version through auto-update (`crates/update`): they check once a day after start-up, verify the SHA-256 and swap the binary for the next start. `kwil --update` checks at once. Local MSI/portable distributions contain that marker and require explicit updates instead.

The exact tagged commit must first pass the `CI` workflow. Release builds refuse
to start without that successful run. A prerelease version such as
`0.1.0-beta.1` uses the matching tag `v0.1.0-beta.1`; the workflow creates a
**draft prerelease**, sets `latest=false` and uses `docs/BETA_RELEASE_NOTES.md`.
Review the notes for the specific prerelease before tagging. Only the owner
publishes the resulting draft.

Beta assets are the raw executables described below, installed and replaced
manually. The combined local MSI/portable ZIP driver requires a numeric MSI
version and is not the beta packaging path. Prereleases do not enter the stable
updater or download installers; prerelease copies also do not auto-advance to a
stable version. See [beta gates and limitations](BETA.md).

## Asset contract (do not change without updating `crates/update`)

- Unix: one raw executable per target, named `kwil-<target triple>`.
- Windows: two regular PE executables from the shared application library: `kwil-<target>.exe` (Windows subsystem) and `kwil-<target>.com` (console subsystem).
- Install the Windows pair together as `kwil.exe` and `kwil.com` in the same folder. Explorer, shortcuts and Open with use `.exe`; shells resolve `.com` first with the standard `.COM;.EXE` PATHEXT order. Custom PATHEXT order is respected; use `kwil.com` explicitly if necessary.
- The console entry point runs terminal work itself. For a GUI it forwards arguments unchanged to `kwil.exe`, waits for its first-frame acknowledgement, then returns. Before acknowledging readiness, the GUI detaches from the shell console and releases standard handles. Startup diagnostics use launcher-owned pipes, so redirected shell streams reach EOF when the launcher exits. Startup failure is handled while it still owns the terminal; CLI and TUI paths keep their console.
- Keep the existing `.exe` asset name so older single-file updater clients can still find it.
- `SHA256SUMS` in `sha256sum` format (`<hex>  <file name>`).
- Drafts and prereleases are never offered to auto-update.

## Not yet in place

- Code signing (Windows Authenticode, macOS notarization) and an update signature (for example minisign). Each needs keys from the owner (KW-4, KW-5).
- Qualified installers and packages: the local Windows MSI/portable source path below still needs integration and lifecycle acceptance; winget, dmg/Homebrew and AppImage/Flatpak remain pending (KW-31).

## Local Windows distribution (not a release)

`dist.bat` invokes `scripts/dist.ps1` to prepare a native x64 or ARM64 MSI and
portable ZIP offline from committed HEAD. See [local installation](INSTALL.md#local-windows-distribution)
for prerequisites, the existing CLI options, installation/Explorer integration
and the exact retained-original repair command. All intended backend, isolated
guard and Cargo/lockfile integration must be committed first; dirty/untracked
inputs are excluded, apart from the invoked driver itself.

Successful output is an immutable
`dist/kwil-<version>-<12-hex-commit>-<target>/` directory containing the MSI,
portable ZIP, `<full-msi-filename>.manifest.json` sidecar, `INSTALL.txt`, outer
`manifest.json` and `SHA256SUMS`. Retain the original MSI and sidecar together.
The sidecar records package identity/inventory and inspection evidence;
`nativeLifecycle: not-run` is not native acceptance. Checksums cover the MSI,
sidecar, ZIP, instructions and outer manifest, not themselves. Neither manifest
nor checksums authenticate the publisher.

This build does not install, run the application, sign or publish. It does not
change the raw GitHub Release asset/updater contract above. The MSI and ZIP both
contain `kwil.no-update`; only the ZIP contains `kwil.portable`. Their updates
are explicit rather than the paired self-update flow below.

Decision [0004](decisions/0004-windows-installer.md) and
[`scripts/windows-installer-contract.json`](../scripts/windows-installer-contract.json)
are jointly normative. Integration and disposable hosted Windows x64/ARM64
lifecycle qualification remain pending: install, original-package repair,
higher-version upgrade, rollback, uninstall, resource/in-use refusals and data
preservation. MSI/CAB/PE inspection alone proves none of those lifecycle results.
Local artifact preparation does not establish v1 completion or authorize a
release/publication; owner approval remains required.

## Windows development build

Build the pair with `cargo build --locked -j 4 -p kwil --features windows-console`, then copy `kwil-console.exe` to `kwil.com` beside `kwil.exe`. Non-Windows builds and releases use only `kwil` without this feature. The release workflow performs the Windows rename during staging. Both entry points accept `--help` and `--version` without creating a window.

## Windows paired updates (KW-13)

Owner-approved 2026-10-23: Windows updates are **transactional with recovery, not crash-atomic**. True crash atomicity would require permanent launchers plus version folders, a possible later packaging change.

The updater requires both assets in new releases. It verifies their `SHA256SUMS` entries, writes synced staging copies beside the installed pair, then takes the exclusive `.kwil-update.lock`. It makes synced, checksum-verified backups and publishes `.kwil-update-journal` before changing either installed file. The journal records the staging folder and old/new hashes. Only a verified new pair receives a commit marker. A replacement failure restores both old files; an older exe-only install also restores the original absence of `.com` on rollback. When adding `.com` for the first time, it is installed before retiring `.exe`, so one entry point remains available through each rename gap.

Both entry points check the same lock before argument handling or frontend startup. Ordinary startups take a shared check; an active exclusive update makes startup fail with a clear retry message. Startup with an interrupted journal takes the exclusive lock, keeps a fully verified committed pair or restores the verified backups, and asks for a restart before running an editor. This avoids continuing an image that Windows may have loaded during an interrupted replacement. If recovery cannot verify/restore the pair, startup fails closed and retains the journal/backups for repair. Start the surviving entry point if interruption left the other executable path temporarily absent. Rollback restores a missing executable path before retiring the surviving path, so an interrupted recovery also retains an entry point. A new install without any update metadata can start from a read-only folder without writing a lock file.

The `.exe` asset name stays unchanged for older single-file updater clients. Those clients can still download and replace their `.exe`; once running the new version, a later update installs both files. New updater clients never silently downgrade to a single-file Windows update if `.com` or its checksum is missing. Non-Windows targets retain the existing single-file replacement.

A running Windows image can keep its retired staging file locked after a successful update. Cleanup removes the other staging files and leaves the locked file/folder; it does not spawn a deletion helper or discard recovery evidence for an unfinished transaction. Neither updates nor recovery touch edited documents or unsaved buffers.
