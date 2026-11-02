# renner.dev NSIS template

Vendored from `E:/Development/renner.dev/tools/installers/nsis/renner-dev.nsh`
on 2026-11-03. That source exists locally as uncommitted work; no upstream
commit is available. Its original SHA-256 is
`921a55c72ffe2ff76ee0f401eab2d22da84ea545870dd04fd84c48f3bbeb5ba9`.
The adjacent MIT license is retained. This is the shared template, not a new
installer design.

Two optional macro calls are the only local template changes:

- `RD_VALIDATE_LEGACY`: Kwil's adapter recognizes its shipped NSIS installs,
  which predate the shared ownership record. Keeps the existing `Kwil` registry
  key and installation folder; no previous uninstaller is run during upgrade.
- `RD_BEFORE_UNINSTALL`: retain `kwil.com --unregister-file-types` and updater
  lock/journal cleanup before removing application files.

`../product.nsh` supplies Kwil's name, existing icon, website night/ice palette,
and these compatibility hooks. `../payload.nsh` is the fixed four-file payload
adapter, using the template's stage/backup/commit macros. Settings and recovery
data are outside that inventory and are never removed. Unknown files inside
the installation folder are also retained by the template's nonrecursive
directory removal.

`scripts/build-installer-art.py` regenerates the committed 164×314 sidebar and
150×57 header bitmaps using Pillow and a resvg-based SVG renderer:
`python scripts/build-installer-art.py --renderer <svgrender.exe>` (CLI:
`<input.svg> <output.png> <size>`). It renders `website/assets/kwil-icon.svg`
directly (`assets/logo/kwil-icon.svg` is identical).
The ICO itself remains the setup and uninstall icon. Pillow is an authoring
tool only; packaging does not need it.
