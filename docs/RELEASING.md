# Releasing Kwil

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
5. Installed copies find the new version through auto-update (`crates/update`): they check once a day after start-up, verify the SHA-256 and swap the binary for the next start. `kwil --update` checks at once.

## Asset contract (do not change without updating `crates/update`)

- Unix: one raw executable per target, named `kwil-<target triple>`.
- Windows: two regular PE executables from the shared application library: `kwil-<target>.exe` (Windows subsystem) and `kwil-<target>.com` (console subsystem).
- Install the Windows pair together as `kwil.exe` and `kwil.com` in the same folder. Explorer, shortcuts and Open with use `.exe`; shells resolve `.com` first with the standard `.COM;.EXE` PATHEXT order. Custom PATHEXT order is respected; use `kwil.com` explicitly if necessary.
- The console entry point runs terminal work itself. For a GUI it forwards arguments unchanged to `kwil.exe`, waits for its first-frame acknowledgement, then returns. Startup failure is handled while it still owns the terminal.
- Keep the existing `.exe` asset name so older single-file updater clients can still find it.
- `SHA256SUMS` in `sha256sum` format (`<hex>  <file name>`).
- Drafts and prereleases are never offered to auto-update.

## Not yet in place

- Code signing (Windows Authenticode, macOS notarization) and an update signature (for example minisign). Each needs keys from the owner (KW-4, KW-5).
- Installers and packages (MSI/winget, dmg/Homebrew, AppImage/Flatpak): KW-31.

## Windows development build

Build the pair with `cargo build --locked -j 4 -p kwil --features windows-console`, then copy `kwil-console.exe` to `kwil.com` beside `kwil.exe`. Non-Windows builds and releases use only `kwil` without this feature. The release workflow performs the Windows rename during staging. Both entry points accept `--help` and `--version` without creating a window.

## Pending paired update gate (KW-13)

Do not release the Windows pair until the updater transaction is implemented and reviewed. The current updater still replaces only its own executable. A proposed staged, locked transaction with rollback is awaiting the coordinator/owner decision; it must verify both checksums before touching either installed file, recover interrupted transactions or fail closed, and retain compatibility with the existing `.exe` asset and `SHA256SUMS` format. Two separate executable paths cannot be replaced by a single atomic filesystem rename.
