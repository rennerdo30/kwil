#requires -Version 7.0
<#
Tiny isolated source/fixture checks, for ROOT to run explicitly:
  pwsh -NoLogo -NoProfile -File .\scripts\test-dist.ps1
Focused archive regression (mock only by default; opt in to tiny real Git):
  pwsh -NoLogo -NoProfile -File .\scripts\test-dist.ps1 -ArchiveOnly -RunGitArchiveFixture
Without -RunGitArchiveFixture no subprocess is launched. The opt-in fixture
uses only a disposable repository, child-only Git settings and bounded Git
processes with exact PID receipts. No Cargo, compiler, app or backend runs.
Synthetic PE images are fixtures only, never represented as a real release build.
All output is below one GUID-owned directory in the approved kwil-dist cache.
Real Git fixture artifacts and PID receipts are retained there for ROOT review.
The empty-environment regression cases require .NET 9 or later; they never skip
or normalize an empty entry to an absent entry on an older runtime.
#>
[CmdletBinding()]
param([switch]$ArchiveOnly, [switch]$RunGitArchiveFixture)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([Environment]::Version.Major -lt 9) { throw 'Exact empty-environment fixtures require .NET 9 or later; no fixture environment was changed.' }
. (Join-Path $PSScriptRoot 'dist.ps1')

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "Assertion failed: $Message" }
}

function Assert-Throws {
    param([scriptblock]$Action, [string]$Pattern)
    $caught = $null
    try { & $Action } catch { $caught = $_ }
    if ($null -eq $caught -or $caught.ToString() -notmatch $Pattern) {
        throw "Expected failure matching '$Pattern'; got '$caught'."
    }
}

function Assert-TestProcessEnvironment {
    param([string]$Name, $Expected, [string]$Message)
    $actual = [Environment]::GetEnvironmentVariable($Name, 'Process')
    $exists = -not [object]::ReferenceEquals($actual, $null)
    Assert-True ($exists -eq $Expected.Exists) "$Message (existence)"
    # Object.Equals keeps null distinct from "" and compares present strings
    # exactly. Do not coalesce null/empty or cast either side to string.
    Assert-True ([object]::Equals($actual, $Expected.Value)) "$Message (exact value)"
}

function Assert-TestNonInstallingRustupArguments {
    param([string[]]$Arguments)
    # ROOT's retained installed-CLI help lists only --install as the opt-in
    # installer option. Assert that real grammar, not an invented opt-out flag.
    Assert-True ($Arguments.Count -ge 3 -and $Arguments[0] -ceq 'run' -and
        $Arguments[1] -ceq '1.99.1' -and $Arguments[2] -cin @('cargo', 'rustc')) 'supported rustup run TOOLCHAIN COMMAND grammar'
    Assert-True ('--install' -cnotin $Arguments -and '--no-auto-install' -cnotin $Arguments) 'no installing or unsupported rustup option'
}

function Test-DistArchiveBoundary {
    param([string]$Root)
    $originalProcess = (Get-Item Function:Invoke-DistProcess).ScriptBlock
    $calls = [Collections.Generic.List[object]]::new()
    $repository = Join-Path $Root 'repo with spaces & (fixture)'
    $archivePath = Join-Path $Root 'source with spaces & (fixture).zip'
    $sourceCommit = '0123456789abcdef0123456789abcdef01234567'
    try {
        function Invoke-DistProcess {
            param($FilePath, $Arguments, $WorkingDirectory, $TimeoutSeconds, $LogDirectory, $Label)
            $calls.Add([pscustomobject]@{ File = $FilePath; Args = @($Arguments); Cwd = $WorkingDirectory;
                Timeout = $TimeoutSeconds; Logs = $LogDirectory; Label = $Label })
            if ($calls.Count -eq 2) { throw 'mock archive failure' }
        }
        Export-DistSourceArchive 'git fixture.exe' $repository $sourceCommit $archivePath $Root $Root
        Assert-Throws {
            Export-DistSourceArchive 'git fixture.exe' $repository $sourceCommit $archivePath $Root $Root
        } 'mock archive failure'
        Assert-True ($calls.Count -eq 2) 'one archive attempt per call, no retry after failure'
        $expected = @('-C', $repository, '-c', 'core.autocrlf=false', '-c', 'core.eol=lf',
            'archive', '--format=zip', "--output=$archivePath", $sourceCommit)
        foreach ($call in $calls) {
            Assert-True ($call.File -ceq 'git fixture.exe' -and $call.Args.Count -eq $expected.Count) 'exact archive executable and argv count'
            for ($i = 0; $i -lt $expected.Count; $i++) {
                Assert-True ($call.Args[$i] -ceq $expected[$i]) "literal command-scoped archive argument $i"
            }
            Assert-True ($call.Cwd -ceq $Root -and $call.Timeout -eq 60 -and
                $call.Logs -ceq $Root -and $call.Label -ceq 'archive') 'archive retains bounded process boundary and receipts'
        }
    } finally { Set-Item Function:Invoke-DistProcess -Value $originalProcess }
    $driverCalls = @((Get-Item Function:Invoke-KwilDist).ScriptBlock.Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.CommandAst] -and
            $node.GetCommandName() -ceq 'Export-DistSourceArchive'
    }, $true))
    Assert-True ($driverCalls.Count -eq 1) 'production snapshot uses the tested archive boundary'
}

function Test-DistRealGitArchive {
    param([string]$Root)
    $git = (Get-Command git.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $repository = Join-Path $Root 'tiny repository with spaces & (fixture)'
    $logs = Join-Path $Root 'git-archive-logs'
    $template = Join-Path $Root 'empty-git-template'
    $globalConfig = Join-Path $Root 'empty-git-config'
    foreach ($directory in $repository, $logs, $template) { $null = New-Item -ItemType Directory -Path $directory }
    [IO.File]::WriteAllBytes($globalConfig, [byte[]]::new(0))
    $originalInfo = (Get-Item Function:New-DistProcessInfo).ScriptBlock
    try {
        # Only child ProcessStartInfo is changed. Inherited Git overrides cannot
        # redirect fixture writes into the owner's index/repository or read their
        # config/attributes. No process/user/machine environment or Git config is set.
        function New-DistProcessInfo {
            param([string]$FilePath, [string[]]$Arguments, [string]$WorkingDirectory)
            $info = & $originalInfo $FilePath $Arguments $WorkingDirectory
            foreach ($name in @($info.Environment.Keys)) {
                if ($name.StartsWith('GIT_', [StringComparison]::OrdinalIgnoreCase)) { $null = $info.Environment.Remove($name) }
            }
            $info.Environment['GIT_CONFIG_NOSYSTEM'] = '1'
            $info.Environment['GIT_CONFIG_GLOBAL'] = $globalConfig
            $info.Environment['GIT_ATTR_NOSYSTEM'] = '1'
            $info.Environment['GIT_CONFIG_COUNT'] = '3'
            $info.Environment['GIT_CONFIG_KEY_0'] = 'core.autocrlf'
            $info.Environment['GIT_CONFIG_VALUE_0'] = 'true'
            $info.Environment['GIT_CONFIG_KEY_1'] = 'core.eol'
            $info.Environment['GIT_CONFIG_VALUE_1'] = 'crlf'
            $info.Environment['GIT_CONFIG_KEY_2'] = 'core.attributesfile'
            $info.Environment['GIT_CONFIG_VALUE_2'] = $globalConfig
            foreach ($role in 'AUTHOR', 'COMMITTER') {
                $info.Environment["GIT_${role}_NAME"] = 'rennerdo30'
                $info.Environment["GIT_${role}_EMAIL"] = '9086097+rennerdo30@users.noreply.github.com'
            }
            return $info
        }
        $null = Invoke-DistProcess $git @('-C', $repository, 'init', '--quiet', '--object-format=sha1',
            "--template=$template") $Root 10 $logs 'fixture-init'
        foreach ($directory in 'scripts', 'docs/decisions', 'crates/installer-guard/src') {
            $null = New-Item -ItemType Directory -Path (Join-Path $repository $directory)
        }
        # Use the actual reviewed LF JSON and mixed-ending document, not
        # normalized text copies. The other installer inputs are inert dummies.
        foreach ($relative in 'scripts/windows-installer-contract.json', 'docs/decisions/0004-windows-installer.md') {
            Copy-Item -LiteralPath (Join-Path $PSScriptRoot "../$relative") -Destination (Join-Path $repository $relative)
        }
        foreach ($relative in 'scripts/windows-msi.ps1', 'crates/installer-guard/Cargo.toml', 'crates/installer-guard/src/lib.rs') {
            [IO.File]::WriteAllText((Join-Path $repository $relative), "inert fixture`n")
        }
        [IO.File]::WriteAllText((Join-Path $repository 'lf.txt'), "first`nsecond`n", [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText((Join-Path $repository 'crlf.txt'), "first`r`nsecond`r`n", [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText((Join-Path $repository 'mixed.txt'), "first`r`nsecond`nthird`r`n", [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllBytes((Join-Path $repository 'binary.bin'), [byte[]]@(0, 10, 13, 10, 255))
        $expected = @{}
        foreach ($relative in 'scripts/windows-installer-contract.json', 'docs/decisions/0004-windows-installer.md',
            'scripts/windows-msi.ps1', 'crates/installer-guard/Cargo.toml', 'crates/installer-guard/src/lib.rs',
            'lf.txt', 'crlf.txt', 'mixed.txt', 'binary.bin') {
            $expected[$relative] = (Get-FileHash -LiteralPath (Join-Path $repository $relative) -Algorithm SHA256).Hash
        }
        $null = Invoke-DistProcess $git @('-C', $repository, '-c', 'core.autocrlf=false', '-c', 'core.eol=lf',
            'add', '--', 'scripts', 'docs', 'crates', 'lf.txt', 'crlf.txt', 'mixed.txt', 'binary.bin') $Root 10 $logs 'fixture-add'
        $treeLog = Invoke-DistProcess $git @('-C', $repository, 'write-tree') $Root 10 $logs 'fixture-tree'
        $tree = [IO.File]::ReadAllText($treeLog.FullName).Trim()
        Assert-True ($tree -cmatch '^[0-9a-f]{40}$') 'fixture tree is exact SHA-1'
        # Plumbing creates an unreferenced disposable commit, without hooks,
        # signing, branch updates or writes to identity/config settings.
        $commitLog = Invoke-DistProcess $git @('-C', $repository, 'commit-tree', $tree, '-m', 'Tiny archive regression fixture') $Root 10 $logs 'fixture-commit'
        $sourceCommit = [IO.File]::ReadAllText($commitLog.FullName).Trim()
        Assert-True ($sourceCommit -cmatch '^[0-9a-f]{40}$') 'fixture commit is exact SHA-1'
        $configPath = Join-Path $repository '.git/config'
        $configBefore = (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash
        [IO.File]::WriteAllText((Join-Path $repository 'lf.txt'), 'dirty working text must not ship')
        [IO.File]::WriteAllText((Join-Path $repository 'untracked.txt'), 'untracked text must not ship')
        $legacyZip = Join-Path $Root 'legacy-source.zip'
        $null = Invoke-DistProcess $git @('-C', $repository, 'archive', '--format=zip', "--output=$legacyZip", $sourceCommit) $Root 60 $logs 'fixture-legacy-archive'
        $legacySnapshot = Join-Path $Root 'legacy-snapshot'
        [IO.Compression.ZipFile]::ExtractToDirectory($legacyZip, $legacySnapshot)
        Assert-True ((Get-FileHash -LiteralPath (Join-Path $legacySnapshot 'lf.txt')).Hash -cne $expected['lf.txt']) 'broken archive control converts LF under hostile autocrlf'
        Assert-Throws { Assert-DistInstallerSources $legacySnapshot } 'Frozen installer contract mismatch'
        $fixedZip = Join-Path $Root 'fixed-source.zip'
        Export-DistSourceArchive $git $repository $sourceCommit $fixedZip $Root $logs
        $fixedSnapshot = Join-Path $Root 'fixed-snapshot'
        [IO.Compression.ZipFile]::ExtractToDirectory($fixedZip, $fixedSnapshot)
        Assert-True (@(Get-ChildItem -LiteralPath $fixedSnapshot -File -Recurse).Count -eq $expected.Count) 'archive has only committed fixture files'
        foreach ($relative in $expected.Keys) {
            Assert-True ((Get-FileHash -LiteralPath (Join-Path $fixedSnapshot $relative)).Hash -ceq $expected[$relative]) "byte-exact archived fixture: $relative"
        }
        Assert-DistInstallerSources $fixedSnapshot
        Assert-True ((Get-FileHash -LiteralPath $configPath).Hash -ceq $configBefore) 'archive commands leave disposable repository config unchanged'
    } finally { Set-Item Function:New-DistProcessInfo -Value $originalInfo }
}

function New-TestPe {
    param([string]$Path, [string]$Dll = 'kernel32.dll', [switch]$Delay,
        [switch]$Unterminated, [int]$Subsystem = 2, [int]$Machine = 0x8664, [switch]$DllImage,
        [switch]$BothImports)
    $bytes = [byte[]]::new(1536)
    $bytes[0] = 0x4d; $bytes[1] = 0x5a
    [BitConverter]::GetBytes([uint32]0x80).CopyTo($bytes, 0x3c)
    [BitConverter]::GetBytes([uint32]0x4550).CopyTo($bytes, 0x80)
    [BitConverter]::GetBytes([uint16]$Machine).CopyTo($bytes, 0x84)
    [BitConverter]::GetBytes([uint16]1).CopyTo($bytes, 0x86)
    [BitConverter]::GetBytes([uint16]240).CopyTo($bytes, 0x94)
    if ($DllImage) { [BitConverter]::GetBytes([uint16]0x2000).CopyTo($bytes, 0x96) }
    [BitConverter]::GetBytes([uint16]0x20b).CopyTo($bytes, 0x98)
    [BitConverter]::GetBytes([uint64]0x140000000).CopyTo($bytes, 0xb0)
    [BitConverter]::GetBytes([uint16]$Subsystem).CopyTo($bytes, 0xdc)
    [BitConverter]::GetBytes([uint32]16).CopyTo($bytes, 0x104)
    # One backed section: virtual 0x1000..0x1400, raw 0x200..0x600.
    [BitConverter]::GetBytes([uint32]0x400).CopyTo($bytes, 0x190)
    [BitConverter]::GetBytes([uint32]0x1000).CopyTo($bytes, 0x194)
    [BitConverter]::GetBytes([uint32]0x400).CopyTo($bytes, 0x198)
    [BitConverter]::GetBytes([uint32]0x200).CopyTo($bytes, 0x19c)
    $directory = if ($Delay) { 13 } else { 1 }
    $recordSize = if ($Delay) { 32 } else { 20 }
    [BitConverter]::GetBytes([uint32]0x1000).CopyTo($bytes, (0x108 + $directory * 8))
    $size = if ($Unterminated) { $recordSize } else { $recordSize * 2 }
    [BitConverter]::GetBytes([uint32]$size).CopyTo($bytes, (0x10c + $directory * 8))
    if ($Delay) {
        [BitConverter]::GetBytes([uint32]1).CopyTo($bytes, 0x200)
        [BitConverter]::GetBytes([uint32]0x1080).CopyTo($bytes, 0x204)
    } else {
        [BitConverter]::GetBytes([uint32]0x1080).CopyTo($bytes, 0x20c)
    }
    [Text.Encoding]::ASCII.GetBytes($Dll + [char]0).CopyTo($bytes, 0x280)
    if ($BothImports) {
        if ($Delay -or $Unterminated) { throw 'BothImports fixture requires the normal terminated table.' }
        [BitConverter]::GetBytes([uint32]0x1100).CopyTo($bytes, (0x108 + 13 * 8))
        [BitConverter]::GetBytes([uint32]64).CopyTo($bytes, (0x10c + 13 * 8))
        [BitConverter]::GetBytes([uint32]1).CopyTo($bytes, 0x300)
        [BitConverter]::GetBytes([uint32]0x1180).CopyTo($bytes, 0x304)
        [Text.Encoding]::ASCII.GetBytes($Dll + [char]0).CopyTo($bytes, 0x380)
    }
    [IO.File]::WriteAllBytes($Path, $bytes)
}

$parent = 'E:/Development/_cache/cargo-target'
if (-not (Test-Path -LiteralPath $parent -PathType Container)) { throw "Missing cache parent: $parent" }
$token = [Guid]::NewGuid().ToString('N')
$root = New-DistOwnedDirectory $parent "kwil-dist-tests-$token" $token
$passed = $false
$script:DistUnreapedProcess = $false
Write-Host "Fixture reader PID=$PID; real Git opt-in=$RunGitArchiveFixture; isolated root=$root"
try {
    Test-DistArchiveBoundary $root
    if ($RunGitArchiveFixture) { Test-DistRealGitArchive $root }
    if ($ArchiveOnly) {
        $passed = $true
        Write-Host 'PASS: focused archive regression; real Git ran only if explicitly selected.'
        return
    }
    $commit = '0123456789abcdef0123456789abcdef01234567'
    $target = 'x86_64-pc-windows-msvc'
    $names = Get-DistNames '0.1.0' $commit $target
    Assert-True ($names.Msi -ceq 'kwil-0.1.0-0123456789ab-x86_64-pc-windows-msvc.msi') 'version, commit and target MSI name'
    Assert-True ($names.Zip -ceq 'kwil-0.1.0-0123456789ab-x86_64-pc-windows-msvc-portable.zip') 'portable name'
    Assert-Throws { Get-DistNames '0.1.0' '../bad' $target } '40-hex'
    Assert-Throws { Get-DistNames '../bad' $commit $target } 'Cargo version'
    Assert-Throws { Assert-DistContainedPath $root (Join-Path $root '../escape') } 'not strictly below'
    Assert-Throws { Assert-DistContainedPath $root $root } 'not strictly below'

    $snapshot = Join-Path $root 'snapshot with spaces & (fixture)'
    $bins = Join-Path $root 'build with spaces'
    foreach ($path in $snapshot, $bins, (Join-Path $snapshot 'assets/logo'), (Join-Path $snapshot 'crates/core-syntax')) {
        $null = New-Item -ItemType Directory -Path $path
    }
    foreach ($relative in 'LICENSE', 'assets/logo/kwil.ico', 'crates/core-syntax/LICENSES.md', 'crates/core-syntax/LICENSES.txt') {
        [IO.File]::WriteAllText((Join-Path $snapshot $relative), "fixture $relative")
    }
    # A profile in source must NEVER enter either payload.
    [IO.File]::WriteAllText((Join-Path $snapshot 'settings.toml'), 'fixture user data')
    New-TestPe (Join-Path $bins 'kwil.exe')
    New-TestPe (Join-Path $bins 'kwil-console.exe') -Subsystem 3
    $msiPayload = Join-Path $root 'msi'
    $portable = Join-Path $root 'portable'
    $null = New-Item -ItemType Directory -Path $msiPayload
    $null = New-Item -ItemType Directory -Path $portable
    Copy-DistPayload $snapshot $bins $msiPayload
    Copy-DistPayload $snapshot $bins $portable -Portable
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $msiPayload 'kwil.portable'))) 'no MSI portable marker'
    Assert-True (Test-Path -LiteralPath (Join-Path $portable 'kwil.portable')) 'portable marker'
    foreach ($payload in $msiPayload, $portable) {
        Assert-True (Test-Path -LiteralPath (Join-Path $payload 'kwil.no-update')) 'manager update suppression'
        Assert-True (Test-Path -LiteralPath (Join-Path $payload 'kwil.com')) 'complete console pair'
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $payload 'kwil-console.exe'))) 'console rename'
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $payload 'settings.toml'))) 'source profile excluded'
        Assert-True ((Get-FileHash -LiteralPath (Join-Path $payload 'kwil.com')).Hash -ceq
            (Get-FileHash -LiteralPath (Join-Path $bins 'kwil-console.exe')).Hash) 'console copied byte exactly'
    }
    Assert-Throws { Copy-DistPayload $snapshot $bins $msiPayload } 'not empty'
    $peInfo = Get-DistPeInfo (Join-Path $bins 'kwil.exe')
    Assert-True ($peInfo.Subsystem -eq 2 -and $peInfo.Imports[0] -ceq 'kernel32.dll') 'normal system import'
    $pePath = Join-Path $root 'pe-fixture.bin'
    New-TestPe $pePath -Delay -Machine 0xaa64 -Subsystem 3
    Assert-True ((Get-DistPeInfo $pePath).Machine -eq 0xaa64) 'ARM64 delay system import'
    foreach ($dll in 'VCRUNTIME140.dll', 'msvcp140.dll', 'msvcr120.dll', 'ucrtbase.dll', 'api-ms-win-crt-stdio-l1-1-0.dll') {
        New-TestPe $pePath -Dll $dll
        Assert-Throws { Get-DistPeInfo $pePath } 'Dynamic runtime import'
        New-TestPe $pePath -Dll $dll -Delay
        Assert-Throws { Get-DistPeInfo $pePath } 'Dynamic runtime import'
    }
    New-TestPe $pePath -Unterminated
    Assert-Throws { Get-DistPeInfo $pePath } 'Unterminated import directory'
    New-TestPe $pePath
    $broken = [IO.File]::ReadAllBytes($pePath)
    [BitConverter]::GetBytes([uint32]0x9000).CopyTo($broken, 0x20c)
    [IO.File]::WriteAllBytes($pePath, $broken)
    Assert-Throws { Get-DistPeInfo $pePath } 'outside backed section'

    $checks = Join-Path $root 'checks'
    $null = New-Item -ItemType Directory -Path $checks
    [IO.File]::WriteAllText((Join-Path $checks 'a file.txt'), 'abc', [Text.UTF8Encoding]::new($false))
    Write-DistChecksums $checks @('a file.txt')
    Assert-True ([IO.File]::ReadAllText((Join-Path $checks 'SHA256SUMS')) -ceq
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  a file.txt`n") 'known SHA256 and exact two-space format'
    Assert-Throws { Write-DistChecksums $checks @('a file.txt') } 'existing checksum'
    Assert-Throws { Write-DistChecksums $checks @('../escape') } 'Unsafe checksum'

    $stage = New-DistOwnedDirectory $root 'stage' $token
    $destination = Join-Path $root 'published'
    [IO.File]::WriteAllText((Join-Path $stage 'artifact.txt'), 'fixture')
    [IO.File]::WriteAllText((Join-Path $stage 'unknown.txt'), 'unknown fixture')
    Assert-Throws { Publish-DistDirectory $root $stage $destination $token @('artifact.txt') } 'unknown artifacts'
    # Remove exactly this newly created fixture file, never unrelated output.
    [IO.File]::Delete((Join-Path $stage 'unknown.txt'))
    $null = New-Item -ItemType Directory -Path $destination
    [IO.File]::WriteAllText((Join-Path $destination 'user.txt'), 'keep me')
    Assert-Throws { Publish-DistDirectory $root $stage $destination $token @('artifact.txt') } 'existing output'
    Assert-True ([IO.File]::ReadAllText((Join-Path $destination 'user.txt')) -ceq 'keep me') 'existing user data preserved'
    $fresh = Join-Path $root 'fresh'
    Publish-DistDirectory $root $stage $fresh $token @('artifact.txt')
    Assert-True ((Test-Path -LiteralPath (Join-Path $fresh 'artifact.txt')) -and -not (Test-Path -LiteralPath $stage)) 'directory publication'
    Assert-Throws { Remove-DistOwnedDirectory $root $fresh 'wrong-owner' } 'Ownership mismatch'
    Assert-Throws { New-DistOwnedDirectory $root 'fresh' $token } 'existing directory'
    $lockPath = Join-Path $root 'reservation.lock'
    $lock = New-DistReservation $lockPath $token
    try { Assert-Throws { New-DistReservation $lockPath 'other-owner' } '.' }
    finally { Close-DistReservation $lock $lockPath $token }
    $cacheFixture = Join-Path $root 'cache'
    Initialize-DistCache $cacheFixture
    Initialize-DistCache $cacheFixture
    Assert-Throws { Initialize-DistCache $destination } 'unowned existing Cargo cache'

    $arguments = @('a path with spaces', 'quote"inside', '& calc.exe', 'trailing\', '')
    $info = New-DistProcessInfo 'C:/fixture path/pwsh.exe' $arguments $snapshot
    Assert-True ($info.ArgumentList.Count -eq $arguments.Count) 'argument cardinality'
    for ($i = 0; $i -lt $arguments.Count; $i++) { Assert-True ($info.ArgumentList[$i] -ceq $arguments[$i]) "literal argument $i" }
    Assert-True ($info.CreateNoWindow -and -not $info.UseShellExecute -and $info.RedirectStandardOutput) 'hidden process with captured output'
    $wrapper = [IO.File]::ReadAllText((Join-Path $PSScriptRoot '../dist.bat'))
    Assert-True ($wrapper.Contains('-File "%~dp0scripts\dist.ps1" %*')) 'quoted wrapper root and argument forwarding'
    Assert-True ($wrapper.Contains('exit /b %kwil_dist_exit%')) 'wrapper exit propagation'

    # Replace ONLY the process boundary: verify build invocation and environment
    # without spawning a process or manufacturing a successful release pipeline.
    $originalProcess = (Get-Item Function:Invoke-DistProcess).ScriptBlock
    $script:mockCalls = [Collections.Generic.List[object]]::new()
    $before = @{}
    try {
        function Invoke-DistProcess {
            param($FilePath, $Arguments, $WorkingDirectory, $TimeoutSeconds, $LogDirectory, $Label)
            $script:mockCalls.Add([pscustomobject]@{ File = $FilePath; Args = @($Arguments); Cwd = $WorkingDirectory; Timeout = $TimeoutSeconds; Label = $Label })
            Assert-True ($env:KWIL_BUILD_COMMIT -ceq $commit) 'exact build commit environment'
            Assert-True ($env:RUSTUP_AUTO_INSTALL -ceq '0' -and $env:CARGO_NET_OFFLINE -ceq 'true') 'noninstalling offline build environment retained'
            Assert-True ($env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -ceq '-C target-feature=+crt-static') 'exact static CRT flag'
            Assert-True ($env:CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS -ceq '-C target-feature=+crt-static') 'both release target flags'
            Assert-True ([object]::ReferenceEquals([Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS', 'Process'), $null)) 'encoded flags are absent, not present empty'
            throw 'mocked build failure'
        }
        $keys = @('KWIL_BUILD_COMMIT', 'CARGO_TARGET_DIR', 'CARGO_NET_OFFLINE', 'CARGO_INCREMENTAL', 'CARGO_TERM_COLOR',
            'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS', 'CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS',
            'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'CARGO_BUILD_TARGET',
            'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTUP_AUTO_INSTALL')
        foreach ($key in $keys) {
            $original = [Environment]::GetEnvironmentVariable($key, 'Process')
            $before[$key] = [pscustomobject]@{ Exists = -not [object]::ReferenceEquals($original, $null); Value = $original }
        }
        Assert-Throws {
            Use-DistBuildEnvironment $commit $bins { Build-DistSnapshot 'rustup fixture.exe' '1.99.1' $target $snapshot $root 7 }
        } 'mocked build failure'
        foreach ($key in $keys) {
            Assert-TestProcessEnvironment $key $before[$key] "restore $key after failure"
        }
        Assert-True ($script:mockCalls.Count -eq 1) 'no retry after failed build'
        $call = $script:mockCalls[0]
        $expected = @('run', '1.99.1', 'cargo', 'build', '--release', '-p', 'kwil',
            '--features', 'windows-console', '--target', $target, '--locked', '--offline', '-j', '4')
        Assert-True (($call.Args -join '|') -ceq ($expected -join '|')) 'concrete offline locked release build command'
        Assert-TestNonInstallingRustupArguments $call.Args
        Assert-True ($call.Cwd -ceq $snapshot -and $call.Timeout -eq 7) 'snapshot cwd and finite timeout'
        # Exercise exact restoration on both callback exits, independently of
        # the inherited environment. None of these callbacks starts a process.
        $cases = @(
            [pscustomobject]@{ Label = 'absent'; Exists = $false; Value = $null },
            [pscustomobject]@{ Label = 'empty'; Exists = $true; Value = '' },
            [pscustomobject]@{ Label = 'nonempty'; Exists = $true; Value = 'Fixture-Original = "quoted" & (space)' }
        )
        foreach ($case in $cases) {
            foreach ($key in @('KWIL_BUILD_COMMIT', 'CARGO_ENCODED_RUSTFLAGS')) {
                if ($case.Exists) {
                    [Environment]::SetEnvironmentVariable($key, $case.Value, 'Process')
                } else {
                    [Environment]::SetEnvironmentVariable($key, [System.Management.Automation.Language.NullString]::Value, 'Process')
                }
                Assert-TestProcessEnvironment $key $case "fixture setup $($case.Label) $key"
            }
            $during = {
                Assert-True ($env:KWIL_BUILD_COMMIT -ceq $commit) 'commit is overridden during callback'
                Assert-True ([object]::ReferenceEquals([Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS', 'Process'), $null)) 'encoded flags removed during callback'
            }
            Use-DistBuildEnvironment $commit $bins $during
            foreach ($key in @('KWIL_BUILD_COMMIT', 'CARGO_ENCODED_RUSTFLAGS')) {
                Assert-TestProcessEnvironment $key $case "restore $($case.Label) $key after success"
            }
            Assert-Throws {
                Use-DistBuildEnvironment $commit $bins { & $during; throw 'fixture callback failure' }
            } 'fixture callback failure'
            foreach ($key in @('KWIL_BUILD_COMMIT', 'CARGO_ENCODED_RUSTFLAGS')) {
                Assert-TestProcessEnvironment $key $case "restore $($case.Label) $key after failure"
            }
        }
    } finally {
        try { Set-Item Function:Invoke-DistProcess -Value $originalProcess }
        finally {
            # Restore the fixture process itself even when a regression assertion
            # fails. Use explicit native-null binding independently of the driver.
            $fixtureRestoreErrors = [Collections.Generic.List[string]]::new()
            foreach ($key in $before.Keys) {
                try {
                    if ($before[$key].Exists) {
                        [Environment]::SetEnvironmentVariable($key, $before[$key].Value, 'Process')
                    } else {
                        [Environment]::SetEnvironmentVariable($key, [System.Management.Automation.Language.NullString]::Value, 'Process')
                    }
                } catch { $fixtureRestoreErrors.Add($key) }
            }
            if ($fixtureRestoreErrors.Count -ne 0) { throw "Fixture process environment restoration failed for: $($fixtureRestoreErrors -join ', ')" }
        }
    }
    # Frozen schema-1 extension fixtures. Authored source only until ROOT runs
    # this file. The MSI bytes below are deliberately fake; no MSI API is called.
    foreach ($version in '0.0.0', '255.255.65535') { Assert-DistInstallerVersion $version }
    foreach ($version in '01.0.0', '0.01.0', '0.0.01', '256.0.0', '0.256.0', '0.0.65536', '1.2.3-beta', '1.2.3+meta') {
        Assert-Throws { Assert-DistInstallerVersion $version } 'MSI'
    }
    Assert-True ((Get-DistUuidV5 '{6BA7B810-9DAD-11D1-80B4-00C04FD430C8}' ([Text.Encoding]::ASCII.GetBytes('www.widgets.com'))) -ceq
        '{21F7F8DE-8051-5B89-8680-0195EF798B6A}') 'RFC UUIDv5 network-order vector'
    $link = Get-DistStartMenuLink
    Assert-True ($link.Length -eq 868 -and [BitConverter]::ToUInt32($link, 0) -eq 76) 'exact shell-link length/header'
    Assert-True ([Convert]::ToHexString($link[4..19]) -ceq '0114020000000000C000000000000046') 'shell-link Windows GUID storage order'
    Assert-True ([BitConverter]::ToUInt32($link, 20) -eq 0x02000200 -and [BitConverter]::ToUInt32($link, 60) -eq 1) 'environment target flags and GUI show mode'
    Assert-True ([BitConverter]::ToUInt32($link, 76) -eq 788 -and [BitConverter]::ToUInt32($link, 80) -eq 2684354561) 'environment block size/signature'
    $linkTarget = '%LOCALAPPDATA%\Programs\Kwil\kwil.exe'
    Assert-True ([Text.Encoding]::ASCII.GetString($link, 84, 260).TrimEnd([char]0) -ceq $linkTarget) 'fixed ANSI environment target'
    Assert-True ([Text.Encoding]::Unicode.GetString($link, 344, 520).TrimEnd([char]0) -ceq $linkTarget) 'fixed Unicode environment target'
    foreach ($range in @(@(24, 59), @(64, 75), @((84 + $linkTarget.Length), 343), @((344 + 2 * $linkTarget.Length), 867))) {
        Assert-True (@($link[$range[0]..$range[1]] | Where-Object { $_ -ne 0 }).Count -eq 0) 'zero reserved fields, padding and terminal block'
    }
    $guardArgs = @(Get-DistGuardBuildArguments '1.99.1' $target 'E:/fixture guard/Cargo.toml')
    Assert-True (($guardArgs -join '|') -ceq ('run|1.99.1|cargo|build|--release|--lib|--manifest-path|E:/fixture guard/Cargo.toml|--target|' +
        $target + '|--locked|--offline|-j|4|--config|profile.release.panic="abort"')) 'isolated locked offline static-environment guard build arguments'
    Assert-TestNonInstallingRustupArguments $guardArgs
    $rustcArgs = @(Get-DistRustupRunArguments '1.99.1' @('rustc', '-vV'))
    Assert-TestNonInstallingRustupArguments $rustcArgs
    Assert-True (($rustcArgs -join '|') -ceq 'run|1.99.1|rustc|-vV') 'rustc version uses the installed CLI grammar without toolchain installation'
    $metadataArgs = @(Get-DistRustupRunArguments '1.99.1' @('cargo', 'metadata', '--no-deps', '--format-version', '1', '--locked', '--offline'))
    Assert-TestNonInstallingRustupArguments $metadataArgs
    Assert-True (($metadataArgs -join '|') -ceq 'run|1.99.1|cargo|metadata|--no-deps|--format-version|1|--locked|--offline') 'application metadata preserves locked offline arguments'
    Assert-True ((Get-DistFileSha256 (Join-Path $checks 'a file.txt')) -ceq 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad') 'streamed SHA256 known vector'
    Assert-Throws { Get-DistFileSha256 (Join-Path $checks 'a file.txt') 2 } 'byte limit'

    $sourceFixture = Join-Path $root 'installer-source'
    $null = New-Item -ItemType Directory -Path $sourceFixture
    Assert-Throws { Assert-DistInstallerSources $sourceFixture } 'windows-msi.ps1'
    foreach ($directory in 'scripts', 'docs/decisions', 'crates/installer-guard/src') {
        $null = New-Item -ItemType Directory -Path (Join-Path $sourceFixture $directory)
    }
    [IO.File]::WriteAllText((Join-Path $sourceFixture 'scripts/windows-msi.ps1'), '# fake backend source; never invoked')
    Assert-Throws { Assert-DistInstallerSources $sourceFixture } 'windows-installer-contract.json'
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'windows-installer-contract.json') -Destination (Join-Path $sourceFixture 'scripts/windows-installer-contract.json')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../docs/decisions/0004-windows-installer.md') -Destination (Join-Path $sourceFixture 'docs/decisions/0004-windows-installer.md')
    Assert-Throws { Assert-DistInstallerSources $sourceFixture } 'installer-guard/Cargo.toml'
    [IO.File]::WriteAllText((Join-Path $sourceFixture 'crates/installer-guard/Cargo.toml'), '# fake manifest; never passed to Cargo')
    Assert-Throws { Assert-DistInstallerSources $sourceFixture } 'installer-guard/src/lib.rs'
    [IO.File]::WriteAllText((Join-Path $sourceFixture 'crates/installer-guard/src/lib.rs'), '// fake guard source; never compiled')
    Assert-DistInstallerSources $sourceFixture
    $guardMetadataPath = Join-Path $root 'fake-guard-cargo-metadata.json'
    $guardManifest = Join-Path $sourceFixture 'crates/installer-guard/Cargo.toml'
    [IO.File]::WriteAllText($guardMetadataPath, (@{ packages = @(@{ manifest_path = $guardManifest;
        targets = @(@{ name = 'kwil_installer_guard'; crate_types = @('cdylib') }) }) } | ConvertTo-Json -Depth 8))
    $originalProcess = (Get-Item Function:Invoke-DistProcess).ScriptBlock
    $script:guardCalls = [Collections.Generic.List[object]]::new()
    try {
        function Invoke-DistProcess {
            param($FilePath, $Arguments, $WorkingDirectory, $TimeoutSeconds, $LogDirectory, $Label)
            $script:guardCalls.Add([pscustomobject]@{ Args = @($Arguments); Cwd = $WorkingDirectory; Timeout = $TimeoutSeconds; Label = $Label })
            Assert-True ($env:CARGO_TARGET_DIR -ceq $bins -and $env:CARGO_NET_OFFLINE -ceq 'true') 'guard gets its isolated offline cache'
            Assert-True ($env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -ceq '-C target-feature=+crt-static' -and
                $env:CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS -ceq '-C target-feature=+crt-static') 'guard uses the preserved static environment'
            if ($Label -ceq 'guard-metadata') { return [IO.FileInfo]::new($guardMetadataPath) }
            if ($Label -cne 'guard-build') { throw 'Unexpected process boundary in guard fixture.' }
        }
        $guardResult = Use-DistBuildEnvironment $commit $bins {
            Build-DistGuardSnapshot 'rustup fixture.exe' '1.99.1' $target $sourceFixture $bins $root 7
        }
        Assert-True ($script:guardCalls.Count -eq 2 -and $script:guardCalls[0].Label -ceq 'guard-metadata' -and
            $script:guardCalls[1].Label -ceq 'guard-build' -and $script:guardCalls[1].Timeout -eq 7 -and
            $script:guardCalls[1].Cwd -ceq $sourceFixture) 'guard-only manifest metadata/build boundary and timeout'
        foreach ($guardCall in $script:guardCalls) { Assert-TestNonInstallingRustupArguments $guardCall.Args }
        Assert-True ($guardResult -ceq (Join-Path $bins "$target/release/kwil_installer_guard.dll")) 'DLL target discovered from manifest metadata'
        Assert-True (-not (Test-Path -LiteralPath $guardResult)) 'mock did not build a guard or an application'
    } finally { Set-Item Function:Invoke-DistProcess -Value $originalProcess }
    $r2 = [IO.File]::ReadAllText((Join-Path $sourceFixture 'scripts/windows-installer-contract.json')) | ConvertFrom-Json -AsHashtable
    Assert-True ('DISABLEROLLBACK' -cin $r2.properties.mustBeEmpty -and 'RollbackDisabled' -cin $r2.properties.mustBeEmpty) 'R2 requires both effective rollback flags empty'
    Assert-True ($r2.properties.mutableOperationAllowlist.REINSTALLMODE -ceq 'empty or omus when REINSTALL is empty (install/uninstall); exactly amus for explicit REINSTALL=ALL repair') 'R2 normal omus does not grant repair authority'
    $r2Decision = [IO.File]::ReadAllText((Join-Path $sourceFixture 'docs/decisions/0004-windows-installer.md'))
    Assert-True ($r2Decision.Contains('NOT RollbackDisabled AND NOT DISABLEROLLBACK') -and $r2Decision.Contains('Author no DisableRollback action')) 'R2 backend launch condition and independent guard rule'
    Assert-True ($r2.packageAuthoring.filename.Contains('Directory.DefaultDir must match the exact decision rows') -and $r2Decision.Contains('including INSTALLDIR=Kwil')) 'R2 exact directory rows remain distinct from File short|long names'
    Assert-True ($r2Decision.Contains('Timeout cleanup may terminate') -and $r2Decision.Contains('The runtime guard never terminates applications.')) 'R2 packaging child cleanup does not authorize runtime app termination'
    [IO.File]::AppendAllText((Join-Path $sourceFixture 'scripts/windows-installer-contract.json'), ' ')
    Assert-Throws { Assert-DistInstallerSources $sourceFixture } 'Frozen installer contract mismatch'

    $guardFixture = Join-Path $root 'synthetic-guard.dll'
    New-TestPe $guardFixture -DllImage -BothImports
    $guardPe = Get-DistPeInfo $guardFixture 16777216
    Assert-True ($guardPe.IsDll -and $guardPe.GuardImports.Count -eq 2 -and
        $guardPe.GuardImports[0].load -ceq 'D' -and $guardPe.GuardImports[1].load -ceq 'N' -and
        $guardPe.GuardImports[0].dll -ceq 'kernel32.dll' -and $guardPe.GuardImports[1].dll -ceq 'kernel32.dll') 'same DLL retained separately in normal/delay evidence and sorted ordinal'
    Assert-Throws { Get-DistPeInfo $guardFixture 100 } 'PE size limit'
    $inventory = @(Get-DistInstallerInventory $msiPayload 'x64')
    $ownerRow = @($inventory | Where-Object { $_.kind -ceq 'R' -and $_.name -ceq 'n:Owner' })[0]
    $ownerProbe = [ordered]@{}
    foreach ($field in $ownerRow.Keys) { $ownerProbe[$field] = $ownerRow[$field] }
    Assert-DistInstallerRegistryData $ownerProbe $inventory 'x64'
    foreach ($invalidOwner in @($ownerRow.data.ToLowerInvariant(), ($ownerRow.data + '{}'), ('${I}' + $ownerRow.data), $ownerRow.data.Replace('{', '').Replace('}', ''))) {
        $ownerProbe.data = $invalidOwner
        Assert-Throws { Assert-DistInstallerRegistryData $ownerProbe $inventory 'x64' } 'File Owner data mismatch'
    }
    $shellProbe = [ordered]@{}
    $shellRow = @($inventory | Where-Object { $_.kind -ceq 'R' -and $_.name -ceq 'n:Icon' })[0]
    foreach ($field in $shellRow.Keys) { $shellProbe[$field] = $shellRow[$field] }
    $shellProbe.data = $ownerRow.data
    Assert-Throws { Assert-DistInstallerRegistryData $shellProbe $inventory 'x64' } 'Shell registry data mismatch'
    Assert-True ($inventory.Count -eq 29 -and @($inventory | Where-Object { $_.kind -ceq 'F' }).Count -eq 8 -and
        @($inventory | Where-Object { $_.kind -ceq 'R' }).Count -eq 11 -and @($inventory | Where-Object { $_.kind -ceq 'K' }).Count -eq 10) 'exact schema-1 F/R/K counts'
    Assert-True (@($inventory.component | Select-Object -Unique).Count -eq 11) 'eight file and three shell components'
    $previousIdentity = ''
    for ($index = 0; $index -lt $inventory.Count; $index++) {
        $row = $inventory[$index]
        Assert-True ($row.id -ceq ('r' + ($index + 1).ToString('D6'))) 'canonical resource IDs'
        $identityText = Get-DistResourceIdentity $row
        Assert-True ([StringComparer]::Ordinal.Compare($previousIdentity, $identityText) -lt 0) 'ASCII ordinal identity order'
        $previousIdentity = $identityText
        Assert-True (@($row.Keys).Count -eq 13 -and @($row.Values | Where-Object { $_ -isnot [string] }).Count -eq 0) 'exact thirteen string fields'
    }
    $shellCommand = @($inventory | Where-Object { $_.kind -ceq 'R' -and $_.relative.EndsWith('\command') })[0]
    Assert-True ($shellCommand.data -ceq '"${I}kwil.exe" "%1"' -and $shellCommand.name -ceq '@') 'exact quoted Explorer command without extra arguments'
    $lnkRow = @($inventory | Where-Object { $_.root -ceq 'P' })[0]
    Assert-True ($lnkRow.relative -ceq 'Kwil.lnk' -and $lnkRow.source -ceq 'g' -and $lnkRow.size -ceq '868' -and
        $lnkRow.sha256 -ceq (Get-DistBytesHash $link)) 'generated shortcut inventory'
    foreach ($keyRow in @($inventory | Where-Object { $_.kind -ceq 'K' })) {
        $direct = @($inventory | Where-Object { $_.kind -ceq 'R' -and $_.relative -ceq $keyRow.relative })
        Assert-True ($direct.Count -gt 0 -and $keyRow.component -ceq $direct[0].component) 'key component belongs to first direct registry value'
    }
    $inventoryBytes = Get-DistInventoryBytes $inventory
    Assert-True ([Text.Encoding]::ASCII.GetString($inventoryBytes, 0, 4) -ceq 'KWR1' -and
        [BitConverter]::ToUInt32($inventoryBytes, 4) -eq 29 -and [BitConverter]::ToUInt32($inventoryBytes, 8) -eq 7) 'KWR1 header and LP length, never JSON'
    $metadata = Initialize-DistInstallerInput $msiPayload $guardFixture 'x64' '0.1.0' $commit $target
    Assert-True ([IO.File]::ReadAllText((Join-Path $msiPayload 'kwil.no-update')) -ceq "Updates are managed by this distribution. Rebuild/reinstall explicitly.`n") 'exact no-update marker text and LF'
    Assert-True ((Get-Item -LiteralPath (Join-Path $portable 'kwil.portable')).Length -eq 0) 'empty portable marker'
    $inputPath = Join-Path $msiPayload '.installer/input.json'
    Assert-DistJsonEqual (Read-DistInstallerJson $inputPath) $metadata
    $inputRecords = @(Get-DistInstallerInputRecords $msiPayload)
    Assert-True ((Get-DistInstallerHashLimit 'kwil.exe' (1073741824L - 7)) -eq 7) 'installed hash allowance includes the generated 868-byte LNK'
    Assert-True ((Get-DistInstallerHashLimit '.installer/input.json' 1073741824L) -eq 1048576 -and
        (Get-DistInstallerHashLimit '.installer/kwil-installer-guard.dll' 1073741824L) -eq 16777216) 'metadata retains separate bounded allowances at the installed cap'
    $originalLengthReader = (Get-Item Function:Get-DistFileLength).ScriptBlock
    $originalHashReader = (Get-Item Function:Get-DistFileSha256).ScriptBlock
    $script:budgetHashCalls = [Collections.Generic.List[object]]::new()
    try {
        # Fake lengths only: no large file, allocation or heavy hash is created.
        function Get-DistFileLength {
            param([string]$Path)
            if ($Path.Contains('.installer')) { return [IO.FileInfo]::new($Path).Length }
            return 536870912L
        }
        function Get-DistFileSha256 {
            param([string]$Path, [long]$MaximumBytes)
            $script:budgetHashCalls.Add([pscustomobject]@{ Path = $Path; Limit = $MaximumBytes })
            return '0' * 64
        }
        Assert-Throws { Get-DistInstallerInputRecords $msiPayload } 'installed aggregate byte limit exceeded before hashing'
        Assert-True ($script:budgetHashCalls.Count -eq 1 -and $script:budgetHashCalls[0].Limit -eq 536870912) 'second 512 MiB installed file refused before its hash starts'
    } finally {
        Set-Item Function:Get-DistFileLength -Value $originalLengthReader
        Set-Item Function:Get-DistFileSha256 -Value $originalHashReader
    }
    Assert-True ($inputRecords.Count -eq 9) 'seven installed files plus immutable metadata and guard input'
    Assert-DistInstallerInputUnchanged $msiPayload $inputRecords
    $originalInputBytes = [IO.File]::ReadAllBytes($inputPath)
    [IO.File]::AppendAllText($inputPath, ' ')
    Assert-Throws { Assert-DistInstallerInputUnchanged $msiPayload $inputRecords } 'changed the input payload'
    [IO.File]::WriteAllBytes($inputPath, $originalInputBytes)
    $extraInput = Join-Path $msiPayload '.installer/extra.txt'
    [IO.File]::WriteAllText($extraInput, 'unexpected fixture')
    Assert-Throws { Assert-DistInstallerInputUnchanged $msiPayload $inputRecords } 'Unexpected installer metadata entry'
    [IO.File]::Delete($extraInput)
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $msiPayload '.installer/kwil-installer-guard.dll')).Hash -ceq
        (Get-FileHash -LiteralPath $guardFixture).Hash) 'staged DLL byte identity'
    Assert-Throws { Initialize-DistInstallerInput $msiPayload $guardFixture 'x64' '0.1.0' $commit $target } 'Unexpected MSI staging entry'
    $identity = Get-DistInstallerIdentity $metadata
    $armPayload = Join-Path $root 'arm64-msi'
    $null = New-Item -ItemType Directory -Path $armPayload
    Copy-DistPayload $snapshot $bins $armPayload
    Assert-Throws { Initialize-DistInstallerInput $armPayload (Join-Path $bins 'kwil.exe') 'x64' '0.1.0' $commit $target } 'DLL mismatch'
    $armGuard = Join-Path $root 'synthetic-arm64-guard.dll'
    New-TestPe $armGuard -DllImage -Machine 0xaa64 -Delay
    Assert-Throws { Initialize-DistInstallerInput $armPayload $armGuard 'x64' '0.1.0' $commit $target } 'architecture/DLL mismatch'
    $armMetadata = Initialize-DistInstallerInput $armPayload $armGuard 'arm64' '0.1.0' $commit 'aarch64-pc-windows-msvc'
    Assert-True ($armMetadata.architecture -ceq 'arm64' -and $armMetadata.guard.imports.Count -eq 1 -and $armMetadata.guard.imports[0].load -ceq 'D') 'ARM64 guard with explicit normal/delay inspection'
    Assert-True ((Get-DistInstallerIdentity $armMetadata).ProductCode -cne $identity.ProductCode -and
        $armMetadata.inventory[0].component -cne $metadata.inventory[0].component) 'architecture enters product and component identities'
    $reordered = $metadata | ConvertTo-Json -Depth 8 -Compress | ConvertFrom-Json -AsHashtable
    Assert-True ((Get-DistInstallerIdentity $reordered).BuildId -ceq $identity.BuildId) 'JSON whitespace does not enter BuildID'
    $reordered.commit = '1123456789abcdef0123456789abcdef01234567'
    Assert-True ((Get-DistInstallerIdentity $reordered).BuildId -cne $identity.BuildId) 'commit changes BuildID'
    $reordered.commit = $commit
    $reordered.inventory[0].sha256 = '0' * 64
    Assert-True ((Get-DistInstallerIdentity $reordered).BuildId -cne $identity.BuildId) 'inventory content changes BuildID'

    $fakeMsi = Join-Path $root 'synthetic.msi'
    [IO.File]::WriteAllText($fakeMsi, 'FAKE MSI - fixture only, never install')
    $sidecar = [ordered]@{ schema = 1; familyGuid = $metadata.familyGuid; componentSchema = 1; architecture = 'x64'; target = $target;
        version = '0.1.0'; commit = $commit; buildId = $identity.BuildId; inventorySha256 = $identity.InventorySha256;
        productCode = $identity.ProductCode; packageCode = '{12345678-1234-4234-8234-123456789ABC}'; guard = $metadata.guard; inventory = $metadata.inventory;
        msi = [ordered]@{ fileName = 'synthetic.msi'; size = (Get-Item -LiteralPath $fakeMsi).Length.ToString(); sha256 = (Get-FileHash -LiteralPath $fakeMsi).Hash.ToLowerInvariant() };
        inspection = [ordered]@{ database = 'passed'; cabinet = 'passed'; staticImports = 'passed' }; signed = $false; nativeLifecycle = 'not-run' }
    $sidecarPath = $fakeMsi + '.manifest.json'
    Assert-Throws { Assert-DistInstallerSidecar $fakeMsi $metadata } '.'
    [IO.File]::WriteAllText($sidecarPath, ($sidecar | ConvertTo-Json -Depth 8))
    $null = Assert-DistInstallerSidecar $fakeMsi $metadata
    $sidecar.inventory[0].size = '999'
    [IO.File]::WriteAllText($sidecarPath, ($sidecar | ConvertTo-Json -Depth 8))
    # Reconstruct independent input: mutating the sidecar object must not mutate authority.
    $freshMetadata = Read-DistInstallerJson $inputPath
    # ConvertFrom-Json returns Int64 for schema/ABI numbers. The R1 failure
    # reached this valid expected guard.abi before the deliberately changed size.
    Assert-True ($freshMetadata.guard.abi -is [long]) 'parsed guard ABI remains a JSON integer represented as Int64'
    Assert-DistJsonEqual (Read-DistInstallerJson $inputPath) $freshMetadata
    foreach ($expectedInteger in @([int]1, [long]1)) {
        foreach ($actualInteger in @([int]1, [long]1)) {
            Assert-DistJsonEqual $actualInteger $expectedInteger
        }
        foreach ($wrongInteger in @([long]2, '1', [double]1, [decimal]1, $true, $null)) {
            Assert-Throws { Assert-DistJsonEqual $wrongInteger $expectedInteger } 'Installer JSON integer mismatch'
        }
    }
    Assert-Throws { Assert-DistJsonEqual ([long]1) ([double]1) } 'Unsupported expected installer JSON type'
    Assert-Throws { Assert-DistInstallerSidecar $fakeMsi $freshMetadata } 'mismatch'
    $sidecar.inventory = $freshMetadata.inventory
    foreach ($field in 'buildId', 'inventorySha256', 'productCode', 'packageCode') {
        $saved = $sidecar[$field]; $sidecar[$field] = 'wrong'
        [IO.File]::WriteAllText($sidecarPath, ($sidecar | ConvertTo-Json -Depth 8))
        Assert-Throws { Assert-DistInstallerSidecar $fakeMsi $freshMetadata } 'mismatch|UUIDv4'
        $sidecar[$field] = $saved
    }
    $sidecar.extra = 'forbidden'
    [IO.File]::WriteAllText($sidecarPath, ($sidecar | ConvertTo-Json -Depth 8))
    Assert-Throws { Assert-DistInstallerSidecar $fakeMsi $freshMetadata } 'shape mismatch'
    $sidecar.Remove('extra')
    [IO.File]::WriteAllText($sidecarPath, ($sidecar | ConvertTo-Json -Depth 8))
    [IO.File]::AppendAllText($fakeMsi, 'changed')
    Assert-Throws { Assert-DistInstallerSidecar $fakeMsi $freshMetadata } 'mismatch'
    foreach ($json in '{"schema":1,"schema":1}', '{"schema":1.0}', '{"schema":1} trailing', '{"x":"\u0000"}', '{"x":true}') {
        [IO.File]::WriteAllText($sidecarPath, $json)
        Assert-Throws { Read-DistInstallerJson $sidecarPath } '.'
    }
    foreach ($overLimit in @(@{ name = 'inventory'; count = 513 }, @{ name = 'imports'; count = 129 })) {
        $json = '{"' + $overLimit.name + '":[' + ((@('1') * $overLimit.count) -join ',') + ']}'
        [IO.File]::WriteAllText($sidecarPath, $json)
        Assert-Throws { Read-DistInstallerJson $sidecarPath } 'row limit'
    }
    [IO.File]::WriteAllBytes($sidecarPath, [byte[]]@(239, 187, 191, 123, 125))
    Assert-Throws { Read-DistInstallerJson $sidecarPath } 'BOM'
    [IO.File]::WriteAllBytes($sidecarPath, [byte[]]@(123, 34, 120, 34, 58, 34, 255, 34, 125))
    Assert-Throws { Read-DistInstallerJson $sidecarPath } '.'
    $zipPath = Join-Path $root 'fixture-portable.zip'
    $portableReceipt = @(Get-DistPortableRecords $portable)
    $portableExe = Join-Path $portable 'kwil.exe'
    $portableOriginal = [IO.File]::ReadAllBytes($portableExe)
    $portableChanged = [byte[]]$portableOriginal.Clone()
    $portableChanged[$portableChanged.Length - 1] = $portableChanged[$portableChanged.Length - 1] -bxor 1
    try {
        [IO.File]::WriteAllBytes($portableExe, $portableChanged)
        $refusedZip = Join-Path $root 'changed-before-pack.zip'
        Assert-Throws { New-DistPortableZip $portable $refusedZip $portableReceipt } 'Portable receipt mismatch'
        Assert-True (-not (Test-Path -LiteralPath $refusedZip)) 'same-size source mutation refused before ZIP creation'
    } finally { [IO.File]::WriteAllBytes($portableExe, $portableOriginal) }
    New-DistPortableZip $portable $zipPath $portableReceipt
    foreach ($mutation in 'content', 'size', 'name', 'duplicate') {
        $changedZip = Join-Path $root "portable-$mutation.zip"
        Copy-Item -LiteralPath $zipPath -Destination $changedZip
        $archive = [IO.Compression.ZipFile]::Open($changedZip, [IO.Compression.ZipArchiveMode]::Update)
        try {
            $removedName = if ($mutation -ceq 'duplicate') { 'kwil.com' } else { 'kwil.exe' }
            $archive.GetEntry($removedName).Delete()
            $replacementName = if ($mutation -ceq 'name') { 'foreign.exe' } else { 'kwil.exe' }
            $replacement = $archive.CreateEntry($replacementName)
            $entryStream = $replacement.Open()
            try {
                $entryBytes = if ($mutation -ceq 'content') { $portableChanged } else { $portableOriginal }
                $entryStream.Write($entryBytes, 0, $entryBytes.Length)
                if ($mutation -ceq 'size') { $entryStream.WriteByte(0) }
            } finally { $entryStream.Dispose() }
        } finally { $archive.Dispose() }
        $pattern = switch ($mutation) { 'content' { 'entry content mismatch' }; 'size' { 'entry size mismatch' }; default { 'entry name mismatch' } }
        Assert-Throws { Assert-DistPortableZip $changedZip $portableReceipt } $pattern
    }
    $zip = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        Assert-True ($zip.Entries.Count -eq 8 -and @($zip.Entries | Where-Object { $_.FullName.StartsWith('.installer') }).Count -eq 0) 'portable ZIP has no installer input or DLL'
    } finally { $zip.Dispose() }
    $null = New-Item -ItemType Directory -Path (Join-Path $portable '.installer')
    Assert-Throws { New-DistPortableZip $portable (Join-Path $root 'must-not-exist.zip') $portableReceipt } 'installer is forbidden'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $root 'must-not-exist.zip'))) 'portable rejection happens before output'
    $publicationFixture = New-DistOwnedDirectory $root 'sidecar-publication' $token
    foreach ($name in 'fixture.msi', 'fixture.msi.manifest.json', 'fixture-portable.zip', 'INSTALL.txt', 'manifest.json') {
        [IO.File]::WriteAllText((Join-Path $publicationFixture $name), 'synthetic publication fixture')
    }
    Write-DistChecksums $publicationFixture @('fixture.msi', 'fixture.msi.manifest.json', 'fixture-portable.zip', 'INSTALL.txt', 'manifest.json')
    Assert-True ([IO.File]::ReadAllText((Join-Path $publicationFixture 'SHA256SUMS')).Contains('  fixture.msi.manifest.json' + "`n")) 'sidecar covered by outer checksums'
    Publish-DistDirectory $root $publicationFixture (Join-Path $root 'sidecar-published') $token @('fixture.msi', 'fixture.msi.manifest.json', 'fixture-portable.zip', 'INSTALL.txt', 'manifest.json', 'SHA256SUMS')
    # Exact Process doubles only: no OS process is created, looked up or killed.
    $previousOwnership = Get-Variable -Name DistUnreapedProcess -Scope Script -ErrorAction SilentlyContinue
    $previousOwnershipValue = if ($null -eq $previousOwnership) { $null } else { $previousOwnership.Value }
    $cacheReceiptPath = Join-Path $root 'unresolved-cache.lock'
    $outputReceiptPath = Join-Path $root 'unresolved-output.lock'
    $cacheReceipt = $null; $outputReceipt = $null
    try {
        $script:DistUnreapedProcess = $false
        $fakeProcess = [pscustomobject]@{ Id = 424242; HasExited = $false; ExitOnKill = $true; WaitResult = $true;
            KillCalls = 0; KillArgumentCounts = [Collections.Generic.List[int]]::new(); WaitMilliseconds = [Collections.Generic.List[int]]::new() }
        $fakeProcess | Add-Member -MemberType ScriptMethod -Name Kill -Value {
            $this.KillCalls++
            $this.KillArgumentCounts.Add($args.Count)
            $this.HasExited = $this.ExitOnKill
        }
        $fakeProcess | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
            param([int]$Milliseconds)
            $this.WaitMilliseconds.Add($Milliseconds)
            return $this.WaitResult
        }
        Stop-DistOwnedProcess $fakeProcess 424242 $root 'synthetic-no-process'
        Assert-True ($fakeProcess.KillCalls -eq 1 -and $fakeProcess.KillArgumentCounts[0] -eq 0 -and
            $fakeProcess.WaitMilliseconds[0] -eq 5000) 'termination uses only the owned Process with no tree argument and a bounded wait'
        Assert-True $script:DistUnreapedProcess 'exact parent exit does not prove descendant ownership resolved'
        $cacheReceipt = New-DistReservation $cacheReceiptPath $token
        $outputReceipt = New-DistReservation $outputReceiptPath $token
        Close-DistReservations $cacheReceipt $cacheReceiptPath $outputReceipt $outputReceiptPath $token
        Assert-True ((Test-Path -LiteralPath $cacheReceiptPath) -and (Test-Path -LiteralPath $outputReceiptPath)) 'unresolved ownership retains both reservation files'
        Assert-True ([IO.File]::ReadAllText($cacheReceiptPath) -ceq $token -and
            [IO.File]::ReadAllText($outputReceiptPath) -ceq $token) 'retained reservation identity is intact'
        $script:DistUnreapedProcess = $false
        Stop-DistOwnedProcess $fakeProcess 424242 $root 'synthetic-exited-parent'
        Assert-True ($fakeProcess.KillCalls -eq 1 -and $script:DistUnreapedProcess) 'already exited parent still leaves an abnormal boundary unresolved'
        $script:DistUnreapedProcess = $false
        Assert-Throws { Stop-DistOwnedProcess $fakeProcess 424243 $root 'synthetic-wrong-id' } 'identity mismatch'
        Assert-True ($fakeProcess.KillCalls -eq 1 -and $script:DistUnreapedProcess) 'PID mismatch never targets another process or releases ownership'
        $fakeProcess.HasExited = $false; $fakeProcess.ExitOnKill = $false; $fakeProcess.WaitResult = $false
        $script:DistUnreapedProcess = $false
        Assert-Throws { Stop-DistOwnedProcess $fakeProcess 424242 $root 'synthetic-still-running' } 'has not exited'
        Assert-True $script:DistUnreapedProcess 'failed exact-parent cleanup retains reservations'
        $script:DistUnreapedProcess = $false
        $normalCachePath = Join-Path $root 'normal-cache.lock'
        $normalOutputPath = Join-Path $root 'normal-output.lock'
        $normalCache = New-DistReservation $normalCachePath $token
        $normalOutput = New-DistReservation $normalOutputPath $token
        Close-DistReservations $normalCache $normalCachePath $normalOutput $normalOutputPath $token
        Assert-True (-not (Test-Path -LiteralPath $normalCachePath) -and -not (Test-Path -LiteralPath $normalOutputPath)) 'normally completed ownership releases only its own reservations'
    } finally {
        if ($null -ne $cacheReceipt) { $cacheReceipt.Dispose() }
        if ($null -ne $outputReceipt) { $outputReceipt.Dispose() }
        if ($null -eq $previousOwnership) { Remove-Variable -Name DistUnreapedProcess -Scope Script -ErrorAction SilentlyContinue }
        else { $script:DistUnreapedProcess = $previousOwnershipValue }
    }
    $passed = $true
    Write-Host 'PASS: tiny dist fixtures and mocked build boundary; no real build, MSI or app run. Real Git ran only if explicitly selected.'
} finally {
    if ($passed -and -not $RunGitArchiveFixture -and -not $script:DistUnreapedProcess) { Remove-DistOwnedDirectory $parent $root $token }
    else { Write-Warning "Fixture artifacts retained for ROOT inspection: $root" }
}
