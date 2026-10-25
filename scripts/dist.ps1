#requires -Version 7.0
<#
.SYNOPSIS
Build committed HEAD offline and prepare a local Windows MSI and portable ZIP.
.DESCRIPTION
Run from any directory: .\dist.bat [-BuildTimeoutSeconds 1800] [-KeepWorkDirectory]
Requires installed Git, PowerShell 7, rustup, the pinned Rust toolchain/native MSVC
target, cached locked dependencies, Windows SDK/linker prerequisites and makecab.
Nothing is downloaded or installed. The MSI backend must be committed at
scripts/windows-msi.ps1, with this interface:
  -PayloadDirectory -OutputPath -Version -Architecture -Commit -IconPath
Architecture is x64 or arm64. OutputPath is an absolute new .msi file path.
The backend must create a per-user MSI with an Explorer verb; no installation,
registry mutation, default association, PATH/profile changes or application launch
is permitted during packaging. It inherits this process's offline/static build
environment and is bounded by a separate process timeout.

Only the driver being invoked is read from the working tree. ALL application,
asset, licence and backend inputs come from a Git archive of one exact HEAD.
Dirty/untracked code is ignored. Commit the intended changes before invoking.
No reset, stash, checkout, fetch, Git worktree or persistent environment writes.

Output policy: dist/kwil-<CargoVersion>-<12-hex-commit>-<target>/ is immutable.
Any existing destination or reservation is refused, including known artifacts.
No -Force/overwrite option. Use a new source commit, or explicitly move your old
output yourself after inspection. Publication is one same-volume directory rename.
Failed work/staging directories are kept with ownership markers and process logs.
Successful work is removed only after containment/link/owner checks, unless kept.
The shared kwil-dist build cache is never cleaned by this script.

This prepares unsigned local artifacts. It never installs, signs or publishes.
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 7200)][int]$BuildTimeoutSeconds = 1800,
    [ValidateRange(1, 120)][int]$PackageTimeoutSeconds = 120,
    [switch]$KeepWorkDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-DistPlainPath {
    param([Parameter(Mandatory)][string]$Path)
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing a reparse point in owned path: $cursor"
            }
        }
        $next = [IO.Path]::GetDirectoryName($cursor)
        if ($next -eq $cursor) { break }
        $cursor = $next
    }
}

function Assert-DistContainedPath {
    param([string]$Parent, [string]$Path)
    Assert-DistPlainPath $Parent
    Assert-DistPlainPath $Path
    $root = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Parent).ProviderPath).TrimEnd('\', '/')
    $child = [IO.Path]::GetFullPath($Path)
    if (-not $child.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Path is not strictly below owned parent '$root': $child"
    }
    return $child
}

function New-DistOwnedDirectory {
    param([string]$Parent, [string]$Name, [string]$Token)
    if ($Name -notmatch '^[a-zA-Z0-9._-]+$' -or $Name -in '.', '..') { throw 'Unsafe directory name.' }
    $path = Assert-DistContainedPath $Parent (Join-Path $Parent $Name)
    if (Test-Path -LiteralPath $path) { throw "Refusing existing directory: $path" }
    $null = New-Item -ItemType Directory -Path $path -ErrorAction Stop
    [IO.File]::WriteAllText((Join-Path $path '.kwil-dist-owner'), $Token)
    return $path
}

function Remove-DistOwnedDirectory {
    param([string]$Parent, [string]$Path, [string]$Token)
    $checked = Assert-DistContainedPath $Parent $Path
    $marker = Join-Path $checked '.kwil-dist-owner'
    Assert-DistPlainPath $marker
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf) -or
        [IO.File]::ReadAllText($marker) -cne $Token) { throw "Ownership mismatch: $checked" }
    # Inspect every descendant without following junctions BEFORE any recursive removal.
    foreach ($entry in Get-ChildItem -LiteralPath $checked -Force -Recurse) {
        if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Cleanup refused; linked descendant retained: $($entry.FullName)"
        }
    }
    Remove-Item -LiteralPath $checked -Recurse -ErrorAction Stop
}

function New-DistReservation {
    param([string]$Path, [string]$Token)
    Assert-DistPlainPath $Path
    # CreateNew is the atomic ownership claim; never open/truncate an existing lock.
    $stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($Token)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } catch { $stream.Dispose(); throw }
    return $stream
}

function Close-DistReservation {
    param([IO.FileStream]$Stream, [string]$Path, [string]$Token)
    if ($null -eq $Stream) { return }
    $Stream.Dispose()
    Assert-DistPlainPath $Path
    if ([IO.File]::ReadAllText($Path) -cne $Token) { throw "Reservation ownership mismatch: $Path" }
    [IO.File]::Delete($Path)
}

function New-DistProcessInfo {
    param([string]$FilePath, [string[]]$Arguments, [string]$WorkingDirectory)
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $FilePath
    $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    # No shell or hand-built command string: spaces, quotes and metacharacters stay arguments.
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    return $info
}

function Stop-DistOwnedProcess {
    param($Process, [int]$RecordedPid, [string]$LogDirectory, [string]$Label)
    # An abnormal boundary cannot prove unrecorded descendants are gone, even
    # after this exact parent exits. Keep reservations for ROOT's inspection.
    $script:DistUnreapedProcess = $true
    if ($RecordedPid -le 0 -or $Process.Id -ne $RecordedPid) { throw 'Owned process identity mismatch; no termination attempted.' }
    [IO.File]::AppendAllText((Join-Path $LogDirectory 'processes.log'), "label=$Label pid=$RecordedPid cleanup=exact-only unresolvedOwnership=true`n")
    if (-not $Process.HasExited) {
        $Process.Kill()
        if (-not $Process.WaitForExit(5000)) { throw "Owned PID $RecordedPid has not exited; retain work and reservations." }
    }
}

function Close-DistReservations {
    param($CacheStream, [string]$CachePath, $OutputStream, [string]$OutputPath, [string]$Token)
    if ($script:DistUnreapedProcess) {
        if ($null -ne $CacheStream) { $CacheStream.Dispose() }
        if ($null -ne $OutputStream) { $OutputStream.Dispose() }
        Write-Warning 'Unresolved process ownership: reservation files retained. ROOT must inspect PID receipts before recovery.'
    } else {
        try { Close-DistReservation $CacheStream $CachePath $Token }
        finally { Close-DistReservation $OutputStream $OutputPath $Token }
    }
}

function Invoke-DistProcess {
    param([string]$FilePath, [string[]]$Arguments, [string]$WorkingDirectory,
        [int]$TimeoutSeconds, [string]$LogDirectory, [string]$Label)
    $null = Assert-DistContainedPath $LogDirectory (Join-Path $LogDirectory "$Label.stdout.log")
    $stdoutPath = Join-Path $LogDirectory "$Label.stdout.log"
    $stderrPath = Join-Path $LogDirectory "$Label.stderr.log"
    $stdout = [IO.File]::Open($stdoutPath, [IO.FileMode]::CreateNew)
    $stderr = $null
    $process = [Diagnostics.Process]::new()
    $started = $false
    $outputComplete = $false
    $boundaryComplete = $false
    $ownedPid = 0
    try {
        $stderr = [IO.File]::Open($stderrPath, [IO.FileMode]::CreateNew)
        $process.StartInfo = New-DistProcessInfo $FilePath $Arguments $WorkingDirectory
        if (-not $process.Start()) { throw "Could not start $Label" }
        $started = $true
        $ownedPid = $process.Id
        $record = "$(Get-Date -Format o) label=$Label pid=$($process.Id) timeoutSeconds=$TimeoutSeconds"
        Write-Host $record
        [IO.File]::AppendAllText((Join-Path $LogDirectory 'processes.log'), $record + "`n")
        $outTask = $process.StandardOutput.BaseStream.CopyToAsync($stdout)
        $errTask = $process.StandardError.BaseStream.CopyToAsync($stderr)
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            throw "$Label timed out after $TimeoutSeconds seconds (owned PID $($process.Id))."
        }
        # A child retaining a pipe must not turn a finished parent into an infinite wait.
        if (-not [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($outTask, $errTask), 5000)) {
            $script:DistUnreapedProcess = $true
            throw "$Label output streams did not close within 5 seconds."
        }
        $outputComplete = $true
        $code = $process.ExitCode
        [IO.File]::AppendAllText((Join-Path $LogDirectory 'processes.log'), "label=$Label pid=$($process.Id) exitCode=$code`n")
        if ($code -ne 0) { throw "$Label exited $code. Logs: $stdoutPath ; $stderrPath" }
        $boundaryComplete = $true
    } finally {
        if ($started -and -not $boundaryComplete) {
            $script:DistUnreapedProcess = $true
            try {
                Stop-DistOwnedProcess $process $ownedPid $LogDirectory $Label
            } catch {
                $script:DistUnreapedProcess = $true
                Write-Warning "Could not confirm cleanup of owned PID $($process.Id): $_"
            }
        }
        if ($started -and -not $outputComplete) {
            [IO.File]::AppendAllText((Join-Path $LogDirectory 'processes.log'), "label=$Label pid=$($process.Id) incomplete=true`n")
        }
        $process.Dispose()
        $stdout.Dispose()
        if ($null -ne $stderr) { $stderr.Dispose() }
    }
    return [IO.FileInfo]::new($stdoutPath)
}

function Get-DistNames {
    param([string]$Version, [string]$Commit, [string]$Target)
    Assert-DistInstallerVersion $Version
    if ($Commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Expected an exact lowercase 40-hex source commit.' }
    if ($Target -notin 'x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc') { throw 'Unsupported native MSVC target.' }
    $stem = "kwil-$Version-$($Commit.Substring(0, 12))-$Target"
    return [pscustomobject]@{ Namespace = $stem; Msi = "$stem.msi"; Zip = "$stem-portable.zip" }
}

function Export-DistSourceArchive {
    param([string]$Git, [string]$Repository, [string]$Commit, [string]$Archive,
        [string]$WorkingDirectory, [string]$Logs)
    # Snapshot bytes must not depend on the runner's checkout defaults. Disable
    # implicit LF/CRLF conversion for this command only; committed attributes
    # still apply. In particular, preserve mixed endings rather than rewriting
    # extracted files or relaxing the frozen-source hashes after extraction.
    $null = Invoke-DistProcess $Git @('-C', $Repository, '-c', 'core.autocrlf=false',
        '-c', 'core.eol=lf', 'archive', '--format=zip', "--output=$Archive", $Commit) `
        $WorkingDirectory 60 $Logs 'archive'
}

function Get-DistRustupRunArguments {
    param([string]$Toolchain, [string[]]$Command)
    # Installed CLI: rustup run [OPTIONS] <TOOLCHAIN> <COMMAND>...
    # Installation requires explicit --install. Omit it: a missing pinned
    # toolchain fails; do not bootstrap, update or retry with installation.
    return @('run', $Toolchain) + $Command
}

function Get-DistBuildArguments {
    param([string]$Toolchain, [string]$Target)
    return Get-DistRustupRunArguments $Toolchain @('cargo', 'build', '--release', '-p', 'kwil',
        '--features', 'windows-console', '--target', $Target, '--locked', '--offline', '-j', '4')
}

function Use-DistBuildEnvironment {
    param([string]$Commit, [string]$CacheDirectory, [scriptblock]$Action)
    if ($Commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid build commit.' }
    $values = @{
        KWIL_BUILD_COMMIT = $Commit
        CARGO_TARGET_DIR = $CacheDirectory
        CARGO_NET_OFFLINE = 'true'
        CARGO_INCREMENTAL = '0'
        CARGO_TERM_COLOR = 'never'
        CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'
        CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'
        RUSTFLAGS = $null
        CARGO_ENCODED_RUSTFLAGS = $null
        CARGO_BUILD_RUSTFLAGS = $null
        CARGO_BUILD_TARGET = $null
        RUSTC_WRAPPER = $null
        RUSTC_WORKSPACE_WRAPPER = $null
        RUSTUP_AUTO_INSTALL = '0'
    }
    $previous = @{}
    # Capture every original entry BEFORE changing any. A missing entry and a
    # present empty entry are distinct process states; never normalize either.
    foreach ($key in $values.Keys) {
        $original = [Environment]::GetEnvironmentVariable($key, 'Process')
        $exists = -not [object]::ReferenceEquals($original, $null)
        $previous[$key] = [pscustomobject]@{ Exists = $exists; Value = $original }
        # Older runtimes remove an entry when given an empty string. Refuse
        # before any mutation if that would make exact restoration impossible.
        if ($exists -and $original.Length -eq 0 -and [Environment]::Version.Major -lt 9) {
            throw "Cannot preserve present empty process entry $key on this runtime; no build environment was changed."
        }
    }
    try {
        foreach ($key in $values.Keys) {
            if ([object]::ReferenceEquals($values[$key], $null)) {
                # Ordinary PowerShell $null binds to a string parameter as "".
                # NullString passes a real null: delete, rather than create an
                # empty entry on runtimes which support empty environment values.
                [Environment]::SetEnvironmentVariable($key, [System.Management.Automation.Language.NullString]::Value, 'Process')
            } else {
                [Environment]::SetEnvironmentVariable($key, $values[$key], 'Process')
            }
        }
        & $Action
    } finally {
        $restoreErrors = [Collections.Generic.List[string]]::new()
        foreach ($key in $previous.Keys) {
            try {
                if ($previous[$key].Exists) {
                    [Environment]::SetEnvironmentVariable($key, $previous[$key].Value, 'Process')
                } else {
                    [Environment]::SetEnvironmentVariable($key, [System.Management.Automation.Language.NullString]::Value, 'Process')
                }
            } catch { $restoreErrors.Add($key) }
        }
        # Attempt every restoration even if one setter fails; never log values.
        if ($restoreErrors.Count -ne 0) { throw "Process environment restoration failed for: $($restoreErrors -join ', ')" }
    }
}

function Build-DistSnapshot {
    param([string]$Rustup, [string]$Toolchain, [string]$Target, [string]$Snapshot,
        [string]$Logs, [int]$TimeoutSeconds)
    $null = Invoke-DistProcess $Rustup (Get-DistBuildArguments $Toolchain $Target) $Snapshot $TimeoutSeconds $Logs 'cargo-build'
}

function Initialize-DistCache {
    param([string]$Cache)
    Assert-DistPlainPath $Cache
    $marker = Join-Path $Cache '.kwil-dist-cache'
    Assert-DistPlainPath $marker
    $identity = 'kwil-dist cargo cache v1; managed build intermediates only'
    if (Test-Path -LiteralPath $Cache) {
        if (-not (Test-Path -LiteralPath $marker -PathType Leaf) -or
            [IO.File]::ReadAllText($marker) -cne $identity) {
            throw "Refusing unowned existing Cargo cache: $Cache"
        }
    } else {
        $null = New-Item -ItemType Directory -Path $Cache
        [IO.File]::WriteAllText($marker, $identity)
    }
}

function Get-DistPeNumber {
    param($Data, [long]$Offset, [ValidateSet(1, 2, 4, 8)][int]$Size)
    $length = if ($Data -is [IO.FileStream]) { $Data.Length } else { $Data.LongLength }
    if ($Offset -lt 0 -or $Offset -gt $length - $Size) { throw 'Truncated PE structure.' }
    if ($Data -is [IO.FileStream]) {
        $null = $Data.Seek($Offset, [IO.SeekOrigin]::Begin)
        $window = [byte[]]::new($Size)
        $read = 0
        while ($read -lt $Size) {
            $count = $Data.Read($window, $read, $Size - $read)
            if ($count -eq 0) { throw 'Truncated PE read.' }
            $read += $count
        }
        $Data = $window; $Offset = 0
    }
    switch ($Size) {
        1 { return $Data[$Offset] }
        2 { return [BitConverter]::ToUInt16($Data, [int]$Offset) }
        4 { return [BitConverter]::ToUInt32($Data, [int]$Offset) }
        8 { return [BitConverter]::ToUInt64($Data, [int]$Offset) }
    }
}

function Get-DistPeInfo {
    param([string]$Path, [long]$MaximumBytes = 536870912)
    # Pure PowerShell port of scripts/check-static-runtime.py: both import tables,
    # backed-section RVA bounds, ASCII names, termination and the same forbidden prefixes.
    Assert-DistPlainPath $Path
    if ((Get-Item -LiteralPath $Path).Length -gt $MaximumBytes) { throw 'PE size limit exceeded.' }
    # Fixed numeric/header windows only; neither app nor guard is buffered whole.
    $data = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
    if ($data.Length -lt 64 -or $data.Length -gt $MaximumBytes -or (Get-DistPeNumber $data 0 2) -ne 0x5a4d) { throw 'Not a PE image or PE size limit exceeded.' }
    $pe = [long](Get-DistPeNumber $data 0x3c 4)
    if ((Get-DistPeNumber $data $pe 4) -ne 0x4550) { throw 'Invalid PE signature.' }
    $machine = Get-DistPeNumber $data ($pe + 4) 2
    if ($machine -notin 0x8664, 0xaa64) { throw 'Expected x64 or ARM64 PE.' }
    $sections = Get-DistPeNumber $data ($pe + 6) 2
    $optionalSize = Get-DistPeNumber $data ($pe + 20) 2
    $optional = $pe + 24
    if ($sections -lt 1 -or $sections -gt 96 -or $optionalSize -lt 240 -or
        (Get-DistPeNumber $data $optional 2) -ne 0x20b) { throw 'Expected PE32+ optional header and sections.' }
    $null = Get-DistPeNumber $data ($optional + $optionalSize - 2) 2
    $imageBase = Get-DistPeNumber $data ($optional + 24) 8
    $subsystem = Get-DistPeNumber $data ($optional + 68) 2
    if ((Get-DistPeNumber $data ($optional + 108) 4) -lt 14) { throw 'Missing import directory declarations.' }
    $table = $optional + $optionalSize
    $map = {
        param([long]$Rva, [long]$Size)
        for ($i = 0; $i -lt $sections; $i++) {
            $record = $table + $i * 40 + 8
            $virtualSize = [long](Get-DistPeNumber $data $record 4)
            $address = [long](Get-DistPeNumber $data ($record + 4) 4)
            $rawSize = [long](Get-DistPeNumber $data ($record + 8) 4)
            $raw = [long](Get-DistPeNumber $data ($record + 12) 4)
            if ($address -le $Rva -and $Rva + $Size -le $address + [Math]::Min($virtualSize, $rawSize)) {
                $offset = $raw + $Rva - $address
                if ($offset -ge 0 -and $offset + $Size -le $data.Length) { return $offset }
            }
        }
        throw 'PE import outside backed section.'
    }
    $names = [Collections.Generic.List[string]]::new()
    $imports = [Collections.Generic.List[object]]::new()
    $importKeys = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($directory in 1, 13) {
        $recordSize = if ($directory -eq 1) { 20 } else { 32 }
        $rva = [long](Get-DistPeNumber $data ($optional + 112 + $directory * 8) 4)
        $size = [long](Get-DistPeNumber $data ($optional + 116 + $directory * 8) 4)
        if ($rva -eq 0 -and $size -eq 0) { continue }
        if ($rva -eq 0 -or $size -lt $recordSize -or $size -gt $data.Length) { throw 'Invalid import directory.' }
        $terminated = $false
        for ($position = 0L; $position -le $size - $recordSize; $position += $recordSize) {
            $offset = & $map ($rva + $position) $recordSize
            $fields = @(for ($field = 0; $field -lt $recordSize / 4; $field++) {
                Get-DistPeNumber $data ($offset + $field * 4) 4
            })
            if (@($fields | Where-Object { $_ -ne 0 }).Count -eq 0) { $terminated = $true; break }
            $nameRva = [long]$(if ($directory -eq 1) { $fields[3] } else { $fields[1] })
            if ($directory -eq 13 -and ($fields[0] -band 1) -eq 0) { $nameRva -= $imageBase }
            $dll = [Text.StringBuilder]::new()
            $nameTerminated = $false
            for ($length = 1; $length -le 129; $length++) {
                $start = & $map $nameRva $length
                $value = Get-DistPeNumber $data ($start + $length - 1) 1
                if ($value -eq 0) { $nameTerminated = $true; break }
                if ($value -gt 127) { throw 'Non-ASCII import name.' }
                $null = $dll.Append([char]$value)
            }
            if (-not $nameTerminated) { throw 'Unterminated DLL name.' }
            $name = $dll.ToString().ToLowerInvariant()
            if ($name -cnotmatch '^[a-z0-9._-]+[.]dll$' -or $name.Length -gt 128) { throw 'Invalid import DLL basename.' }
            if ($name -match '^(vcruntime|msvcp|msvcr|ucrtbase|api-ms-win-crt-)') { throw "Dynamic runtime import: $name" }
            $load = if ($directory -eq 1) { 'N' } else { 'D' }
            if (-not $importKeys.Add("${load}:$name")) { throw 'Duplicate PE import pair.' }
            if ($imports.Count -ge 128) { throw 'PE import row limit exceeded.' }
            $imports.Add([ordered]@{ load = $load; dll = $name })
            $names.Add($name)
        }
        if (-not $terminated) { throw 'Unterminated import directory.' }
    }
    if ($names.Count -eq 0) { throw 'Expected system imports.' }
    $keys = [string[]]@($importKeys)
    [Array]::Sort($keys, [StringComparer]::Ordinal)
    $orderedImports = @(foreach ($key in $keys) { $imports | Where-Object { "$($_.load):$($_.dll)" -ceq $key } })
    return [pscustomobject]@{ Machine = $machine; Subsystem = $subsystem; IsDll = ((Get-DistPeNumber $data ($pe + 22) 2) -band 0x2000) -ne 0;
        Imports = @($names.ToArray()); GuardImports = $orderedImports; Sha256 = Get-DistFileSha256 $Path $MaximumBytes }
    } finally { $data.Dispose() }
}

function Get-DistFileSha256 {
    param([string]$Path, [long]$MaximumBytes = [long]::MaxValue)
    Assert-DistPlainPath $Path
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $hash = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    try {
        if ($stream.Length -gt $MaximumBytes) { throw 'File hash byte limit exceeded.' }
        $buffer = [byte[]]::new(65536)
        $total = 0L
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -ne 0) {
            if ($count -gt $MaximumBytes - $total) { throw 'File hash byte limit exceeded.' }
            $total += $count
            $hash.AppendData($buffer, 0, $count)
        }
        return [Convert]::ToHexString($hash.GetHashAndReset()).ToLowerInvariant()
    } finally { $hash.Dispose(); $stream.Dispose() }
}

function Assert-DistInstallerVersion {
    param([string]$Version)
    if ($Version -cnotmatch '^(0|[1-9][0-9]{0,2})[.](0|[1-9][0-9]{0,2})[.](0|[1-9][0-9]{0,4})$') { throw 'Invalid Cargo version for MSI: canonical major.minor.build required.' }
    $parts = $Version.Split('.')
    if ([int]$parts[0] -gt 255 -or [int]$parts[1] -gt 255 -or [int]$parts[2] -gt 65535) { throw 'Cargo version exceeds MSI numeric ranges.' }
}

function Assert-DistInstallerSources {
    param([string]$Snapshot)
    foreach ($relative in 'scripts/windows-msi.ps1', 'scripts/windows-installer-contract.json',
        'docs/decisions/0004-windows-installer.md', 'crates/installer-guard/Cargo.toml', 'crates/installer-guard/src/lib.rs') {
        $path = Join-Path $Snapshot $relative
        Assert-DistPlainPath $path
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required committed installer source missing: $relative . ROOT must integrate it before any application compilation." }
    }
    # R2: rollback must remain enabled, normal omus grants no repair authority,
    # and directory rows are exact. Backend/guard enforce session/database rules;
    # this wrapper pins their jointly reviewed normative pair before compilation.
    $frozen = @{
        'scripts/windows-installer-contract.json' = '5b2b2d1e21ee545552f2270ce3c5bd923fec65dcd6e86dd0ef1ac1ed88f54d44'
        'docs/decisions/0004-windows-installer.md' = 'fb92572c9090294869fdc52b8d573de4e5bd8b116b28b6a9291ca213947e483c'
    }
    foreach ($relative in $frozen.Keys) {
        if ((Get-FileHash -LiteralPath (Join-Path $Snapshot $relative) -Algorithm SHA256).Hash.ToLowerInvariant() -cne $frozen[$relative]) {
            throw "Frozen installer contract mismatch: $relative . Ask ROOT; no application compilation is authorized."
        }
    }
}

function Get-DistGuardBuildArguments {
    param([string]$Toolchain, [string]$Target, [string]$Manifest)
    # A manifest-scoped lib build, in its own target cache; no application graph build.
    return Get-DistRustupRunArguments $Toolchain @('cargo', 'build', '--release', '--lib',
        '--manifest-path', $Manifest, '--target', $Target, '--locked', '--offline', '-j', '4',
        '--config', 'profile.release.panic="abort"')
}

function Build-DistGuardSnapshot {
    param([string]$Rustup, [string]$Toolchain, [string]$Target, [string]$Snapshot,
        [string]$Cache, [string]$Logs, [int]$TimeoutSeconds)
    $manifest = Join-Path $Snapshot 'crates/installer-guard/Cargo.toml'
    $metadataLog = Invoke-DistProcess $Rustup (Get-DistRustupRunArguments $Toolchain @('cargo', 'metadata',
        '--manifest-path', $manifest, '--no-deps', '--format-version', '1', '--locked', '--offline')) $Snapshot 60 $Logs 'guard-metadata'
    $metadata = [IO.File]::ReadAllText($metadataLog.FullName) | ConvertFrom-Json
    $packages = @($metadata.packages | Where-Object { [IO.Path]::GetFullPath($_.manifest_path) -eq [IO.Path]::GetFullPath($manifest) })
    if ($packages.Count -ne 1) { throw 'ROOT-integrated guard manifest is absent from Cargo metadata.' }
    $libraries = @($packages[0].targets | Where-Object { 'cdylib' -cin $_.crate_types })
    if ($libraries.Count -ne 1 -or $libraries[0].name -cnotmatch '^[a-zA-Z0-9_]+$') { throw 'Guard must have exactly one cdylib target with a safe DLL basename.' }
    $null = Invoke-DistProcess $Rustup (Get-DistGuardBuildArguments $Toolchain $Target $manifest) $Snapshot $TimeoutSeconds $Logs 'guard-build'
    return Join-Path $Cache "$Target/release/$($libraries[0].name).dll"
}

function Get-DistUtf8Bytes {
    param([string]$Value)
    return ,([Text.UTF8Encoding]::new($false, $true).GetBytes($Value))
}

function Get-DistBytesHash {
    param([byte[]]$Bytes)
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant()
}

function Write-DistLp {
    param([IO.BinaryWriter]$Writer, [string]$Value)
    $bytes = Get-DistUtf8Bytes $Value
    if ($bytes.Length -gt 32767 -or $Writer.BaseStream.Length + 4L + $bytes.Length -gt 1048576) { throw 'Canonical encoding limit exceeded.' }
    $Writer.Write([uint32]$bytes.Length)
    $Writer.Write([byte[]]$bytes)
}

function Get-DistUuidV5 {
    param([string]$Namespace, [byte[]]$Name)
    $hex = $Namespace.Trim('{', '}').Replace('-', '')
    if ($hex -cnotmatch '^[0-9A-F]{32}$') { throw 'Invalid canonical UUID namespace.' }
    # Parse RFC bytes directly: Guid.ToByteArray would swap the first three fields.
    $bytes = [byte[]]::new(16 + $Name.Length)
    [Convert]::FromHexString($hex).CopyTo($bytes, 0)
    $Name.CopyTo($bytes, 16)
    $digest = [Security.Cryptography.SHA1]::HashData($bytes)
    $digest[6] = ($digest[6] -band 0x0f) -bor 0x50
    $digest[8] = ($digest[8] -band 0x3f) -bor 0x80
    $value = [Convert]::ToHexString([byte[]]$digest[0..15])
    return '{' + $value.Substring(0, 8) + '-' + $value.Substring(8, 4) + '-' + $value.Substring(12, 4) + '-' + $value.Substring(16, 4) + '-' + $value.Substring(20, 12) + '}'
}

function Get-DistComponent {
    param([string]$Architecture, [string]$Kind, [string]$Identity)
    $stream = [IO.MemoryStream]::new()
    $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([Text.Encoding]::ASCII.GetBytes('KWC1'))
        foreach ($value in $Architecture, $Kind, $Identity, '1') { Write-DistLp $writer $value }
        $guid = Get-DistUuidV5 '{E596D49A-3F36-5A1A-89C8-7D052FDE6C47}' $stream.ToArray()
        return [pscustomobject]@{ Guid = $guid; Id = 'C' + $guid.Trim('{', '}').Replace('-', '') }
    } finally { $writer.Dispose(); $stream.Dispose() }
}

function Get-DistStartMenuLink {
    # start-menu-v1: deterministic 76 + 788 + 4 bytes; no COM or shell access.
    $bytes = [byte[]]::new(868)
    [BitConverter]::GetBytes([uint32]0x4c).CopyTo($bytes, 0)
    [Convert]::FromHexString('0114020000000000C000000000000046').CopyTo($bytes, 4)
    [BitConverter]::GetBytes([uint32]0x02000200).CopyTo($bytes, 20)
    [BitConverter]::GetBytes([uint32]1).CopyTo($bytes, 60)
    [BitConverter]::GetBytes([uint32]0x314).CopyTo($bytes, 76)
    [BitConverter]::GetBytes([uint32]2684354561).CopyTo($bytes, 80)
    $target = '%LOCALAPPDATA%\Programs\Kwil\kwil.exe'
    [Text.Encoding]::ASCII.GetBytes($target).CopyTo($bytes, 84)
    [Text.Encoding]::Unicode.GetBytes($target).CopyTo($bytes, 344)
    return ,$bytes
}

function Get-DistResourceIdentity {
    param($Row)
    switch -CaseSensitive ($Row.kind) {
        'F' { return $Row.root + ':' + $Row.relative.ToLowerInvariant() }
        'R' { return 'H:' + $Row.relative.ToLowerInvariant() + ':' + $Row.name.ToLowerInvariant() }
        'K' { return 'H:' + $Row.relative.ToLowerInvariant() + ':#key' }
        default { throw 'Unknown inventory resource kind.' }
    }
}

function New-DistResource {
    param([string]$Kind, [string]$Root, [string]$Relative, [string]$Component)
    return [ordered]@{ id = '-'; kind = $Kind; root = $Root; relative = $Relative; name = '-'; data = '-';
        size = '-'; sha256 = '-'; component = $Component; source = '-'; fileKey = '-'; registryKey = '-'; valueType = '0' }
}

function Assert-DistInstallerRegistryData {
    param($Row, [object[]]$Inventory, [string]$Architecture)
    if ($Row.kind -cne 'R' -or $Row.root -cne 'H' -or $Row.valueType -cne '1') { throw 'Registry resource type mismatch.' }
    $family = '{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}'
    if ($Row.name -ceq 'n:Owner') {
        $files = @($Inventory | Where-Object { $_.kind -ceq 'F' -and $_.component -ceq $Row.component })
        if ($files.Count -ne 1) { throw 'File Owner component mismatch.' }
        $identity = Get-DistResourceIdentity $files[0]
        $component = Get-DistComponent $Architecture 'F' $identity
        if ($Row.component -cne $component.Id -or $Row.relative -cne "Software\Kwil\Installer\$family\Components\$($component.Guid)" -or
            $Row.data -cne "$family|$Architecture|1|$identity") { throw 'File Owner data mismatch; only the exact fixed family GUID literal is permitted.' }
        return
    }
    $shell = 'Software\Classes\*\shell\Kwil.3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A'
    $expected = if ($Row.relative -ceq $shell -and $Row.name -ceq '@') { 'Open with Kwil' }
        elseif ($Row.relative -ceq $shell -and $Row.name -ceq 'n:Icon') { '"${I}kwil.exe",0' }
        elseif ($Row.relative -ceq "$shell\command" -and $Row.name -ceq '@') { '"${I}kwil.exe" "%1"' }
        else { throw 'Unexpected registry data resource.' }
    if ($Row.data -cne $expected) { throw 'Shell registry data mismatch; no arbitrary dollar/braces format is permitted.' }
}

function Get-DistInstallerInventory {
    param([string]$Payload, [ValidateSet('x64', 'arm64')][string]$Architecture)
    $family = '{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}'
    $required = @('kwil.exe', 'kwil.com', 'kwil.ico', 'LICENSE', 'LICENSES.md', 'LICENSES.txt', 'kwil.no-update')
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($Payload)) {
        if ([IO.Path]::GetFileName($path) -cnotin $required -or [IO.Directory]::Exists($path)) { throw 'Unexpected MSI staging entry.' }
    }
    $entries = @(Get-DistFileRecords $Payload 536870912 1073740956)
    if ($entries.Count -ne 7 -or @($entries | Where-Object { $_.Path -cnotin $required }).Count -ne 0) { throw 'MSI payload must contain exactly the seven schema-1 root files before metadata staging.' }
    $rows = [Collections.Generic.List[object]]::new()
    $total = 868L
    foreach ($relative in @($required) + @('Kwil.lnk')) {
        $root = if ($relative -ceq 'Kwil.lnk') { 'P' } else { 'I' }
        $identity = $root + ':' + $relative.ToLowerInvariant()
        $component = Get-DistComponent $Architecture 'F' $identity
        $row = New-DistResource 'F' $root $relative $component.Id
        if ($root -ceq 'P') {
            $row.size = '868'; $row.sha256 = Get-DistBytesHash (Get-DistStartMenuLink); $row.source = 'g'
        } else {
            $file = @($entries | Where-Object { $_.Path -ceq $relative })[0]
            if ($file.Bytes -gt 536870912) { throw 'Installed file size limit exceeded.' }
            $total += $file.Bytes
            if ($total -gt 1073741824) { throw 'Installed file total limit exceeded.' }
            $row.size = $file.Bytes.ToString([Globalization.CultureInfo]::InvariantCulture)
            $row.sha256 = $file.Sha256; $row.source = 'p'
        }
        $rows.Add($row)
        $owner = New-DistResource 'R' 'H' "Software\Kwil\Installer\$family\Components\$($component.Guid)" $component.Id
        $owner.name = 'n:Owner'; $owner.data = "$family|$Architecture|1|$identity"; $owner.valueType = '1'
        $rows.Add($owner)
    }
    $shell = 'Software\Classes\*\shell\Kwil.3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A'
    foreach ($value in @(
        @{ relative = $shell; name = '@'; data = 'Open with Kwil' },
        @{ relative = $shell; name = 'n:Icon'; data = '"${I}kwil.exe",0' },
        @{ relative = "$shell\command"; name = '@'; data = '"${I}kwil.exe" "%1"' }
    )) {
        $component = Get-DistComponent $Architecture 'R' ('H:' + $value.relative.ToLowerInvariant() + ':' + $value.name.ToLowerInvariant())
        $row = New-DistResource 'R' 'H' $value.relative $component.Id
        $row.name = $value.name; $row.data = $value.data; $row.valueType = '1'; $rows.Add($row)
    }
    $registry = @($rows | Where-Object { $_.kind -ceq 'R' })
    $keys = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
    foreach ($row in $registry) {
        $identity = $row.relative.ToLowerInvariant()
        if (-not $keys.ContainsKey($identity) -or [StringComparer]::Ordinal.Compare((Get-DistResourceIdentity $row), (Get-DistResourceIdentity $keys[$identity])) -lt 0) { $keys[$identity] = $row }
    }
    foreach ($row in $keys.Values) { $rows.Add((New-DistResource 'K' 'H' $row.relative $row.component)) }
    $byIdentity = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
    foreach ($row in $rows) {
        if (-not $byIdentity.TryAdd((Get-DistResourceIdentity $row), $row)) { throw 'Duplicate normalized inventory identity.' }
    }
    $identities = [string[]]@($byIdentity.Keys)
    [Array]::Sort($identities, [StringComparer]::Ordinal)
    $ordered = @(for ($index = 0; $index -lt $identities.Length; $index++) {
        $row = $byIdentity[$identities[$index]]
        $digits = ($index + 1).ToString('D6', [Globalization.CultureInfo]::InvariantCulture)
        $row.id = 'r' + $digits
        if ($row.kind -ceq 'F') { $row.fileKey = 'F' + $digits }
        if ($row.kind -ceq 'R') { $row.registryKey = 'R' + $digits }
        $row
    })
    if ($ordered.Count -ne 29 -or $keys.Count -ne 10) { throw 'Schema-1 resource count mismatch.' }
    foreach ($row in $ordered) {
        if ($row.kind -ceq 'R') { Assert-DistInstallerRegistryData $row $ordered $Architecture }
    }
    return $ordered
}

function Get-DistInventoryBytes {
    param([object[]]$Inventory)
    if ($Inventory.Count -gt 512) { throw 'Inventory row limit exceeded.' }
    $stream = [IO.MemoryStream]::new(); $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([Text.Encoding]::ASCII.GetBytes('KWR1')); $writer.Write([uint32]$Inventory.Count)
        foreach ($row in $Inventory) {
            foreach ($field in 'id', 'kind', 'root', 'relative', 'name', 'data', 'size', 'sha256', 'component', 'source', 'fileKey', 'registryKey', 'valueType') { Write-DistLp $writer $row[$field] }
        }
        return ,$stream.ToArray()
    } finally { $writer.Dispose(); $stream.Dispose() }
}

function Get-DistInstallerIdentity {
    param($InputMetadata)
    $inventory = Get-DistInventoryBytes $InputMetadata.inventory
    $stream = [IO.MemoryStream]::new(); $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([Text.Encoding]::ASCII.GetBytes('KWB1'))
        foreach ($value in '1', '1', $InputMetadata.familyGuid, $InputMetadata.architecture, $InputMetadata.version,
            $InputMetadata.commit, $InputMetadata.target, '1', $InputMetadata.guard.sha256) { Write-DistLp $writer $value }
        $writer.Write([uint32]$InputMetadata.guard.imports.Count)
        foreach ($import in $InputMetadata.guard.imports) { Write-DistLp $writer $import.load; Write-DistLp $writer $import.dll }
        if ($stream.Length + 4L + $inventory.Length -gt 1048576) { throw 'Build encoding limit exceeded.' }
        $writer.Write([uint32]$inventory.Length); $writer.Write([byte[]]$inventory)
        $buildId = Get-DistBytesHash $stream.ToArray()
    } finally { $writer.Dispose(); $stream.Dispose() }
    $stream = [IO.MemoryStream]::new(); $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([Text.Encoding]::ASCII.GetBytes('KWP1'))
        foreach ($value in $InputMetadata.architecture, $InputMetadata.version, $buildId) { Write-DistLp $writer $value }
        $product = Get-DistUuidV5 '{918F53A7-7A40-5E54-96EB-E2FD9B0C605B}' $stream.ToArray()
        return [pscustomobject]@{ BuildId = $buildId; InventorySha256 = Get-DistBytesHash $inventory; ProductCode = $product }
    } finally { $writer.Dispose(); $stream.Dispose() }
}

function Initialize-DistInstallerInput {
    param([string]$Payload, [string]$GuardPath, [string]$Architecture, [string]$Version, [string]$Commit, [string]$Target)
    Assert-DistInstallerVersion $Version
    $mapping = @{ x64 = 'x86_64-pc-windows-msvc'; arm64 = 'aarch64-pc-windows-msvc' }
    if ($Architecture -cnotin @('x64', 'arm64') -or $Target -cne $mapping[$Architecture] -or $Commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid installer identity arguments.' }
    $inventory = @(Get-DistInstallerInventory $Payload $Architecture)
    $guard = Get-DistPeInfo $GuardPath 16777216
    $machine = if ($Architecture -ceq 'x64') { 0x8664 } else { 0xaa64 }
    if ($guard.Machine -ne $machine -or -not $guard.IsDll) { throw 'Guard architecture/DLL mismatch.' }
    $metadata = [ordered]@{ schema = 1; componentSchema = 1; familyGuid = '{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}';
        version = $Version; commit = $Commit; architecture = $Architecture; target = $Target;
        guard = [ordered]@{ abi = 1; sha256 = $guard.Sha256; imports = @($guard.GuardImports) }; inventory = $inventory }
    $directory = Join-Path $Payload '.installer'
    if (Test-Path -LiteralPath $directory) { throw 'Refusing existing installer input directory.' }
    $null = New-Item -ItemType Directory -Path $directory
    Copy-Item -LiteralPath $GuardPath -Destination (Join-Path $directory 'kwil-installer-guard.dll')
    if ((Get-DistFileSha256 (Join-Path $directory 'kwil-installer-guard.dll') 16777216) -cne $guard.Sha256) { throw 'Guard changed during input staging.' }
    $json = Get-DistUtf8Bytes (($metadata | ConvertTo-Json -Depth 8) + "`n")
    if ($json.Length -gt 1048576) { throw 'Installer input JSON limit exceeded.' }
    [IO.File]::WriteAllBytes((Join-Path $directory 'input.json'), $json)
    Assert-DistJsonEqual (Read-DistInstallerJson (Join-Path $directory 'input.json')) $metadata
    # Freeze all inputs, including the metadata and DLL, in the after-backend hash receipt.
    return $metadata
}

function Get-DistInstallerInputRecords {
    param([string]$Payload)
    $allowed = @('kwil.exe', 'kwil.com', 'kwil.ico', 'LICENSE', 'LICENSES.md', 'LICENSES.txt', 'kwil.no-update', '.installer')
    $count = 0
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($Payload)) {
        $count++
        if ($count -gt 8 -or [IO.Path]::GetFileName($path) -cnotin $allowed) { throw 'Unexpected MSI input entry.' }
        Assert-DistPlainPath $path
        $directory = [IO.Directory]::Exists($path)
        if ($directory -ne ([IO.Path]::GetFileName($path) -ceq '.installer')) { throw 'MSI input object type mismatch.' }
    }
    if ($count -ne 8) { throw 'Missing MSI input entry.' }
    $metadataDirectory = Join-Path $Payload '.installer'
    $count = 0
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($metadataDirectory)) {
        $count++
        if ($count -gt 2 -or [IO.Path]::GetFileName($path) -cnotin @('input.json', 'kwil-installer-guard.dll') -or [IO.Directory]::Exists($path)) { throw 'Unexpected installer metadata entry.' }
        Assert-DistPlainPath $path
        $limit = if ([IO.Path]::GetFileName($path) -ceq 'input.json') { 1048576 } else { 16777216 }
        if ((Get-Item -LiteralPath $path).Length -gt $limit) { throw 'Installer metadata byte limit exceeded.' }
    }
    if ($count -ne 2) { throw 'Missing installer metadata entry.' }
    return Get-DistFileRecords $Payload 536870912 -InstallerInputs
}

function Assert-DistInstallerInputUnchanged {
    param([string]$Payload, [object[]]$Before)
    $after = @(Get-DistInstallerInputRecords $Payload)
    if ($after.Count -ne $Before.Count) { throw 'MSI backend changed the input payload.' }
    for ($i = 0; $i -lt $after.Count; $i++) {
        if ($after[$i].Path -cne $Before[$i].Path -or $after[$i].Bytes -ne $Before[$i].Bytes -or $after[$i].Sha256 -cne $Before[$i].Sha256) { throw 'MSI backend changed the input payload.' }
    }
}

function Copy-DistPayload {
    param([string]$Snapshot, [string]$BinaryDirectory, [string]$Destination, [switch]$Portable)
    # Caller owns an empty destination. Never copy a tree containing profiles/settings.
    if (@(Get-ChildItem -LiteralPath $Destination -Force).Count -ne 0) { throw "Payload destination is not empty: $Destination" }
    Copy-Item -LiteralPath (Join-Path $BinaryDirectory 'kwil.exe') -Destination (Join-Path $Destination 'kwil.exe')
    Copy-Item -LiteralPath (Join-Path $BinaryDirectory 'kwil-console.exe') -Destination (Join-Path $Destination 'kwil.com')
    Copy-Item -LiteralPath (Join-Path $Snapshot 'LICENSE') -Destination (Join-Path $Destination 'LICENSE')
    Copy-Item -LiteralPath (Join-Path $Snapshot 'assets/logo/kwil.ico') -Destination (Join-Path $Destination 'kwil.ico')
    foreach ($name in 'LICENSES.md', 'LICENSES.txt') {
        Copy-Item -LiteralPath (Join-Path $Snapshot "crates/core-syntax/$name") -Destination (Join-Path $Destination $name)
    }
    [IO.File]::WriteAllText((Join-Path $Destination 'kwil.no-update'), "Updates are managed by this distribution. Rebuild/reinstall explicitly.`n", [Text.UTF8Encoding]::new($false))
    if ($Portable) { [IO.File]::WriteAllText((Join-Path $Destination 'kwil.portable'), '') }
}

function Assert-DistJsonElement {
    param([System.Text.Json.JsonElement]$Element, [string]$MemberName = '')
    switch ($Element.ValueKind.ToString()) {
        'Object' {
            $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            foreach ($member in $Element.EnumerateObject()) {
                if ($member.Name.Contains([char]0) -or -not $seen.Add($member.Name)) { throw 'Duplicate or NUL JSON member.' }
                Assert-DistJsonElement $member.Value $member.Name
            }
        }
        'Array' {
            $limit = if ($MemberName -ceq 'imports') { 128 } else { 512 }
            if ($Element.GetArrayLength() -gt $limit) { throw 'Installer JSON row limit exceeded before object construction.' }
            foreach ($child in $Element.EnumerateArray()) { Assert-DistJsonElement $child }
        }
        'String' {
            $value = $Element.GetString()
            if ($value.Contains([char]0)) { throw 'NUL JSON string.' }
            if ($value.Length -gt 32767) { throw 'Installer JSON string limit exceeded.' }
            $null = Get-DistUtf8Bytes $value
        }
        'Number' { if ($Element.GetRawText() -cne '1') { throw 'Unexpected JSON number; schema integers must be exactly 1.' } }
        'False' { }
        default { throw 'Unexpected JSON primitive.' }
    }
}

function Read-DistInstallerJson {
    param([string]$Path)
    Assert-DistPlainPath $Path
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $size = $stream.Length
        if ($size -lt 1 -or $size -gt 1048576) { throw 'Installer JSON byte limit exceeded.' }
        $bytes = [byte[]]::new([int]$size)
        $read = 0
        while ($read -lt $size) {
            $count = $stream.Read($bytes, $read, [Math]::Min(65536, $size - $read))
            if ($count -eq 0) { throw 'Truncated installer JSON read.' }
            $read += $count
        }
    } finally { $stream.Dispose() }
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 239 -and $bytes[1] -eq 187 -and $bytes[2] -eq 191) { throw 'Installer JSON must have no BOM.' }
    $json = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
    $options = [System.Text.Json.JsonDocumentOptions]::new()
    $options.MaxDepth = 8
    $document = [System.Text.Json.JsonDocument]::Parse([string]$json, $options)
    try { Assert-DistJsonElement $document.RootElement }
    finally { $document.Dispose() }
    # Duplicate detection and primitive checks precede construction of a PS object.
    return ConvertFrom-Json -InputObject $json -AsHashtable -Depth 8
}

function Assert-DistJsonEqual {
    param($Actual, $Expected)
    if ($Expected -is [Collections.IDictionary]) {
        if ($Actual -isnot [Collections.IDictionary] -or $Actual.Count -ne $Expected.Count) { throw 'Installer JSON object shape mismatch.' }
        foreach ($key in $Expected.Keys) {
            if ($key -cnotin @($Actual.Keys)) { throw "Installer JSON member mismatch: $key" }
            Assert-DistJsonEqual $Actual[$key] $Expected[$key]
        }
    } elseif ($Expected -is [array]) {
        if ($Actual -isnot [array] -or $Actual.Count -ne $Expected.Count) { throw 'Installer JSON array mismatch.' }
        for ($i = 0; $i -lt $Expected.Count; $i++) { Assert-DistJsonEqual $Actual[$i] $Expected[$i] }
    } elseif ($Expected -is [string]) {
        if ($Actual -isnot [string] -or $Actual -cne $Expected) { throw 'Installer JSON string mismatch.' }
    } elseif ($Expected -is [bool]) {
        if ($Actual -isnot [bool] -or $Actual -ne $Expected) { throw 'Installer JSON boolean mismatch.' }
    } elseif ($Expected -is [int] -or $Expected -is [long]) {
        # JSON parsing produces Int64 even for schema/ABI 1; literals authored
        # above are Int32. Accept those two integer representations symmetrically
        # while retaining exact value comparison and rejecting every other type.
        if (($Actual -isnot [int] -and $Actual -isnot [long]) -or $Actual -ne $Expected) { throw 'Installer JSON integer mismatch.' }
    } else { throw 'Unsupported expected installer JSON type.' }
}

function Assert-DistInstallerSidecar {
    param([string]$MsiPath, $InputMetadata)
    Assert-DistPlainPath $MsiPath
    if (-not (Test-Path -LiteralPath $MsiPath -PathType Leaf) -or (Get-Item -LiteralPath $MsiPath).Length -eq 0) { throw 'Backend returned without a nonempty MSI.' }
    $sidecar = Read-DistInstallerJson ($MsiPath + '.manifest.json')
    $identity = Get-DistInstallerIdentity $InputMetadata
    if ($sidecar.packageCode -isnot [string] -or $sidecar.packageCode -cnotmatch '^\{[0-9A-F]{8}-[0-9A-F]{4}-4[0-9A-F]{3}-[89AB][0-9A-F]{3}-[0-9A-F]{12}\}$') { throw 'Sidecar PackageCode must be a canonical UUIDv4.' }
    $expected = [ordered]@{ schema = 1; familyGuid = $InputMetadata.familyGuid; componentSchema = 1;
        architecture = $InputMetadata.architecture; target = $InputMetadata.target; version = $InputMetadata.version;
        commit = $InputMetadata.commit; buildId = $identity.BuildId; inventorySha256 = $identity.InventorySha256;
        productCode = $identity.ProductCode; packageCode = $sidecar.packageCode; guard = $InputMetadata.guard;
        inventory = $InputMetadata.inventory;
        msi = [ordered]@{ fileName = [IO.Path]::GetFileName($MsiPath); size = (Get-Item -LiteralPath $MsiPath).Length.ToString([Globalization.CultureInfo]::InvariantCulture);
            sha256 = Get-DistFileSha256 $MsiPath };
        inspection = [ordered]@{ database = 'passed'; cabinet = 'passed'; staticImports = 'passed' };
        signed = $false; nativeLifecycle = 'not-run' }
    Assert-DistJsonEqual $sidecar $expected
    return $identity
}

function Get-DistPortableRecords {
    param([string]$Payload)
    $required = @('kwil.exe', 'kwil.com', 'kwil.ico', 'LICENSE', 'LICENSES.md', 'LICENSES.txt', 'kwil.no-update', 'kwil.portable')
    $count = 0
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($Payload)) {
        $count++
        if ($count -gt 8 -or [IO.Path]::GetFileName($path) -cnotin $required -or [IO.Directory]::Exists($path)) { throw 'Portable ZIP input differs from the fixed payload; .installer is forbidden.' }
    }
    $records = @(Get-DistFileRecords $Payload 536870912 1073741824)
    if ($records.Count -ne 8 -or @($records | Where-Object { $_.Path -cnotin $required }).Count -ne 0 -or
        (Test-Path -LiteralPath (Join-Path $Payload '.installer'))) { throw 'Portable ZIP input differs from the fixed payload; .installer is forbidden.' }
    return $records
}

function Assert-DistPortableRecords {
    param([object[]]$Actual, [object[]]$Receipt)
    if ($Actual.Count -ne $Receipt.Count) { throw 'Portable receipt mismatch.' }
    for ($i = 0; $i -lt $Actual.Count; $i++) {
        if ($Actual[$i].Path -cne $Receipt[$i].Path -or $Actual[$i].Bytes -ne $Receipt[$i].Bytes -or $Actual[$i].Sha256 -cne $Receipt[$i].Sha256) { throw 'Portable receipt mismatch.' }
    }
}

function Assert-DistPortableZip {
    param([string]$Path, [object[]]$Receipt)
    $expected = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
    $total = 0L
    if ($Receipt.Count -ne 8) { throw 'Portable ZIP receipt count mismatch.' }
    foreach ($record in $Receipt) {
        if ($record.Bytes -lt 0 -or $record.Bytes -gt 536870912 -or $record.Bytes -gt 1073741824L - $total -or
            $record.Sha256 -cnotmatch '^[0-9a-f]{64}$' -or -not $expected.TryAdd($record.Path, $record)) { throw 'Invalid portable ZIP receipt.' }
        $total += $record.Bytes
    }
    Assert-DistPlainPath $Path
    $zip = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        if ($zip.Entries.Count -ne $Receipt.Count) { throw 'Portable ZIP entry count mismatch.' }
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($entry in $zip.Entries) {
            if (-not $expected.ContainsKey($entry.FullName) -or -not $seen.Add($entry.FullName)) { throw 'Portable ZIP entry name mismatch.' }
            $record = $expected[$entry.FullName]
            if ($entry.Length -ne $record.Bytes) { throw 'Portable ZIP entry size mismatch.' }
            $stream = $entry.Open()
            $hash = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
            try {
                $buffer = [byte[]]::new(65536)
                $read = 0L
                while (($count = $stream.Read($buffer, 0, [int][Math]::Min(65536L, $record.Bytes - $read + 1))) -ne 0) {
                    if ($count -gt $record.Bytes - $read) { throw 'Portable ZIP entry expanded size mismatch.' }
                    $read += $count
                    $hash.AppendData($buffer, 0, $count)
                }
                if ($read -ne $record.Bytes -or [Convert]::ToHexString($hash.GetHashAndReset()).ToLowerInvariant() -cne $record.Sha256) { throw 'Portable ZIP entry content mismatch.' }
            } finally { $hash.Dispose(); $stream.Dispose() }
        }
    } finally { $zip.Dispose() }
}

function New-DistPortableZip {
    param([string]$Payload, [string]$OutputPath, [Parameter(Mandatory)][object[]]$Receipt)
    # Keep the pre-backend receipt as authority. A new observation cannot replace it.
    Assert-DistPortableRecords @(Get-DistPortableRecords $Payload) $Receipt
    if (Test-Path -LiteralPath $OutputPath) { throw 'Refusing existing portable ZIP.' }
    [IO.Compression.ZipFile]::CreateFromDirectory($Payload, $OutputPath, [IO.Compression.CompressionLevel]::Optimal, $false)
    Assert-DistPortableZip $OutputPath $Receipt
}

function Get-DistInstallerHashLimit {
    param([string]$Relative, [long]$InstalledBytes)
    if ($InstalledBytes -lt 868 -or $InstalledBytes -gt 1073741824) { throw 'Installed aggregate byte limit exceeded before hashing.' }
    switch -CaseSensitive ($Relative) {
        '.installer/input.json' { return 1048576L }
        '.installer/kwil-installer-guard.dll' { return 16777216L }
        default {
            if ($Relative -cnotin @('kwil.exe', 'kwil.com', 'kwil.ico', 'LICENSE', 'LICENSES.md', 'LICENSES.txt', 'kwil.no-update')) { throw 'Unexpected installer hash input.' }
            return [Math]::Min(536870912L, 1073741824L - $InstalledBytes)
        }
    }
}

function Get-DistFileLength {
    param([string]$Path)
    return [IO.FileInfo]::new($Path).Length
}

function Get-DistFileRecords {
    param([string]$Directory, [long]$MaximumFileBytes = [long]::MaxValue, [long]$MaximumTotalBytes = [long]::MaxValue,
        [switch]$InstallerInputs)
    Assert-DistPlainPath $Directory
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($Directory)
    $records = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
    $count = 0
    $total = 0L
    # Include the generated Start Menu LNK, which is not an input payload file.
    $installedTotal = 868L
    while ($pending.Count -ne 0) {
        $current = $pending.Pop()
        # Enumerate lazily; reject before collecting beyond the contract's bound.
        foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($current)) {
            $count++
            if ($count -gt 512) { throw 'Packaging enumeration limit exceeded.' }
            $null = Assert-DistContainedPath $Directory $path
            $file = Get-Item -LiteralPath $path -Force
            if ($file.PSIsContainer) { $pending.Push($path); continue }
            $relative = [IO.Path]::GetRelativePath($Directory, $path).Replace('\', '/')
            $length = Get-DistFileLength $path
            $hashLimit = [Math]::Min($MaximumFileBytes, $MaximumTotalBytes - $total)
            if ($InstallerInputs) { $hashLimit = [Math]::Min($hashLimit, (Get-DistInstallerHashLimit $relative $installedTotal)) }
            if ($length -gt $hashLimit) { throw 'Packaging file or installed aggregate byte limit exceeded before hashing.' }
            # The streaming hash gets the remaining allowance too, so growth
            # after the length observation cannot hash beyond the admitted cap.
            $sha256 = Get-DistFileSha256 $path $hashLimit
            if ((Get-DistFileLength $path) -ne $length) { throw 'Input file length changed during hashing.' }
            $total += $length
            if ($InstallerInputs -and -not $relative.StartsWith('.installer/', [StringComparison]::Ordinal)) { $installedTotal += $length }
            $records.Add($relative, [pscustomobject]@{ Path = $relative; Bytes = $length; Sha256 = $sha256 })
        }
    }
    $names = [string[]]@($records.Keys)
    [Array]::Sort($names, [StringComparer]::Ordinal)
    foreach ($name in $names) { $records[$name] }
}

function Write-DistChecksums {
    param([string]$Directory, [string[]]$Names)
    $lines = foreach ($name in $Names) {
        if ([IO.Path]::GetFileName($name) -cne $name -or $name -match '[\r\n]') { throw 'Unsafe checksum file name.' }
        $file = Assert-DistContainedPath $Directory (Join-Path $Directory $name)
        "{0}  {1}" -f (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant(), $name
    }
    $path = Join-Path $Directory 'SHA256SUMS'
    if (Test-Path -LiteralPath $path) { throw "Refusing existing checksum file: $path" }
    [IO.File]::WriteAllText($path, ($lines -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
}

function Publish-DistDirectory {
    param([string]$Parent, [string]$Staging, [string]$Destination, [string]$Token, [string[]]$ExpectedNames)
    $source = Assert-DistContainedPath $Parent $Staging
    $target = Assert-DistContainedPath $Parent $Destination
    if (Test-Path -LiteralPath $target) { throw "Refusing existing output (known or unknown): $target" }
    $marker = Join-Path $source '.kwil-dist-owner'
    Assert-DistPlainPath $marker
    if ([IO.File]::ReadAllText($marker) -cne $Token) { throw 'Staging ownership mismatch.' }
    $actual = @(Get-ChildItem -LiteralPath $source -Force | ForEach-Object {
        if ($_.PSIsContainer -or ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Unexpected staging directory or link.' }
        $_.Name
    })
    $expected = @('.kwil-dist-owner') + $ExpectedNames
    if (@(Compare-Object ($actual | Sort-Object) ($expected | Sort-Object) -CaseSensitive).Count -ne 0) {
        throw 'Staging contains missing or unknown artifacts; publication refused.'
    }
    # Directory.Move refuses existing targets; no replacement even in a reservation race.
    [IO.Directory]::Move($source, $target)
}

function Invoke-KwilDist {
    param([string]$Repository, [int]$BuildTimeout, [int]$PackageTimeout, [bool]$KeepWork)
    if (-not $IsWindows) { throw 'Windows is required for the native MSVC distribution.' }
    if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required.' }
    Assert-DistPlainPath $Repository
    $tools = @{}
    foreach ($name in 'git.exe', 'rustup.exe', 'pwsh.exe', 'makecab.exe') {
        $command = Get-Command $name -CommandType Application -ErrorAction Stop | Select-Object -First 1
        $tools[$name] = $command.Source
    }
    $architecture = switch ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) {
        'X64' { 'x64' }
        'Arm64' { 'arm64' }
        default { throw 'Only native Windows x64 and ARM64 are supported.' }
    }
    $target = if ($architecture -eq 'x64') { 'x86_64-pc-windows-msvc' } else { 'aarch64-pc-windows-msvc' }
    $cacheParent = 'E:/Development/_cache/cargo-target'
    Assert-DistPlainPath $cacheParent
    if (-not (Test-Path -LiteralPath $cacheParent -PathType Container)) { throw "Expected repository cache parent: $cacheParent" }
    $token = [Guid]::NewGuid().ToString('N')
    $work = New-DistOwnedDirectory $cacheParent "kwil-dist-work-$token" $token
    $logs = Join-Path $work 'logs'
    $null = New-Item -ItemType Directory -Path $logs
    Write-Host "Driver PID=$PID. Owned work: $work"
    [IO.File]::WriteAllText((Join-Path $logs 'processes.log'), "driver pid=$PID source=committed-HEAD`n")
    $outputLock = $null
    $cacheLock = $null
    $staging = $null
    $outputLockPath = $null
    $cacheLockPath = $null
    $completed = $false
    $script:DistUnreapedProcess = $false
    try {
        $headLog = Invoke-DistProcess $tools['git.exe'] @('-C', $Repository, 'rev-parse', '--verify', 'HEAD^{commit}') $work 30 $logs 'head'
        $commit = [IO.File]::ReadAllText($headLog.FullName).Trim()
        if ($commit -cnotmatch '^[0-9a-f]{40}$') { throw 'HEAD did not resolve to an exact 40-hex commit.' }
        Write-Host "Building committed HEAD $commit. Dirty and untracked working code is explicitly ignored."
        [IO.File]::WriteAllText((Join-Path $logs 'source-commit.txt'), $commit + "`n")
        $archive = Join-Path $work 'source.zip'
        Export-DistSourceArchive $tools['git.exe'] $Repository $commit $archive $work $logs
        $snapshot = Join-Path $work 'snapshot'
        [IO.Compression.ZipFile]::ExtractToDirectory($archive, $snapshot)
        Assert-DistPlainPath $snapshot
        Assert-DistInstallerSources $snapshot
        $backend = Join-Path $snapshot 'scripts/windows-msi.ps1'
        foreach ($inputPath in @($backend, (Join-Path $snapshot 'Cargo.lock'), (Join-Path $snapshot 'scripts/check-static-runtime.py'), (Join-Path $snapshot 'assets/logo/kwil.ico'),
            (Join-Path $snapshot 'LICENSE'), (Join-Path $snapshot 'crates/core-syntax/LICENSES.md'),
            (Join-Path $snapshot 'crates/core-syntax/LICENSES.txt'))) {
            if (-not (Test-Path -LiteralPath $inputPath -PathType Leaf)) { throw "Required committed input missing: $inputPath . Commit the MSI backend and intended source before building." }
        }
        $toolchainText = [IO.File]::ReadAllText((Join-Path $snapshot 'rust-toolchain.toml'))
        if ($toolchainText -notmatch '(?m)^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$') { throw 'Expected a pinned numeric Rust toolchain.' }
        $toolchain = $Matches[1]
        $rootManifest = [IO.File]::ReadAllText((Join-Path $snapshot 'Cargo.toml'))
        if ($rootManifest -notmatch '(?ms)^\[workspace\.package\]\s*\r?\n(?:(?!^\[).)*?^version\s*=\s*"([^"]+)"') { throw 'Missing workspace Cargo version.' }
        $cargoVersion = $Matches[1]
        $names = Get-DistNames $cargoVersion $commit $target
        $dist = Join-Path $Repository 'dist'
        Assert-DistPlainPath $dist
        if (-not (Test-Path -LiteralPath $dist)) { $null = New-Item -ItemType Directory -Path $dist }
        $destination = Assert-DistContainedPath $dist (Join-Path $dist $names.Namespace)
        if (Test-Path -LiteralPath $destination) { throw "Refusing existing output: $destination" }
        $outputLockPath = Join-Path $dist ".$($names.Namespace).lock"
        $outputLock = New-DistReservation $outputLockPath $token
        $staging = New-DistOwnedDirectory $dist ".$($names.Namespace).$token.staging" $token
        $cache = Join-Path $cacheParent 'kwil-dist'
        $cacheLockPath = Join-Path $cacheParent 'kwil-dist.build.lock'
        $cacheLock = New-DistReservation $cacheLockPath $token
        Initialize-DistCache $cache
        $guardCache = Join-Path $cacheParent 'kwil-dist-guard'
        Initialize-DistCache $guardCache
        $rustcLog = Invoke-DistProcess $tools['rustup.exe'] (Get-DistRustupRunArguments $toolchain @('rustc', '-vV')) $snapshot 30 $logs 'rustc-version'
        $rustcVersion = [IO.File]::ReadAllText($rustcLog.FullName).Trim()
        if ($rustcVersion -notmatch "(?m)^host: $([regex]::Escape($target))\r?$" ) { throw "Pinned toolchain must be native $target; no targets are installed automatically." }
        # The guard is built separately from the archived commit, with its own cache.
        # ROOT integrates its manifest/lock; a missing integration fails before the app build.
        $guardDll = Use-DistBuildEnvironment $commit $guardCache {
            Build-DistGuardSnapshot $tools['rustup.exe'] $toolchain $target $snapshot $guardCache $logs $BuildTimeout
        }
        $guardInfo = Get-DistPeInfo $guardDll 16777216
        $machine = if ($architecture -eq 'x64') { 0x8664 } else { 0xaa64 }
        if ($guardInfo.Machine -ne $machine -or -not $guardInfo.IsDll) { throw 'Guard architecture/DLL mismatch before application compilation.' }
        Use-DistBuildEnvironment $commit $cache {
            # Plain rustup run fails for a missing toolchain; no --install retry.
            $metadataLog = Invoke-DistProcess $tools['rustup.exe'] (Get-DistRustupRunArguments $toolchain @('cargo', 'metadata', '--no-deps', '--format-version', '1', '--locked', '--offline')) $snapshot 60 $logs 'cargo-metadata'
            $metadata = [IO.File]::ReadAllText($metadataLog.FullName) | ConvertFrom-Json
            $package = @($metadata.packages | Where-Object { $_.name -ceq 'kwil' })
            if ($package.Count -ne 1 -or $package[0].version -cne $cargoVersion) { throw 'Cargo package/workspace version mismatch.' }
            Build-DistSnapshot $tools['rustup.exe'] $toolchain $target $snapshot $logs $BuildTimeout
            $binaries = Join-Path $cache "$target/release"
            $gui = Get-DistPeInfo (Join-Path $binaries 'kwil.exe')
            $console = Get-DistPeInfo (Join-Path $binaries 'kwil-console.exe')
            $machine = if ($architecture -eq 'x64') { 0x8664 } else { 0xaa64 }
            if ($gui.Machine -ne $machine -or $console.Machine -ne $machine -or $gui.Subsystem -ne 2 -or $console.Subsystem -ne 3) {
                throw 'Release pair architecture/subsystem mismatch (GUI=2, console=3 required).'
            }
            $msiPayload = Join-Path $work 'msi-payload'
            $portablePayload = Join-Path $work 'portable-payload'
            $null = New-Item -ItemType Directory -Path $msiPayload
            $null = New-Item -ItemType Directory -Path $portablePayload
            Copy-DistPayload $snapshot $binaries $msiPayload
            Copy-DistPayload $snapshot $binaries $portablePayload -Portable
            $installerInput = Initialize-DistInstallerInput $msiPayload $guardDll $architecture $cargoVersion $commit $target
            $msiRecords = @(Get-DistInstallerInputRecords $msiPayload)
            $portableRecords = @(Get-DistPortableRecords $portablePayload)
            $msiPath = Join-Path $staging $names.Msi
            $null = Invoke-DistProcess $tools['pwsh.exe'] @('-NoLogo', '-NoProfile', '-File', $backend,
                '-PayloadDirectory', $msiPayload, '-OutputPath', $msiPath, '-Version', $cargoVersion,
                '-Architecture', $architecture, '-Commit', $commit, '-IconPath', (Join-Path $msiPayload 'kwil.ico')) $snapshot ([Math]::Min($PackageTimeout, 120)) $logs 'msi-package'
            Assert-DistInstallerInputUnchanged $msiPayload $msiRecords
            $installerIdentity = Assert-DistInstallerSidecar $msiPath $installerInput
            New-DistPortableZip $portablePayload (Join-Path $staging $names.Zip) $portableRecords
            $instructions = @"
Kwil $cargoVersion ($commit), $target. Unsigned local distribution.
Install: double-click $($names.Msi), then follow Windows Installer prompts.
The MSI installs per user and provides an Explorer Open with Kwil verb.
It does not select default file associations. Uninstall through Windows Settings > Apps.
MSI settings/recovery data use the normal user location and are retained on uninstall.
Repair only with the retained original MSI: msiexec /i "$($names.Msi)" REINSTALL=ALL REINSTALLMODE=amus
Keep $($names.Msi).manifest.json alongside the original MSI; it records unsigned package evidence.
Portable: extract $($names.Zip) into a new user-writable directory; run kwil.exe.
Keep kwil.exe and kwil.com together. For explicit terminal use, run kwil.com.
kwil.portable keeps portable settings beside the executable; it is absent from the MSI.
Both payloads contain kwil.no-update: update this managed distribution explicitly.
No installer or application was launched by dist.bat. Nothing was signed or published.
SHA256SUMS covers the MSI, MSI sidecar, ZIP, this file and manifest.json; it is not a signature.
"@
            [IO.File]::WriteAllText((Join-Path $staging 'INSTALL.txt'), $instructions + "`n")
            $manifest = [ordered]@{
                SchemaVersion = 1; SourceCommit = $commit; CargoVersion = $cargoVersion
                Target = $target; Architecture = $architecture; Toolchain = $toolchain; RustcVersion = $rustcVersion
                SourceMode = 'git-archive-committed-HEAD'; WorkingTreeCodeIncluded = $false
                DriverSha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
                StaticRuntimeGate = 'PowerShell port of scripts/check-static-runtime.py, normal and delay imports'
                StaticRuntimeReferenceSha256 = (Get-FileHash -LiteralPath (Join-Path $snapshot 'scripts/check-static-runtime.py') -Algorithm SHA256).Hash.ToLowerInvariant()
                Pe = @{ 'kwil.exe' = $gui; 'kwil.com' = $console }
                GuardPe = $guardInfo
                InstallerContractSha256 = (Get-FileHash -LiteralPath (Join-Path $snapshot 'scripts/windows-installer-contract.json') -Algorithm SHA256).Hash.ToLowerInvariant()
                InstallerDecisionSha256 = (Get-FileHash -LiteralPath (Join-Path $snapshot 'docs/decisions/0004-windows-installer.md') -Algorithm SHA256).Hash.ToLowerInvariant()
                InstallerBuildId = $installerIdentity.BuildId; InstallerProductCode = $installerIdentity.ProductCode
                InstallerSidecar = $names.Msi + '.manifest.json'
                MsiPayload = $msiRecords
                PortablePayload = $portableRecords
                Artifacts = @(Get-DistFileRecords $staging | Where-Object { $_.Path -ne '.kwil-dist-owner' })
                Processes = @([IO.File]::ReadAllLines((Join-Path $logs 'processes.log')))
                Checksums = 'SHA256SUMS includes manifest.json; manifest does not hash itself or SHA256SUMS'
                CliValidation = 'Not run; no application launch'; Signed = $false; Published = $false
            }
            [IO.File]::WriteAllText((Join-Path $staging 'manifest.json'), ($manifest | ConvertTo-Json -Depth 12) + "`n")
            Write-DistChecksums $staging @($names.Msi, ($names.Msi + '.manifest.json'), $names.Zip, 'INSTALL.txt', 'manifest.json')
        }
        Publish-DistDirectory $dist $staging $destination $token @($names.Msi, ($names.Msi + '.manifest.json'), $names.Zip, 'INSTALL.txt', 'manifest.json', 'SHA256SUMS')
        $completed = $true
        Write-Host "Prepared local artifacts: $destination"
        Write-Host "MSI: $(Join-Path $destination $names.Msi)"
        Write-Host "Portable ZIP: $(Join-Path $destination $names.Zip)"
        Write-Host "Install instructions: $(Join-Path $destination 'INSTALL.txt')"
        Write-Host 'No install, app launch, signing, or publication performed.'
    } catch {
        Write-Warning "Failed artifacts retained at: $work"
        if ($staging -and (Test-Path -LiteralPath $staging)) { Write-Warning "Unpublished staging retained at: $staging" }
        throw
    } finally {
        Close-DistReservations $cacheLock $cacheLockPath $outputLock $outputLockPath $token
        if ($completed -and -not $KeepWork -and -not $script:DistUnreapedProcess) {
            try { Remove-DistOwnedDirectory $cacheParent $work $token }
            catch { Write-Warning "Success cleanup refused; inspect retained work $work : $_" }
        } elseif ($completed) { Write-Host "Kept owned work/logs: $work" }
    }
}

# Dot sourcing exposes helpers for tiny isolated tests; it never builds or probes tools.
if ($MyInvocation.InvocationName -ne '.') {
    try {
        Invoke-KwilDist -Repository ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))) `
            -BuildTimeout $BuildTimeoutSeconds -PackageTimeout $PackageTimeoutSeconds -KeepWork $KeepWorkDirectory.IsPresent
        exit 0
    } catch {
        [Console]::Error.WriteLine("Kwil dist: $_")
        exit 1
    }
}
