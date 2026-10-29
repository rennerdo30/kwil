# Windows release packages from one built kwil.exe/kwil.com pair:
#   kwil-<version>-windows-<arch>.zip        portable folder
#   kwil-<version>-windows-<arch>-setup.exe  per-user NSIS installer (installer/kwil.nsi)
# Used by release.yml and install.yml. -InstallNsis lets CI runners install
# NSIS when it is missing; without it a missing makensis is an error.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')][string]$Target,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][string]$Com,
    [Parameter(Mandatory)][string]$Notices,
    [Parameter(Mandatory)][string]$OutDir,
    [switch]$InstallNsis
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$arch = if ($Target -like 'aarch64-*') { 'arm64' } else { 'x64' }
if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z.-]+)?$') { throw "Unexpected version $Version" }
$numeric = "$($Matches[1]).$($Matches[2]).$($Matches[3]).0"
$name = "kwil-$Version-windows-$arch"

New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$stage = Join-Path $OutDir $name
if (Test-Path $stage) { throw "$stage already exists" }
New-Item -ItemType Directory $stage | Out-Null
Copy-Item $Exe (Join-Path $stage 'kwil.exe')
Copy-Item $Com (Join-Path $stage 'kwil.com')
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
Copy-Item $Notices (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')

Compress-Archive -Path $stage -DestinationPath (Join-Path $OutDir "$name.zip")

$makensis = (Get-Command makensis.exe -ErrorAction SilentlyContinue).Source
if (!$makensis) { $makensis = Join-Path ${env:ProgramFiles(x86)} 'NSIS\makensis.exe' }
if (!(Test-Path $makensis) -and $InstallNsis) {
    choco install nsis -y --no-progress
    if ($LASTEXITCODE) { throw "choco install nsis failed ($LASTEXITCODE)" }
}
if (!(Test-Path $makensis)) { throw 'makensis.exe (NSIS 3) not found' }
$setup = Join-Path $OutDir "$name-setup.exe"
& $makensis -V3 "-DVERSION=$Version" "-DVI_VERSION=$numeric" "-DARCH=$arch" "-DSRC=$stage" "-DOUTFILE=$setup" (Join-Path $root 'installer\kwil.nsi')
if ($LASTEXITCODE) { throw "makensis failed ($LASTEXITCODE)" }
Remove-Item -Recurse $stage
Get-ChildItem $OutDir -Filter "$name*" | ForEach-Object { '{0,12:N0}  {1}' -f $_.Length, $_.Name }
