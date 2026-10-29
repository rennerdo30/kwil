# 0005: Plain NSIS setup instead of the MSI protocol

Status: accepted by the owner, 2026-10-29. Replaces 0004 (schema-2 MSI).

The MSI path (own MSI/CAB builder, frozen schema-2 contract, native guard DLL,
ordinary-user lifecycle proof) never completed an install on a hosted runner and
blocked the betas. beta.2 shipped bare executables instead.

From beta.3 on, Windows gets a conventional per-user NSIS setup
(`installer/kwil.nsi`) and a portable zip, macOS and Linux get tar.gz archives.
The bare binaries stay, because the updater (`crates/update`) downloads them.

- Per user, no admin rights: `%LOCALAPPDATA%\Programs\Kwil`, Start menu
  shortcut, `HKCU` Installed apps entry. No PATH or default-app changes.
- Updates stay with Kwil's own updater; it keeps the entry's `DisplayVersion`
  current. The setup has no update logic of its own.
- Uninstall runs `kwil --unregister-file-types` ("Remove all") before removing
  the installed files; settings stay.
- Every package carries `LICENSE.txt` and `THIRD-PARTY-NOTICES.txt` from
  cargo-about (`about.toml`).
- Verification: the release workflow installs, runs and uninstalls the x64 setup
  on a hosted runner (`scripts/test-nsis-installer.ps1`).

Signing (Authenticode) is a separate owner decision (KW-4).
