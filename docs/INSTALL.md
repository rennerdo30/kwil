# Install Kwil

The commands below become usable after these scripts reach `main` and the owner
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
