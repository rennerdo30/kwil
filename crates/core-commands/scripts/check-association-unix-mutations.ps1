# Hosted Linux only. Never run against live association data: the selected Rust
# test uses private fixtures and bounds/kills/reaps only its own subprocesses.
param([string]$EvidenceRoot = [IO.Path]::GetTempPath())
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsLinux) { throw 'Association Unix mutation proof requires Linux; no Windows runtime inference.' }

$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$sourcePath = Join-Path $repo 'crates/core-commands/src/associations.rs'
$workerPath = Join-Path $repo 'crates/core-commands/src/associations/worker.rs'
$test = 'associations::worker::tests::linux_sync_rejects_raced_resources_in_bounded_subprocesses'
$runId = [guid]::NewGuid().ToString('N')
$proof = Join-Path ([IO.Path]::GetFullPath($EvidenceRoot)) "kwil-association-unix-$runId"
New-Item -ItemType Directory -Path $proof | Out-Null
$original = [IO.File]::ReadAllBytes($sourcePath)
$originalText = [Text.Encoding]::UTF8.GetString($original)
$originalHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
$workerHash = (Get-FileHash -LiteralPath $workerPath -Algorithm SHA256).Hash
$pins = @{}
foreach ($relative in @('Cargo.toml', 'Cargo.lock', 'crates/core-commands/Cargo.toml', '.cargo/config.toml', 'rust-toolchain.toml')) {
    $pins[$relative] = (Get-FileHash -LiteralPath (Join-Path $repo $relative) -Algorithm SHA256).Hash
}
$cases = @(
    @{ Name='omit-nonblock'; Old=' | libc::O_NONBLOCK'; New=''; Child='file-fifo' },
    @{ Name='omit-nofollow'; Old=' | libc::O_NOFOLLOW'; New=''; Child='file-symlink' }
)
# Deliberately excluded: omitting O_DIRECTORY alone still rejects a FIFO via
# descriptor type validation while O_NONBLOCK survives. The existing ENOTDIR
# assertion distinguishes admission paths, not independently unsafe behavior.
# Do not count it as a killed behavioral mutant or tune the test's error text.
$excluded = 'omit-directory: no independent behavioral failure; nonblocking descriptor validation still rejects it'

function Assert-Pins {
    if ((Get-FileHash -LiteralPath $workerPath -Algorithm SHA256).Hash -ne $workerHash) { throw 'Worker source changed during proof' }
    foreach ($relative in $pins.Keys) {
        if ((Get-FileHash -LiteralPath (Join-Path $repo $relative) -Algorithm SHA256).Hash -ne $pins[$relative]) {
            throw "Manifest/configuration changed during proof: $relative"
        }
    }
}

function Invoke-RecordedProcess([string]$Executable, [string[]]$Arguments, [string]$Label) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $repo
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    # Inherit target directory, RUSTC_WRAPPER/sccache and all existing CI env.
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $started = $false
    try {
        if (-not $process.Start()) { throw "Cannot start $Executable" }
        $started = $true
        $ownPid = $process.Id
        "label=$Label pid=$ownPid timeout=180s" | Add-Content -LiteralPath (Join-Path $proof 'pids.log')
        if ($Label -ne 'git-head') { Write-Host "START $Label PID=$ownPid timeout=180s" }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(180000)) {
            $process.Kill($true)
            $process.WaitForExit()
            throw "$Label exceeded 180s; only its own process tree was stopped"
        }
        $result = [pscustomobject]@{ Exit=$process.ExitCode; Out=$stdout.GetAwaiter().GetResult(); Err=$stderr.GetAwaiter().GetResult(); Pid=$ownPid }
        "label=$Label pid=$ownPid exit=$($result.Exit)" | Add-Content -LiteralPath (Join-Path $proof 'pids.log')
        return $result
    } finally {
        # Also reap on an exception during capture/validation. Never select by name.
        if ($started -and -not $process.HasExited) {
            $process.Kill($true)
            $process.WaitForExit()
            "label=$Label pid=$($process.Id) stopped-and-reaped" | Add-Content -LiteralPath (Join-Path $proof 'pids.log')
        }
        $process.Dispose()
    }
}

$git = Invoke-RecordedProcess 'git' @('rev-parse', 'HEAD') 'git-head'
if ($git.Exit -ne 0) { throw "Cannot identify commit: $($git.Err)" }
$head = $git.Out.Trim()
$header = "commit=$head source_sha256=$originalHash worker_sha256=$workerHash"
$pidLog = Join-Path $proof 'pids.log'
$pidText = [IO.File]::ReadAllText($pidLog)
[IO.File]::WriteAllText($pidLog, "$header`n$pidText")
$header | Set-Content -LiteralPath (Join-Path $proof 'summary.log')
Write-Host $header
"EXCLUDED $excluded" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append | Write-Host
[ordered]@{ commit=$head; source=$originalHash; worker=$workerHash; manifests=$pins; excluded=$excluded } |
    ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $proof 'inputs.json')

function Invoke-Phase([string]$Label, [byte[]]$Bytes, [string]$ExpectedChild = '') {
    Assert-Pins
    if ((Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash -ne $originalHash) { throw 'Source is not the original before phase' }
    # A unique comment changes compiler inputs without changing original behavior,
    # forcing provenance for this phase through Cargo and the inherited sccache.
    # No cache cleanup, target override, wrapper bypass or persistent env changes.
    $stamp = [Text.Encoding]::UTF8.GetBytes("`n// association proof $runId phase $Label`n")
    $phaseBytes = [byte[]]($Bytes + $stamp)
    $phaseHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($phaseBytes))
    $log = Join-Path $proof "$Label.log"
    "commit=$head source_sha256=$phaseHash original_sha256=$originalHash worker_sha256=$workerHash test=$test" |
        Set-Content -LiteralPath $log
    try {
        [IO.File]::WriteAllBytes($sourcePath, $phaseBytes)
        $start = [DateTime]::UtcNow
        $result = Invoke-RecordedProcess 'cargo' @('test', '--offline', '--locked', '-j', '4', '-p', 'kwil-core-commands', '--lib', $test, '--message-format=json', '-v', '--', '--exact', '--nocapture', '--test-threads=1') $Label
        $result.Err, $result.Out | Add-Content -LiteralPath $log
        # Strip only ANSI escapes for matching; retain the original log bytes/text.
        $text = [regex]::Replace(($result.Err + "`n" + $result.Out), "`e\[[0-9;]*m", '')
        if ($text -match 'error\[E\d+\]|could not compile|error: linking') { throw "$Label compiler failure is not mutation proof" }
        $artifacts = @(
            foreach ($line in ($result.Out -split "`n")) {
                if ($line.StartsWith('{')) {
                    $entry = $line | ConvertFrom-Json
                    if ($entry.reason -eq 'compiler-artifact' -and $entry.target.name -eq 'kwil_core_commands' -and $entry.profile.test -and $entry.executable) { $entry }
                }
            }
        )
        if ($artifacts.Count -ne 1 -or $artifacts[0].fresh -or $text -notmatch 'Running.*rustc.*--crate-name kwil_core_commands.*--test') {
            throw "$Label lacks actual test recompilation; stale/fresh sibling artifacts are not proof"
        }
        $exe = Get-Item -LiteralPath $artifacts[0].executable
        if ($exe.LastWriteTimeUtc -lt $start.AddSeconds(-2) -or -not $text.Contains($exe.FullName)) { throw "$Label test executable provenance mismatch" }
        $exeHash = (Get-FileHash -LiteralPath $exe.FullName -Algorithm SHA256).Hash
        if ($text -notmatch "(?m)^test $([regex]::Escape($test)) \.\.\. (ok|FAILED)" -or $text -notmatch 'running 1 test') {
            throw "$Label did not execute the exact selected regression"
        }
        if ($ExpectedChild) {
            if ($result.Exit -ne 101 -or $text -notmatch "association sync child $([regex]::Escape($ExpectedChild)) (failed|blocked)" -or
                $text -notmatch 'panicked at|assertion.*failed' -or $text -notmatch 'test result: FAILED\. 0 passed; 1 failed;') {
                throw "$Label did not fail the intended assertion/child case ($ExpectedChild)"
            }
            if ($exeHash -eq $script:baselineExe) { throw "$Label reused the original test executable" }
        } elseif ($result.Exit -ne 0 -or $text -notmatch 'test result: ok\. 1 passed; 0 failed;') {
            throw "$Label original regression failed"
        }
        Assert-Pins
        "PASS $Label exit=$($result.Exit) pid=$($result.Pid) source=$phaseHash exe=$exeHash child=$ExpectedChild" |
            Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append | Write-Host
        $text -split "`n" | Where-Object { $_ -match 'Running.*rustc|association sync child|own association sync subprocess|test result:|panicked at' } | ForEach-Object { Write-Host $_ }
        return $exeHash
    } finally {
        $current = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
        if ($current -ne $phaseHash -and $current -ne $originalHash) { throw 'Source changed externally; refusing to overwrite another writer' }
        [IO.File]::WriteAllBytes($sourcePath, $original)
        if ((Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash -ne $originalHash) { throw 'Byte/SHA256 restoration mismatch' }
        "RESTORED $Label source=$originalHash" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append | Write-Host
    }
}

$failure = $null
try {
    # Validate all exact anchors before touching source or spending a compile.
    foreach ($case in $cases) {
        if ([regex]::Matches($originalText, [regex]::Escape($case.Old)).Count -ne 1) { throw "Missing/ambiguous anchor: $($case.Name)" }
    }
    $script:baselineExe = Invoke-Phase 'original' $original
    foreach ($case in $cases) {
        $mutant = [Text.Encoding]::UTF8.GetBytes($originalText.Replace($case.Old, $case.New))
        $null = Invoke-Phase $case.Name $mutant $case.Child
    }
} catch {
    $failure = $_
} finally {
    # Re-run original behavior after restoration even if a mutant was rejected.
    try { $null = Invoke-Phase 'restored-original' $original } catch { if ($null -eq $failure) { $failure = $_ } else { Write-Error $_ -ErrorAction Continue } }
    Assert-Pins
    Write-Host "Evidence: $proof; excluded: $excluded"
}
if ($null -ne $failure) { throw $failure }
Write-Host 'PASS original/restored Linux regression and 2 independent compiling mutants; omit-directory excluded.'
