# Install Kwil

## Local Windows distribution

From the repository root, run:

```powershell
.\dist.bat
# Optional: retain work/logs and set bounded build/package timeouts (seconds).
.\dist.bat -KeepWorkDirectory -BuildTimeoutSeconds 1800 -PackageTimeoutSeconds 120
```

Prerequisites must already be installed: Windows x64 or ARM64, PowerShell 7
(`pwsh.exe`), Git, rustup, the pinned Rust toolchain for the native MSVC target,
Windows SDK/linker tools and `makecab.exe`. Locked Cargo dependencies must be
cached, and `E:/Development/_cache/cargo-target` must exist. The build is offline;
it does not download or install missing tools or dependencies.

The driver archives one exact **committed HEAD**. Application, installer backend,
guard, assets and lockfile integration must already be committed; dirty and
untracked inputs are ignored. Only the invoked driver comes from the working
tree. Missing committed integration is a refusal, not a fallback to dirty code.

On success, `dist/kwil-<version>-<12-hex-commit>-<target>/` contains an unsigned
MSI, `*-portable.zip`, the MSI's adjacent `<full-msi-filename>.manifest.json`
sidecar, `INSTALL.txt`, `manifest.json` and `SHA256SUMS`. Checksums cover the MSI,
sidecar, ZIP, instructions and outer manifest; they are not publisher signatures.
Existing output directories are refused, never overwritten. **Building does not
install or launch Kwil, sign artifacts or publish a release.**

To try the MSI, open the generated `.msi` as your normal, non-elevated user.
The installer contract specifies `%LOCALAPPDATA%/Programs/Kwil` (without a
version subdirectory), an Apps & Features entry, a current-user Start Menu
shortcut and an Explorer **Open with Kwil** static context-menu verb for files.
On Windows 11, look under **Show more options**. This verb opens `kwil.exe`;
it does not choose Kwil as a default application. The MSI does not change PATH.
Uninstall through Windows Settings > Apps (Apps & Features / Installed apps).
Settings, recovery data, edited documents and unrelated files/registry values
are retained.

Keep the exact original MSI and its sidecar. Close Kwil before maintenance.
Explicit repair of missing owned resources uses the retained original package
(replace the placeholder with its actual path):

```powershell
msiexec /i "<retained-original.msi>" REINSTALL=ALL REINSTALLMODE=amus
```

There is no advertised/automatic repair. A present changed installer-authored
file, shortcut or registry value blocks repair, upgrade and uninstall; it is
not overwritten or deleted. Preserve modifications and restore the original
resource before retrying. A repacked MSI with a different PackageCode cannot
substitute for the retained original. Upgrades require a strictly higher numeric
version; equal-version different builds, downgrades and architecture switching
in place are refused.

For portable use, extract the ZIP into a new writable folder and keep `kwil.exe`
and `kwil.com` together. Run `kwil.exe` for the GUI or `kwil.com --tui` for explicit
terminal use. `kwil.portable` keeps settings beside the executable; the MSI
omits it and uses the normal user settings location. Both payloads contain
`kwil.no-update`, disabling self-update: update these copies explicitly by
rebuilding/reinstalling or replacing the portable distribution. The ZIP adds no
Start Menu or Explorer registration.

Installer/guard integration and native lifecycle qualification remain pending.
These instructions describe the source contract, not a passed install, repair,
upgrade or uninstall run, nor v1 completion. See [decision 0004](decisions/0004-windows-installer.md)
and its normative [machine-readable contract](../scripts/windows-installer-contract.json).
Local artifacts are not a release and confer no publication authorization.

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

Windows release builds request Rust's `+crt-static` for both MSVC targets. The
release gate inspects both normal and delay-load PE imports and refuses dynamic
Visual C++/Universal CRT imports. Windows system DLLs remain normal OS
dependencies. This avoids a separate Visual C++ redistributable installation;
it does not embed Windows itself.

Linux GNU builds retain their platform/display-library dependencies. macOS
builds retain system-framework dependencies. Neither platform is described as
fully static. A musl/Linux distribution requires separate GUI/display-driver
qualification before changing the current GNU asset contract.

No published release, hosted static-artifact inspection or native installation
is established merely by adding these scripts. Hosted release and installer
checks must pass before distribution is advertised as available.
