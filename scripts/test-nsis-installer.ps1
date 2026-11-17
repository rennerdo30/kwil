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
$appPathsKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\kwil.exe'
# Do not overwrite a runner profile or installation provided by someone else.
foreach ($path in $dir, $key, $shortcut, $data, $appPathsKey) {
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

# KW-90: command-line registration on the disposable runner's own profile. The
# user PATH is read raw (unexpanded) so the registry text and type are compared.
function Get-UserPath {
    $environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        $value = $environment.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $kind = if ($null -eq $value) { $null } else { $environment.GetValueKind('Path') }
        return [pscustomobject]@{ Text = $value; Kind = $kind }
    } finally { $environment.Dispose() }
}
function Assert-PathRestored($Before, [string]$Stage) {
    $now = Get-UserPath
    if ($null -eq $Before.Text) {
        Assert ([string]::IsNullOrEmpty($now.Text)) "$Stage leaves the user PATH empty or absent"
    } else {
        Assert ($now.Text -ceq $Before.Text) "$Stage restores the user PATH byte for byte"
        Assert ($now.Kind -eq $Before.Kind) "$Stage keeps the PATH value type $($Before.Kind)"
    }
    Assert (!(Test-Path -LiteralPath $appPathsKey)) "$Stage removes the App Paths entry"
}
function Assert-CommandLineRegistered($Before) {
    $now = Get-UserPath
    Assert (($now.Text -split ';' | Where-Object { $_.TrimEnd([char]92) -ieq $dir }).Count -eq 1) 'install folder is on the user PATH exactly once'
    if ($null -ne $Before.Text) {
        Assert ($now.Text.StartsWith($Before.Text)) 'every earlier PATH entry is untouched and in order'
        Assert ($now.Kind -eq $Before.Kind) "the PATH value type stays $($Before.Kind)"
    } else {
        Assert ($now.Kind -eq 'ExpandString') 'a new PATH value is REG_EXPAND_SZ'
    }
    $appPaths = Get-Item -LiteralPath $appPathsKey
    Assert ($appPaths.GetValue('') -ieq (Join-Path $dir 'kwil.exe')) 'App Paths default is kwil.exe (Win+R)'
    Assert ($appPaths.GetValue('Path') -ieq $dir) 'App Paths Path is the install folder'
}
$pathBefore = Get-UserPath
$registered = Invoke-Bounded (Join-Path $dir 'kwil.com') @('--register-command-line') 30
Assert ($registered -clike 'Kwil is on your PATH.*') "kwil.com --register-command-line: $registered"
Assert-CommandLineRegistered $pathBefore
$pathAfter = Get-UserPath
$again = Invoke-Bounded (Join-Path $dir 'kwil.com') @('--register-command-line') 30
Assert ($again -ceq 'Kwil is already on your PATH.') "second register changes nothing: $again"
Assert ((Get-UserPath).Text -ceq $pathAfter.Text) 'second register leaves the PATH text unchanged'
$unregister = Invoke-Bounded (Join-Path $dir 'kwil.com') @('--unregister-file-types') 30 -RegistrationCleanup
Write-Host "ok: kwil.com --unregister-file-types: $unregister"
Assert-PathRestored $pathBefore 'kwil.com --unregister-file-types'
# The uninstaller must do the same without being asked separately.
Invoke-Bounded (Join-Path $dir 'kwil.com') @('--register-command-line') 30 | Write-Host
Assert-CommandLineRegistered $pathBefore

# The uninstaller copies itself to %TEMP% and returns at once; wait for the
# entry to disappear instead.
Invoke-Bounded (Join-Path $dir 'uninstall.exe') @('/S') | Write-Host
$deadline = (Get-Date).AddSeconds(120)
while ((Test-Path $key) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (!(Test-Path $key)) 'Installed apps entry removed'
while ((Test-Path $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (!(Test-Path $dir)) 'install folder removed'
Assert (!(Test-Path $shortcut)) 'Start menu shortcut removed'
Assert-PathRestored $pathBefore 'uninstall'
Assert-UserData 'uninstall'
# Every check passed; do not leak the last native exit code into the step.
exit 0
