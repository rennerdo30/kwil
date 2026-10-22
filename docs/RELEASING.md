# Releasing Kwil

Every publication needs the owner's yes. Agents prepare releases; only the owner publishes them.

## Steps

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo build` so `Cargo.lock` follows, and commit.
2. Tag and push the tag: `git tag vX.Y.Z && git push origin refs/tags/vX.Y.Z`.
3. The **Release** workflow (`.github/workflows/release.yml`) checks that the tag matches the version, builds `kwil` for:
   - Windows x64 and ARM64;
   - macOS Apple silicon and Intel;
   - Linux x64 and ARM64.

   It then creates a **draft** GitHub Release with the binaries (`kwil-<target>[.exe]`) and `SHA256SUMS`.
4. The owner reviews the draft and publishes it.
5. Installed copies find the new version through auto-update (`crates/update`): they check once a day after start-up, verify the SHA-256 and swap the binary for the next start. `kwil --update` checks at once.

## Asset contract (do not change without updating `crates/update`)

- One raw executable per target, named `kwil-<target triple>` plus `.exe` on Windows.
- `SHA256SUMS` in `sha256sum` format (`<hex>  <file name>`).
- Drafts and prereleases are never offered to auto-update.

## Not yet in place

- Code signing (Windows Authenticode, macOS notarization) and an update signature (for example minisign). Each needs keys from the owner (KW-4, KW-5).
- Installers and packages (MSI/winget, dmg/Homebrew, AppImage/Flatpak): KW-31.
