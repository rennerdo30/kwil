# Install, check and uninstall the NSIS setup silently. Disposable CI runners
# only: this writes the current user's Installed apps entry and Start menu.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if (!$env:CI) { throw 'Runs only on CI runners (it installs Kwil for the current user).' }
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Kwil'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Kwil.lnk'

function Invoke-Bounded([string]$File, [string[]]$Arguments, [int]$Seconds = 120) {
    $process = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru -NoNewWindow
    if (!$process.WaitForExit($Seconds * 1000)) {
        $process.Kill()
        throw "$File timed out after $Seconds s"
    }
    if ($process.ExitCode) { throw "$File exited with $($process.ExitCode)" }
}
function Assert([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw "FAILED: $Message" }
    "ok: $Message"
}

Invoke-Bounded $Setup @('/S')
foreach ($file in 'kwil.exe', 'kwil.com', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'uninstall.exe') {
    Assert (Test-Path (Join-Path $dir $file)) "installed $file"
}
$entry = Get-ItemProperty $key
Assert ($entry.DisplayVersion -eq $Version) "Installed apps version $($entry.DisplayVersion)"
Assert ($entry.InstallLocation -eq $dir) "install location $($entry.InstallLocation)"
Assert (Test-Path $shortcut) 'Start menu shortcut'
$reported = & (Join-Path $dir 'kwil.com') --version
Assert ($reported -eq "kwil $Version") "kwil.com --version: $reported"
# Hosted runners run as an elevated admin whose profile files belong to
# Administrators, so Kwil's registration storage check refuses there (as the
# file-types page would). Any other outcome than success or that refusal fails.
$unregister = & (Join-Path $dir 'kwil.com') --unregister-file-types 2>&1 | Out-String
$refused = $unregister -match 'Registration storage is not owned by the token user'
Assert ($LASTEXITCODE -eq 0 -or $refused) "kwil.com --unregister-file-types: $($unregister.Trim())"

# The uninstaller copies itself to %TEMP% and returns at once; wait for the
# entry to disappear instead.
Invoke-Bounded (Join-Path $dir 'uninstall.exe') @('/S')
$deadline = (Get-Date).AddSeconds(120)
while ((Test-Path $key) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (!(Test-Path $key)) 'Installed apps entry removed'
while ((Test-Path $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (!(Test-Path $dir)) 'install folder removed'
Assert (!(Test-Path $shortcut)) 'Start menu shortcut removed'
# Every check passed; do not leak the last native exit code into the step.
exit 0
