# Install, optionally upgrade from a published setup, then uninstall. This
# writes the disposable hosted user's Installed apps entry and Start menu.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version,
    [string]$PreviousSetup,
    [string]$PreviousVersion,
    [string]$CapturePath
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'Runs only on disposable GitHub-hosted Windows runners; never on the owner PC.'
}
if ([bool]$PreviousSetup -ne [bool]$PreviousVersion) { throw 'Pass both PreviousSetup and PreviousVersion.' }
if ($PreviousVersion -and $PreviousVersion -eq $Version) { throw 'Upgrade must use a different previous version.' }
$env:KWIL_NO_UPDATE = '1'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Kwil'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kwil'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Kwil.lnk'
$data = Join-Path $env:APPDATA 'kwil'
# Do not overwrite a runner profile or installation provided by someone else.
foreach ($path in $dir, $key, $shortcut, $data) {
    if (Test-Path -LiteralPath $path) { throw "Expected a fresh disposable runner: $path exists" }
}

function Invoke-Bounded([string]$File, [string[]]$Arguments, [int]$Seconds = 120, [switch]$RegistrationCleanup) {
    $start = [Diagnostics.ProcessStartInfo]::new($File)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::Start($start)
    Write-Host "Started PID $($process.Id): $File (timeout $Seconds s)"
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit($Seconds * 1000)) {
            $process.Kill()
            throw "$File timed out after $Seconds s"
        }
        $output = $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult()
        # Retain the existing test's one allowed refusal on elevated hosted
        # profiles; every other nonzero result still fails the smoke.
        $refused = $RegistrationCleanup -and $output -match 'Registration storage is not owned by the token user'
        if ($process.ExitCode -and !$refused) { throw "$File exited with $($process.ExitCode): $output" }
        return $output.Trim()
    } finally {
        $process.Dispose()
    }
}
function Assert([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw "FAILED: $Message" }
    "ok: $Message"
}

function Assert-UserData([string]$Stage) {
    foreach ($item in $expected.GetEnumerator()) {
        $path = Join-Path $data $item.Key
        Assert (Test-Path -LiteralPath $path -PathType Leaf) "$Stage retains $($item.Key)"
        Assert ([Convert]::ToBase64String([IO.File]::ReadAllBytes($path)) -ceq $item.Value) "$Stage preserves exact $($item.Key) bytes"
    }
}

function Assert-Version([string]$ExpectedVersion) {
    $entry = Get-ItemProperty -LiteralPath $key
    Assert ($entry.DisplayVersion -ceq $ExpectedVersion) "Installed apps version $ExpectedVersion"
    Assert ($entry.InstallLocation -eq $dir) 'same per-user install location'
    $reported = Invoke-Bounded (Join-Path $dir 'kwil.com') @('--version') 30
    Assert ($reported -ceq "kwil $ExpectedVersion") "kwil.com --version: $reported"
}

if ($CapturePath) {
    & (Join-Path $PSScriptRoot 'capture-nsis.ps1') -Setup $Setup -Output $CapturePath
}
if ($PreviousSetup) {
    Invoke-Bounded $PreviousSetup @('/S') | Write-Host
    Assert-Version $PreviousVersion
} else {
    Invoke-Bounded $Setup @('/S') | Write-Host
    Assert-Version $Version
}

# Valid settings plus a KWS3 session containing one untitled tab, and recovery
# bytes. These live in the actual Windows profile paths, not an isolation
# override that could hide a setup accidentally deleting %APPDATA%\kwil.
$settings = [Text.Encoding]::UTF8.GetBytes("# KW-31 user settings`nkeymap = 'notepad++'`n[files]`nrestore_session = true`n")
$session = [Convert]::FromBase64String('S1dTMwIAAAAAAAAAAQAAAAAAAAABAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAyfvuLQ==')
$recovery = [byte[]](0, 255, 13, 10, 75, 87, 45, 51, 49)
$expected = @{
    'settings.toml' = [Convert]::ToBase64String($settings)
    'workspace/session.kws' = [Convert]::ToBase64String($session)
    'workspace/recovery-preservation.bin' = [Convert]::ToBase64String($recovery)
}
New-Item -ItemType Directory -Path (Join-Path $data 'workspace') | Out-Null
foreach ($item in $expected.GetEnumerator()) {
    [IO.File]::WriteAllBytes((Join-Path $data $item.Key), [Convert]::FromBase64String($item.Value))
}
if ($PreviousSetup) {
    Invoke-Bounded $Setup @('/S') | Write-Host
    Assert-UserData 'upgrade'
}
foreach ($file in 'kwil.exe', 'kwil.com', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'uninstall.exe') {
    Assert (Test-Path (Join-Path $dir $file)) "installed $file"
}
Assert (Test-Path (Join-Path $dir '.renner-install.ini')) 'shared template ownership record'
Assert-Version $Version
Assert (Test-Path $shortcut) 'Start menu shortcut'
$unregister = Invoke-Bounded (Join-Path $dir 'kwil.com') @('--unregister-file-types') 30 -RegistrationCleanup
Write-Host "ok: kwil.com --unregister-file-types: $unregister"

# The uninstaller copies itself to %TEMP% and returns at once; wait for the
# entry to disappear instead.
Invoke-Bounded (Join-Path $dir 'uninstall.exe') @('/S') | Write-Host
$deadline = (Get-Date).AddSeconds(120)
while ((Test-Path $key) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (!(Test-Path $key)) 'Installed apps entry removed'
while ((Test-Path $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (!(Test-Path $dir)) 'install folder removed'
Assert (!(Test-Path $shortcut)) 'Start menu shortcut removed'
Assert-UserData 'uninstall'
# Every check passed; do not leak the last native exit code into the step.
exit 0
