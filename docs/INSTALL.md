# Install Kwil

## Downloads

Every GitHub Release (https://github.com/rennerdo30/kwil/releases) offers:

| Platform | For people | Bare binaries (used by the updater) |
| --- | --- | --- |
| Windows x64 / ARM64 | `kwil-<version>-windows-<arch>-setup.exe`, `kwil-<version>-windows-<arch>.zip` | `kwil-<target>.exe`, `kwil-<target>.com` |
| macOS Apple silicon / Intel | `kwil-<version>-macos-<arch>.tar.gz` | `kwil-<target>` |
| Linux x64 / ARM64 | `kwil-<version>-linux-<arch>.tar.gz` | `kwil-<target>` |

Each archive and the installer contain `LICENSE.txt` and
`THIRD-PARTY-NOTICES.txt` (the licenses of all bundled Rust crates and syntax
grammars; the same text is on the About page). `SHA256SUMS` covers every file.
Builds are not signed yet: Windows SmartScreen and macOS Gatekeeper warn on
first start. Checksums detect damaged downloads; they do not authenticate the
publisher.

## Windows setup

Run `kwil-<version>-windows-<arch>-setup.exe` as your normal user; it needs no
administrator rights. It installs `kwil.exe` and `kwil.com` to
`%LOCALAPPDATA%\Programs\Kwil`, adds a Start menu shortcut and an Installed
apps entry. It does not change PATH or default apps; the first-run page offers
file-type registration ("Open with").

Kwil updates itself in place (see [Updates](UPDATES.md)) and keeps the version
in Installed apps current. Close Kwil before installing over an existing copy;
the setup asks if it is still running.

Uninstall through Settings > Apps > Installed apps. The uninstaller first
removes Kwil's "Open with" entries (`kwil --unregister-file-types`, the same
as "Remove all" on the file-types page), then the installed files, shortcut and
entry. Settings and recovery data in `%APPDATA%\kwil` stay. Silent use:
`setup.exe /S`, `uninstall.exe /S`; `/D=<folder>` (last argument) picks
another folder.

The installer script is `installer/kwil.nsi`; `scripts/package-windows.ps1`
builds the setup and zip, and CI installs, runs and uninstalls the x64 setup
(`scripts/test-nsis-installer.ps1`) for every release.

## Portable (zip, tar.gz)

Extract into a writable folder. On Windows keep `kwil.exe` and `kwil.com`
together; run `kwil.exe` for the GUI or `kwil.com --tui` in a terminal. On
macOS and Linux run `kwil`. Portable copies update themselves too; put a
`kwil.no-update` file next to the executable to turn that off.

## Published-release download scripts

The download commands below become usable after these scripts reach `main` and the owner
publishes a stable GitHub Release containing the platform binaries and
`SHA256SUMS`. Draft releases are deliberately unavailable to the installer.

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/rennerdo30/kwil/main/scripts/install.ps1 | iex
```

Linux/macOS (requires curl and Python 3.8+):

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/rennerdo30/kwil/main/scripts/install.sh | sh
```

For review before execution, download the script instead of piping it into a
shell. Both scripts accept `vX.Y.Z` as their first argument to pin a version.
They download only published stable releases, select the native CPU architecture,
and check exact SHA-256 entries before publishing any executable. Checksums from
the same HTTPS release detect damaged downloads; they are not an independent
publisher signature.

Windows installs the complete `kwil.exe`/`kwil.com` pair under
`%LOCALAPPDATA%/Programs/Kwil/vX.Y.Z`. It adds that directory to the current
PowerShell process's PATH only and prints the executable path for later use.
No persistent environment, shell profile, administrator settings or file
associations are changed. Run the printed path or manually add its directory to
your PATH if desired. The first-run UI separately offers file-type registration.

Linux/macOS install under `~/.local/share/kwil/vX.Y.Z` with a
`~/.local/bin/kwil` symlink. Add `~/.local/bin` manually if it is not already on
your PATH. Existing launchers/version directories are refused, never replaced.
These scripts perform fresh installations, not in-place updates. Windows can
retain multiple version directories; Unix existing launchers require explicit
user-managed removal or the existing installation's update mechanism.

To uninstall a script installation, remove its version directory and the Unix
symlink, if present. Keep your settings/recovery data. Unregister any file types
through Settings before removing the executable if you enabled them.

## Linking

For stable/prerelease self-update settings and managed-install exclusions, see
[Updates](UPDATES.md). Frozen beta.1 copies still require manual replacement.

Windows release builds request Rust's `+crt-static` for both MSVC targets. The
release gate inspects both normal and delay-load PE imports and refuses dynamic
Visual C++/Universal CRT imports. Windows system DLLs remain normal OS
dependencies. This avoids a separate Visual C++ redistributable installation;
it does not embed Windows itself.

Linux GNU builds retain their platform/display-library dependencies. macOS
builds retain system-framework dependencies. Neither platform is described as
fully static. A musl/Linux distribution requires separate GUI/display-driver
qualification before changing the current GNU asset contract.
