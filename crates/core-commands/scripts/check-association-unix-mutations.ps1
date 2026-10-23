# Hosted Linux only. Never run against live association data: the selected Rust
# test uses private fixtures and bounds/kills/reaps only its own subprocesses.
param([string]$EvidenceRoot = [IO.Path]::GetTempPath(), [switch]$ProcessFixture)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsLinux -and -not $ProcessFixture) { throw 'Association Unix mutation proof requires Linux; no Windows runtime inference.' }

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

if (-not ('AssociationBoundedProcessV2' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Threading;
using System.Threading.Tasks;
public sealed class AssociationProcessReceipt {
    public int Pid, Exit = -1;
    public string Out = "", Err = "", Detail = "";
    public bool ProcessTimeout, CaptureTimeout, RootReaped, CaptureComplete;
    public bool CleanupComplete, ReadersDisposed, KillReturned, Complete;
}
public static class AssociationBoundedProcessV2 {
    public static AssociationProcessReceipt Run(ProcessStartInfo info, int processMs, int captureMs, int cleanupMs) {
        var r = new AssociationProcessReceipt();
        var p = new Process { StartInfo = info };
        var cancellation = new CancellationTokenSource();
        Task<string[]> capture = null;
        bool started = false;
        try {
            if (!p.Start()) throw new InvalidOperationException("Process start refused");
            started = true;
            r.Pid = p.Id;
            capture = Task.WhenAll(p.StandardOutput.ReadToEndAsync(cancellation.Token), p.StandardError.ReadToEndAsync(cancellation.Token));
            r.RootReaped = p.WaitForExit(processMs);
            r.ProcessTimeout = !r.RootReaped;
            if (r.RootReaped) {
                r.Exit = p.ExitCode;
                r.CaptureComplete = capture.Wait(captureMs);
                r.CaptureTimeout = !r.CaptureComplete;
                if (r.CaptureComplete) {
                    // Result access only after bounded, successful completion.
                    r.Out = capture.Result[0];
                    r.Err = capture.Result[1];
                }
            }
        } catch (Exception e) {
            r.Detail = "process/capture failure: " + e.GetType().Name + ": " + e.Message;
        } finally {
            // Kill, reap, cancellation and reader disposal can themselves block.
            // Isolate ALL of them in a background task; never synchronously
            // dispose these readers/process on the restoration thread.
            var cleanup = Task.Run(() => {
                try {
                    if (started && !r.RootReaped) {
                        if (!p.HasExited) { p.Kill(true); r.KillReturned = true; }
                        r.RootReaped = p.WaitForExit(Math.Min(1000, cleanupMs));
                    }
                    cancellation.Cancel();
                    if (capture != null) {
                        try { capture.Wait(Math.Min(1000, cleanupMs)); } catch (AggregateException) { }
                    }
                    if (started) {
                        p.StandardOutput.Dispose();
                        p.StandardError.Dispose();
                        r.ReadersDisposed = true;
                    }
                    p.Dispose();
                    cancellation.Dispose();
                } catch (Exception e) {
                    r.Detail += "; cleanup failure: " + e.GetType().Name + ": " + e.Message;
                }
            });
            r.CleanupComplete = cleanup.Wait(cleanupMs);
        }
        // Root reaping never proves that an exited root's inherited-pipe holder
        // was stopped. Do not discover/kill an unrelated process by name or PID.
        if (r.CaptureTimeout) r.Detail += "; inherited output holder cleanup UNVERIFIED";
        if (r.ProcessTimeout) r.Detail += "; process deadline exceeded; descendant cleanup UNVERIFIED";
        if (!r.CleanupComplete) r.Detail += "; bounded cleanup expired; kill/reap/reader disposal UNVERIFIED";
        r.Complete = r.RootReaped && r.CaptureComplete && r.CleanupComplete && r.ReadersDisposed &&
            !r.ProcessTimeout && !r.CaptureTimeout && r.Detail.Length == 0;
        return r;
    }
    public static bool StopKnown(Process p, int timeoutMs) {
        var cleanup = Task.Run(() => {
            try { if (!p.HasExited) p.Kill(true); return p.WaitForExit(timeoutMs); }
            catch { return false; }
        });
        return cleanup.Wait(timeoutMs) && cleanup.Status == TaskStatus.RanToCompletion && cleanup.Result;
    }
}
'@
}
$script:cleanupUnverified = $false
function Invoke-RecordedProcess([string]$Executable, [string[]]$Arguments, [string]$Label,
    [int]$ProcessMs = 170000, [int]$CaptureMs = 3000, [int]$CleanupMs = 5000, [switch]$InspectFailure) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $repo
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    # Inherit target directory, RUSTC_WRAPPER/sccache and all existing CI env.
    # Default stages total <=178s. In particular there is no parameterless
    # WaitForExit or unbounded GetResult after the root exits or is killed.
    $result = [AssociationBoundedProcessV2]::Run($info, $ProcessMs, $CaptureMs, $CleanupMs)
    "label=$Label pid=$($result.Pid) exit=$($result.Exit) process_ms=$ProcessMs capture_ms=$CaptureMs cleanup_ms=$CleanupMs root_reaped=$($result.RootReaped) capture_complete=$($result.CaptureComplete) cleanup_complete=$($result.CleanupComplete) readers_disposed=$($result.ReadersDisposed) detail=$($result.Detail)" |
        Add-Content -LiteralPath (Join-Path $proof 'pids.log')
    $result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $proof "$Label.process.json")
    if (-not $result.Complete) {
        $script:cleanupUnverified = $true
        if (-not $InspectFailure) { throw "$Label failed closed: $($result.Detail); PID=$($result.Pid); see $Label.process.json" }
    }
    return $result
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

function Invoke-Phase([string]$Label, [byte[]]$Bytes, [string]$ExpectedChild = '', [scriptblock]$ProcessAction = $null) {
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
        $result = if ($ProcessAction) { & $ProcessAction $Label } else {
            Invoke-RecordedProcess 'cargo' @('test', '--offline', '--locked', '-j', '4', '-p', 'kwil-core-commands', '--lib', $test, '--message-format=json', '-v', '--', '--exact', '--nocapture', '--test-threads=1') $Label
        }
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

if ($ProcessFixture) {
    # Portable process-only fixture. The REAL phase finally is exercised against
    # a disposable marker; neither Rust file is ever changed and Cargo is not run.
    $pwsh = (Get-Process -Id $PID).Path
    $normal = Invoke-RecordedProcess $pwsh @('-NoProfile', '-Command', '[Console]::Write("captured"); [Console]::Error.Write("stderr"); exit 0') 'fixture-normal' 5000 500 1500
    if ($normal.Out -ne 'captured' -or $normal.Err -ne 'stderr' -or -not $normal.Complete) { throw 'Normal process capture fixture failed' }
    $slow = Invoke-RecordedProcess $pwsh @('-NoProfile', '-Command', 'Start-Sleep -Seconds 20') 'fixture-process-timeout' 800 300 1500 -InspectFailure
    if (-not $slow.ProcessTimeout -or -not $slow.RootReaped -or -not $slow.CleanupComplete) { throw 'Bounded kill/reap fixture failed' }
    $script:cleanupUnverified = $false
    $sourcePath = Join-Path $proof 'restore-marker.bin'
    $original = [Text.Encoding]::UTF8.GetBytes("original fixture marker`n")
    [IO.File]::WriteAllBytes($sourcePath, $original)
    $originalHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
    $holderReceipt = Join-Path $proof 'known-holder.json'
    $parent = Join-Path $proof 'pipe-parent.ps1'
    @'
param([string]$Receipt)
$info = [Diagnostics.ProcessStartInfo]::new()
$info.FileName = (Get-Process -Id $PID).Path
$info.UseShellExecute = $false
foreach ($arg in @('-NoProfile', '-Command', 'Start-Sleep -Seconds 20')) { $info.ArgumentList.Add($arg) }
# Inherit BOTH redirected output handles, then let this root exit first.
$holder = [Diagnostics.Process]::Start($info)
@{ pid=$holder.Id; startTicks=$holder.StartTime.ToUniversalTime().Ticks.ToString() } | ConvertTo-Json | Set-Content -LiteralPath $Receipt
exit 0
'@ | Set-Content -LiteralPath $parent
    $caught = $null
    try {
        try {
            $null = Invoke-Phase 'fixture-inherited-pipe' ([Text.Encoding]::UTF8.GetBytes('temporary fixture mutation')) '' {
                param($label)
                Invoke-RecordedProcess $pwsh @('-NoProfile', '-File', $parent, '-Receipt', $holderReceipt) $label 5000 300 1500
            }
        } catch { $caught = $_ }
        if ($null -eq $caught -or $caught.ToString() -notmatch 'failed closed.*inherited output holder cleanup UNVERIFIED') { throw 'Inherited-pipe fixture did not fail closed at bounded capture' }
        $receipt = Get-Content -LiteralPath (Join-Path $proof 'fixture-inherited-pipe.process.json') -Raw | ConvertFrom-Json
        if (-not $receipt.CaptureTimeout -or -not $receipt.RootReaped -or -not $script:cleanupUnverified) { throw 'Inherited-pipe cleanup uncertainty was concealed' }
        if ((Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash -ne $originalHash) { throw 'Real phase finally failed marker byte/SHA restoration' }
        Write-Host 'PASS normal capture, bounded process kill/reap, inherited-pipe timeout, and real phase finally marker restoration'
    } finally {
        # The runner cannot identify a pipe holder after its root exited. This
        # fixture separately owns its explicit child receipt and verifies identity
        # before bounded cleanup; never claim the runner itself cleaned it.
        if (Test-Path -LiteralPath $holderReceipt) {
            $known = Get-Content -LiteralPath $holderReceipt -Raw | ConvertFrom-Json
            $holder = [Diagnostics.Process]::GetProcessById($known.pid)
            if ($holder.StartTime.ToUniversalTime().Ticks.ToString() -cne $known.startTicks) { throw 'Fixture holder PID identity changed; refusing termination' }
            $stopped = [AssociationBoundedProcessV2]::StopKnown($holder, 1500)
            "fixture-known-holder pid=$($known.pid) bounded_cleanup=$stopped runner_cleanup_claim=UNVERIFIED" | Add-Content -LiteralPath (Join-Path $proof 'pids.log')
            if (-not $stopped) { throw 'Fixture known-holder cleanup unverified' }
        } else { throw 'Fixture holder receipt missing; no unknown process was terminated' }
        Assert-Pins
        Write-Host "Process-only fixture evidence: $proof"
    }
    return
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
    # Re-run after ordinary mutant rejection, but never start more Cargo while
    # prior process/descendant/capture cleanup remains unverified. Phase finally
    # has already restored source; record this exception to the rerun gate.
    if ($script:cleanupUnverified) {
        'SKIPPED restored-original: process/capture/descendant cleanup UNVERIFIED; source restoration required' |
            Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append | Write-Host
    } else {
        try { $null = Invoke-Phase 'restored-original' $original } catch { if ($null -eq $failure) { $failure = $_ } else { Write-Error $_ -ErrorAction Continue } }
    }
    Assert-Pins
    Write-Host "Evidence: $proof; excluded: $excluded"
}
if ($null -ne $failure) { throw $failure }
Write-Host 'PASS original/restored Linux regression and 2 independent compiling mutants; omit-directory excluded.'
