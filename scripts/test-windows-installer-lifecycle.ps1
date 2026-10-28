#requires -Version 7.2
<#
Hosted, disposable Windows ONLY. This file must never be run on the owner PC.
The default mode builds committed HEAD through the real offline dist wrapper,
then runs MSI operations as the SAME current user with a limited token. Network
dependency preparation is hosted-only; no signing, publication or app shutdown.
Every refusal must come from KwilPreflight (or the rollback LaunchCondition)
before InstallInitialize. A random
msiexec error, UAC failure or timeout cannot count as a passing negative case.

The fixed component schema cannot express new upgrade resources. A separate
fixture-only MSI adds a fixed Type19 failure after predecessor removal; it is
never a release package or a shipped guard bypass. Native proof is still needed.
The result intentionally cannot say full lifecycle acceptance with gaps present.
No MSI or fixture is automatically uninstalled/deleted after a failed case.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('x64','arm64')][string]$Architecture,
    [Parameter(Mandatory)][string]$EvidenceDirectory,
    [ValidateSet('Driver','Worker')][string]$Mode = 'Driver',
    [string]$OwnerToken = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$Family = '{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}'
# All seconds below are work/observation budgets, not permission to keep retrying.
# Driver allocation3060s leaves checkout60 + upload300 + slack180 under job3600.
# Worker840s work +60s cleanup fits task900; driver observes it for920s, with20s
# registration/start allowance inside its940s stage. No outer dist process exists.
$Budget = [ordered]@{ Preparation=600; Distribution=1440; Task=940; FinalCleanup=80
    Driver=3060; WorkerWork=840; WorkerTask=900; WorkerWait=920
    ChildCleanup=5; ChildDrain=5; BackendWork=120; BackendCleanup=30; Build=480
    Job=3600; Checkout=60; RunStep=3240; Artifact=300; Orchestration=180 }
$script:Stage = $null
$script:DriverWatch = $null
$script:ActiveAppLease = $null

function Assert-Case([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Hosted MSI lifecycle: $Message" }
}

function Assert-BudgetPlan {
    Assert-Case ($Budget.Driver -eq ($Budget.Preparation+$Budget.Distribution+$Budget.Task+$Budget.FinalCleanup)) 'driver stage budgets do not sum to3060s'
    Assert-Case (($Budget.Checkout+$Budget.Driver+$Budget.Artifact+$Budget.Orchestration) -le $Budget.Job) 'job leaves insufficient checkout/artifact/orchestration reserve'
    Assert-Case ($Budget.Driver -lt $Budget.RunStep -and ($Budget.Checkout+$Budget.RunStep+$Budget.Artifact) -le $Budget.Job) 'run-step fence overlaps artifact reserve'
    Assert-Case (($Budget.WorkerWork+60) -le $Budget.WorkerTask -and ($Budget.WorkerTask+20) -le $Budget.WorkerWait -and ($Budget.WorkerWait+20) -le $Budget.Task) 'worker cleanup/start/observation reserves overlap'
    Assert-Case (($Budget.BackendWork+$Budget.BackendCleanup+$Budget.ChildDrain+$Budget.ChildCleanup) -lt $Budget.Distribution) 'backend work/cleanup cannot fit distribution budget'
}

function Assert-Hosted {
    # These checks precede ALL filesystem writes and registry/profile access.
    Assert-Case ($IsWindows -and $env:GITHUB_ACTIONS -ceq 'true') 'requires GITHUB_ACTIONS=true on Windows'
    Assert-Case ($env:RUNNER_ENVIRONMENT -ceq 'github-hosted') 'requires RUNNER_ENVIRONMENT=github-hosted'
    Assert-Case ($env:RUNNER_OS -ceq 'Windows') 'RUNNER_OS must be Windows'
    Assert-Case ($env:GITHUB_RUN_ID -cmatch '^[0-9]+$' -and $env:GITHUB_RUN_ATTEMPT -cmatch '^[0-9]+$') 'missing run identity'
    Assert-Case ($env:GITHUB_SHA -cmatch '^[0-9a-f]{40}$') 'missing exact source commit'
    Assert-Case ($env:RUNNER_TEMP -and [IO.Path]::IsPathFullyQualified($env:RUNNER_TEMP)) 'missing absolute RUNNER_TEMP'
    $expected = if ($Architecture -ceq 'x64') { 'X64' } else { 'Arm64' }
    Assert-Case ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -ceq $expected) 'OS architecture mismatch; emulation is not native coverage'
    Assert-Case ([Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString() -ceq $expected) 'PowerShell must run natively'
    $full = [IO.Path]::GetFullPath($EvidenceDirectory)
    $parent = [IO.Path]::GetFullPath($env:RUNNER_TEMP).TrimEnd('\') + '\'
    Assert-Case ($full.StartsWith($parent, [StringComparison]::OrdinalIgnoreCase) -and $full.Length -gt $parent.Length) 'evidence must be strictly inside RUNNER_TEMP'
    $script:EvidenceDirectory = $full
}

function Assert-Plain([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            Assert-Case (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "reparse ancestry: $cursor"
        }
        $next = [IO.Path]::GetDirectoryName($cursor)
        if ($next -eq $cursor) { break }
        $cursor = $next
    }
}

function Write-Json([string]$Path, $Object) {
    [IO.File]::WriteAllText($Path, ($Object | ConvertTo-Json -Depth 12) + "`n", [Text.UTF8Encoding]::new($false))
}

function Get-Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

function Add-Receipt([string]$Text) {
    [IO.File]::AppendAllText((Join-Path $EvidenceDirectory 'processes.log'), "$(Get-Date -Format o) $Text`n")
}

function Start-Stage([string]$Name, [int]$Seconds) {
    $script:Stage = [pscustomobject]@{Name=$Name;Seconds=$Seconds;Watch=[Diagnostics.Stopwatch]::StartNew()}
}

function Get-StageRemaining([int]$ReserveSeconds = 0) {
    Assert-Case ($null -ne $script:Stage) 'missing absolute stage deadline'
    $remaining = $script:Stage.Seconds - $script:Stage.Watch.Elapsed.TotalSeconds
    if ($null -ne $script:DriverWatch) {
        $remaining = [Math]::Min($remaining,$Budget.Driver - $script:DriverWatch.Elapsed.TotalSeconds)
    }
    return [int][Math]::Floor($remaining - $ReserveSeconds)
}

function Get-AppAdmission([int]$RequestedMilliseconds,[long]$WorkerRemainingMilliseconds) {
    # Worst bounded native cleanup: watchdog reap5s, final reap retry5s,
    # ConPTY drain5s. Keep all15s BEFORE the absolute worker work fence.
    $reserve=([long](2*$Budget.ChildCleanup+$Budget.ChildDrain))*1000
    $available=$WorkerRemainingMilliseconds-$reserve
    Assert-Case ($RequestedMilliseconds -gt 0 -and $available -ge 1) 'no app lifetime remains after exact cleanup reserve; refused before creation'
    return [pscustomobject]@{LifetimeMilliseconds=[int][Math]::Min($RequestedMilliseconds,$available)
        RequestedMilliseconds=$RequestedMilliseconds;WorkerRemainingMilliseconds=$WorkerRemainingMilliseconds;CleanupReserveMilliseconds=$reserve}
}

function Get-AppWait([int]$MaximumMilliseconds,[long]$RemainingAppMilliseconds,[switch]$CleanupObservation) {
    $available=[Math]::Max(0,$RemainingAppMilliseconds)
    # The deadline probe may observe the watchdog's exact-handle reap, using
    # only its pre-reserved5s. Startup gets no cleanup-time extension.
    if ($CleanupObservation) { $available+=$Budget.ChildCleanup*1000 }
    Assert-Case ($MaximumMilliseconds -ge 0 -and $available -gt 0) 'app effective deadline exhausted before wait'
    return [int][Math]::Min($MaximumMilliseconds,$available)
}

function Test-LateAppAdmission {
    $null=Assert-Owner
    # Pure fake elapsed input to the SAME admission/wait calculations. Never
    # replace the real worker clock or launch a late app to test this boundary.
    $fakeElapsed=([long]$Budget.WorkerWork*1000)-17000
    $remaining=([long]$Budget.WorkerWork*1000)-$fakeElapsed
    $admission=Get-AppAdmission 400000 $remaining
    Assert-Case ($admission.LifetimeMilliseconds -eq 2000) 'late-stage app watchdog was not capped to2s'
    Assert-Case (($admission.LifetimeMilliseconds+$admission.CleanupReserveMilliseconds) -le $remaining) 'late app lifetime crosses the worker fence'
    Assert-Case ((Get-AppWait 1000 50) -eq 50) 'startup wait ignored the same effective app deadline'
    $refused=$false
    try { $null=Get-AppAdmission 400000 $admission.CleanupReserveMilliseconds }
    catch { if ($_.Exception.Message -notlike '*no app lifetime remains*') { throw }; $refused=$true }
    Assert-Case $refused 'cleanup-only late admission unexpectedly allowed process creation'
    $oracle=@{case='late-app-admission';fakeElapsedMilliseconds=$fakeElapsed;remainingMilliseconds=$remaining
        admission=$admission;cleanupOnlyAdmissionRefused=$refused;processCreated=$false;passed=$true}
    Write-Json (Join-Path $EvidenceDirectory 'r3-late-admission-oracle.json') $oracle
    $script:Results.Add($oracle)
}

function Get-ChildWait([int]$WorkSeconds, [switch]$SelfManagedBackend) {
    $available = Get-StageRemaining ($Budget.ChildCleanup + $Budget.ChildDrain)
    if ($null -ne $script:ActiveAppLease) {
        $appAvailable=[int][Math]::Floor($script:ActiveAppLease.RemainingMilliseconds / 1000) - $Budget.ChildCleanup - $Budget.ChildDrain
        $available=[Math]::Min($available,$appAvailable)
    }
    if ($SelfManagedBackend) {
        # Backend enforces its own immutable120s work deadline. Do not launch
        # it with an outer cancellation that can preempt its exact-child cleanup.
        $wait = $Budget.BackendWork + $Budget.BackendCleanup
        Assert-Case ($available -ge $wait) 'insufficient stage budget for backend120s plus30s cleanup; refused before launch'
        return $wait
    }
    Assert-Case ($available -ge 1) 'absolute stage deadline reached; no new child launched'
    return [Math]::Min($WorkSeconds,$available)
}

function Invoke-Owned {
    param([string]$Executable, [string[]]$Arguments, [string]$Label, [int]$TimeoutSeconds = 120, [hashtable]$ChildEnvironment = @{}, [switch]$SelfManagedBackend)
    $waitSeconds = Get-ChildWait $TimeoutSeconds -SelfManagedBackend:$SelfManagedBackend
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable; $info.UseShellExecute = $false
    $info.CreateNoWindow = $true; $info.WindowStyle = 'Hidden'
    $info.WorkingDirectory = $Repository
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    foreach ($name in $ChildEnvironment.Keys) { $info.Environment[$name] = [string]$ChildEnvironment[$name] }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $out = [IO.File]::Create((Join-Path $EvidenceDirectory "$Label.stdout.log"))
    $err = [IO.File]::Create((Join-Path $EvidenceDirectory "$Label.stderr.log"))
    $header = [Text.Encoding]::UTF8.GetBytes("commit=$($env:GITHUB_SHA) label=$Label`n")
    $out.Write($header,0,$header.Length); $err.Write($header,0,$header.Length)
    $recordedPid = 0
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $state = [ordered]@{label=$Label;pid=0;stage=$script:Stage.Name;workLimitSeconds=$TimeoutSeconds;waitSeconds=$waitSeconds
        startedUtc='';status='not-started';timeout=$false;killAttempted=$false;exactParentExited=$false;descendants='not-enumerated'}
    $boundaryComplete = $false
    $outTask=$null; $errTask=$null
    try {
        $waitSeconds = Get-ChildWait $TimeoutSeconds -SelfManagedBackend:$SelfManagedBackend
        $state.waitSeconds=$waitSeconds; $watch.Restart()
        Assert-Case ($process.Start()) "could not start $Label"
        $recordedPid = $process.Id
        $state.pid=$recordedPid; $state.startedUtc=$process.StartTime.ToUniversalTime().ToString('o'); $state.status='running'
        Write-Json (Join-Path $EvidenceDirectory "$Label.process.json") $state
        Add-Receipt "label=$Label pid=$recordedPid workLimitSeconds=$TimeoutSeconds waitSeconds=$waitSeconds stage=$($script:Stage.Name)"
        $outTask = $process.StandardOutput.BaseStream.CopyToAsync($out)
        $errTask = $process.StandardError.BaseStream.CopyToAsync($err)
        $milliseconds=[int][Math]::Max(0,($waitSeconds * 1000)-$watch.ElapsedMilliseconds)
        if (-not $process.WaitForExit($milliseconds)) { $state.timeout=$true; throw "$Label timeout; no retry or cleanup MSI follows" }
        Assert-Case ([Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($outTask,$errTask), $Budget.ChildDrain * 1000)) "$Label stream drain timeout"
        Add-Receipt "label=$Label pid=$recordedPid exitCode=$($process.ExitCode)"
        $state.status='exited'; $state.exitCode=$process.ExitCode; $boundaryComplete=$true
        return $process.ExitCode
    } finally {
        if ($recordedPid -gt 0) {
          try {
            if (-not $process.HasExited) {
                # Receipt/drain setup failures still wait to this exact child's
                # recorded deadline. An unrelated exception is not a kill grant.
                $milliseconds=[int][Math]::Max(0,($waitSeconds * 1000)-$watch.ElapsedMilliseconds)
                if (-not $process.WaitForExit($milliseconds)) {
                    $state.timeout=$true
                    Assert-Case ($process.Id -eq $recordedPid) 'owned process identity changed'
                    $state.killAttempted=$true
                    $process.Kill() # exact owned Process only; never a process tree
                    Assert-Case ($process.WaitForExit($Budget.ChildCleanup * 1000)) "owned child $recordedPid could not be reaped"
                }
            }
            $state.exactParentExited=$process.HasExited
          } finally {
            if (-not $boundaryComplete) {
                # Killing/reaping a parent does not prove its descendants exited.
                # No scan/tree kill or follow-up stage; preserve for VM disposal.
                $state.status='failed-boundary'; $state.descendants='unresolved'; $script:RetainDrive=$true
            }
            Write-Json (Join-Path $EvidenceDirectory "$Label.process.json") $state
          }
        }
        if ($state.timeout) {
            $script:RetainDrive = $true
            Add-Receipt "label=$Label pid=$recordedPid timeout=true exactParentExited=$($state.exactParentExited) descendants=unresolved"
        }
        $out.Dispose(); $err.Dispose(); $process.Dispose()
    }
}

function Invoke-Checked([string]$Executable, [string[]]$Arguments, [string]$Label, [int]$TimeoutSeconds = 120, [hashtable]$ChildEnvironment = @{}, [switch]$SelfManagedBackend) {
    $code = Invoke-Owned $Executable $Arguments $Label $TimeoutSeconds $ChildEnvironment -SelfManagedBackend:$SelfManagedBackend
    if ($code -ne 0) { $script:RetainDrive=$true }
    Assert-Case ($code -eq 0) "$Label exit=$code; see retained logs"
}

function Invoke-HostedDist([string]$LinkerDirectory) {
    # Execute the unchanged real wrapper in THIS process. There is no outer pwsh
    # wrapper PID to kill while it is cleaning up its recorded children.
    . (Join-Path $PSScriptRoot 'dist.ps1')
    $script:HostedDistOriginalProcess = ${function:Invoke-DistProcess}
    $sourceHash = Get-Hash (Join-Path $PSScriptRoot 'dist.ps1')
    function Invoke-DistProcess {
        param([string]$FilePath,[string[]]$Arguments,[string]$WorkingDirectory,[int]$TimeoutSeconds,[string]$LogDirectory,[string]$Label)
        $selfManaged = $Label -ceq 'msi-package'
        $effective = Get-ChildWait $TimeoutSeconds -SelfManagedBackend:$selfManaged
        Add-Receipt "distChild=$Label originalLimitSeconds=$TimeoutSeconds effectiveWaitSeconds=$effective stage=distribution adapter=absolute-deadline"
        # Invoke the captured ORIGINAL scriptblock, not this adapter again.
        # It retains stream/refusal/exit/env semantics and exact-PID receipts.
        $effective=[Math]::Min($effective,(Get-ChildWait $TimeoutSeconds -SelfManagedBackend:$selfManaged))
        return & $script:HostedDistOriginalProcess $FilePath $Arguments $WorkingDirectory $effective $LogDirectory $Label
    }
    $oldPath = [Environment]::GetEnvironmentVariable('PATH','Process')
    $outcome='failed'
    try {
        [Environment]::SetEnvironmentVariable('PATH',$LinkerDirectory+';'+$oldPath,'Process')
        # Both builds keep a finite480s ceiling and share ONE1440s stage clock;
        # metadata, packaging, drains and cleanup consume that same stage.
        Invoke-KwilDist -Repository $Repository -BuildTimeout $Budget.Build -PackageTimeout $Budget.BackendWork -KeepWork $true
        Assert-Case ((Get-StageRemaining) -ge 0) 'distribution stage exceeded absolute deadline'
        $outcome='completed'
    } finally {
        if ($null -eq $oldPath) { [Environment]::SetEnvironmentVariable('PATH',[System.Management.Automation.Language.NullString]::Value,'Process') }
        else { [Environment]::SetEnvironmentVariable('PATH',$oldPath,'Process') }
        if (Get-Variable DistUnreapedProcess -Scope Script -ErrorAction SilentlyContinue) {
            if ($script:DistUnreapedProcess) { $script:RetainDrive=$true }
        }
        Write-Json (Join-Path $EvidenceDirectory 'dist-stage.json') @{commit=$env:GITHUB_SHA;outcome=$outcome;seconds=$script:Stage.Watch.Elapsed.TotalSeconds;limitSeconds=$Budget.Distribution;wrapperInProcess=$true;wrapperSourceSha256=$sourceHash;unresolvedOwnership=$script:RetainDrive;childReceipts='ephemeral-drive/Development/_cache/cargo-target/kwil-dist-work-*/logs/processes.log'}
        $script:HostedDistOriginalProcess=$null
        Assert-Case ((Get-Hash (Join-Path $PSScriptRoot 'dist.ps1')) -ceq $sourceHash) 'real wrapper source changed during invocation'
    }
}

function Get-Sid {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try { return $identity.User.Value } finally { $identity.Dispose() }
}

function Assert-Owner {
    Assert-Hosted
    Assert-Plain $EvidenceDirectory
    $owner = Get-Content -LiteralPath (Join-Path $EvidenceDirectory 'owner.json') -Raw | ConvertFrom-Json
    Assert-Case ($owner.token -ceq $OwnerToken -and $OwnerToken -cmatch '^[0-9a-f]{32}$') 'fixture token mismatch'
    Assert-Case ($owner.sid -ceq (Get-Sid) -and $owner.machine -ceq $env:COMPUTERNAME) 'fixture user/machine mismatch'
    Assert-Case ($owner.runId -ceq $env:GITHUB_RUN_ID -and $owner.attempt -ceq $env:GITHUB_RUN_ATTEMPT) 'fixture run mismatch'
    Assert-Case ($owner.architecture -ceq $Architecture -and $owner.repository -ceq $Repository) 'fixture architecture/repository mismatch'
    return $owner
}

function Invoke-Driver {
    $script:DriverWatch=[Diagnostics.Stopwatch]::StartNew()
    Start-Stage 'preparation' $Budget.Preparation
    Assert-Case (-not (Test-Path -LiteralPath $EvidenceDirectory)) 'refusing existing evidence directory'
    Assert-Plain $EvidenceDirectory
    $null = [IO.Directory]::CreateDirectory($EvidenceDirectory)
    $script:OwnerToken = [Guid]::NewGuid().ToString('N')
    $head = Join-Path $EvidenceDirectory 'source-commit.txt'
    Invoke-Checked 'git.exe' @('rev-parse','--verify','HEAD^{commit}') 'head' 30
    $commit = @(Get-Content -LiteralPath (Join-Path $EvidenceDirectory 'head.stdout.log'))[-1].Trim()
    Assert-Case ($commit -cmatch '^[0-9a-f]{40}$' -and $commit -ceq $env:GITHUB_SHA) 'checkout does not match workflow SHA'
    [IO.File]::WriteAllText($head, "$commit`n")
    Write-Json (Join-Path $EvidenceDirectory 'owner.json') ([ordered]@{
        token=$OwnerToken; sid=Get-Sid; machine=$env:COMPUTERNAME; runId=$env:GITHUB_RUN_ID
        attempt=$env:GITHUB_RUN_ATTEMPT; architecture=$Architecture; repository=$Repository; commit=$commit
    })
    Add-Receipt "driver pid=$PID timeoutSeconds=$($Budget.Driver) commit=$commit"
    Write-Json (Join-Path $EvidenceDirectory 'budgets.json') $Budget
    Write-Json (Join-Path $EvidenceDirectory 'r1-repair-oracles.json') @(
        @{finding='P1-outer-dist-cancellation';oracle='wrapperInProcess=true; each original child receipt has an absolute-stage-capped wait; stage expiry refuses the next child; backend requires120+30 before launch';tamper='old2400s wrapper timeout cannot occur because no wrapper child exists';runtimeVerification='not-run'},
        @{finding='P2-masked-foreign-R';oracle='fresh empty/wrong-type R requires its exact foreign-resource identity even with a foreign containing K; installed changed R requires exact changed-owned-value identity';tamper='a key-only or different-resource diagnostic fails ExpectedResource';runtimeVerification='not-run'},
        @{finding='P2-metadata-only-upgrade';oracle='LICENSES.txt hash and size change with same component/id/fileKey; old bytes before upgrade, new hash and not-old hash after';tamper='retaining old file bytes with new registration fails replacement oracle';runtimeVerification='not-run'},
        @{finding='P2-visible-task-worker';oracle='scheduled pwsh arguments explicitly require -WindowStyle Hidden';tamper='removing Hidden from action arguments fails before task registration';runtimeVerification='not-run'},
        @{finding='P2-job-budget-overrun';oracle='Assert-BudgetPlan plus monotonic per-stage launch caps; driver3060+checkout60+artifact300+slack180=job3600';tamper='increasing a stage without reallocating totals or artifact reserve fails budget assertions';runtimeVerification='not-run'}
    )
    Write-Json (Join-Path $EvidenceDirectory 'r2-repair-oracles.json') @{
        finding='P2-app-first-receipt-outside-cleanup';runtimeVerification='not-run'
        oracle='native creation, in-memory handle/PID owner,400s watchdog and every receipt share a pre-established finally; primary ErrorRecord carries cleanup diagnostics'
        injectedReceipt='first receipt throws IOException after real hosted CreateProcess/ConPTY setup in all four mapped/active pair modes; exact process exit, both handle closes and ConPTY disposal required'
        injectedDeadline='real mapped image uses100ms watchdog and bounded2000ms observation; cleanup must report deadlineExpired=true'
        tamper='moving first receipt outside scope or disabling reap/handle close prevents injected receipt probe success; disabling watchdog fails deadline probe'
    }
    Write-Json (Join-Path $EvidenceDirectory 'result.json') @{status='not-run';commit=$commit;architecture=$Architecture}
    # Provision the dist wrapper's fixed drive only on this fresh hosted VM. Do
    # not redirect or overwrite any preexisting drive, cache or repository data.
    Assert-Case (-not (Test-Path 'E:/')) 'E: already exists; refusing to assume drive ownership'
    $drive = Join-Path $EvidenceDirectory 'ephemeral-drive'
    $null = [IO.Directory]::CreateDirectory($drive)
    Invoke-Checked 'subst.exe' @('E:', $drive) 'map-drive' 15
    $mapped = $true
    try {
        $null = [IO.Directory]::CreateDirectory('E:/Development/_cache/cargo-target')
        $target = if ($Architecture -ceq 'x64') { 'x86_64-pc-windows-msvc' } else { 'aarch64-pc-windows-msvc' }
        $toolchainFile = [IO.File]::ReadAllText((Join-Path $Repository 'rust-toolchain.toml'))
        Assert-Case ($toolchainFile -match '(?m)^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') 'numeric pinned toolchain required'
        $toolchain = $Matches[1]
        Invoke-Checked 'rustup.exe' @('toolchain','install',$toolchain,'--profile','minimal','--no-self-update') 'toolchain' 300
        # Downloads are confined to disposable CI preparation. Actual dist stays
        # --locked --offline and archives committed HEAD through its real driver.
        Invoke-Checked 'rustup.exe' @('run',$toolchain,'cargo','fetch','--locked','--target',$target) 'fetch-app' 300
        Invoke-Checked 'rustup.exe' @('run',$toolchain,'cargo','fetch','--locked','--target',$target,'--manifest-path',(Join-Path $Repository 'crates/installer-guard/Cargo.toml')) 'fetch-guard' 300
        Invoke-Checked 'rustup.exe' @('run',$toolchain,'rustc','--print','sysroot') 'sysroot' 30
        $sysroot = @(Get-Content -LiteralPath (Join-Path $EvidenceDirectory 'sysroot.stdout.log'))[-1].Trim()
        $linkerDirectory = Join-Path $sysroot "lib/rustlib/$target/bin"
        Assert-Case (Test-Path -LiteralPath (Join-Path $linkerDirectory 'rust-lld.exe') -PathType Leaf) 'native pinned rust-lld is absent'
        Assert-Case ((Get-StageRemaining) -ge 0) 'preparation exceeded its600s absolute stage deadline'
        Start-Stage 'distribution' $Budget.Distribution
        Invoke-HostedDist $linkerDirectory
        $directories = @(Get-ChildItem -LiteralPath (Join-Path $Repository 'dist') -Directory | Where-Object { $_.Name -like "*-$target" })
        Assert-Case ($directories.Count -eq 1) 'expected exactly one actual dist directory'
        $null = [IO.Directory]::CreateDirectory((Join-Path $EvidenceDirectory 'original'))
        foreach ($file in Get-ChildItem -LiteralPath $directories[0].FullName -File) {
            Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $EvidenceDirectory 'original')
        }
        Assert-Case ((Get-StageRemaining) -ge 0) 'distribution/original-artifact retention exceeded its absolute1440s stage deadline'
        Start-Stage 'task' $Budget.Task
        # Run under the same logged-on user, RunLevel Limited. No new accounts,
        # passwords, elevation, saved credentials or environment persistence.
        $forward = @{}
        foreach ($name in 'GITHUB_ACTIONS','RUNNER_ENVIRONMENT','RUNNER_OS','RUNNER_TEMP','GITHUB_RUN_ID','GITHUB_RUN_ATTEMPT','GITHUB_SHA') {
            $forward[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        }
        $workerSpec = @{environment=$forward; script=$PSCommandPath; evidence=$EvidenceDirectory; architecture=$Architecture; token=$OwnerToken}
        $specBytes = [Text.Encoding]::UTF8.GetBytes(($workerSpec | ConvertTo-Json -Compress))
        $spec64 = [Convert]::ToBase64String($specBytes)
        $workerCommand = @"
`$spec = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$spec64')) | ConvertFrom-Json
foreach (`$p in `$spec.environment.PSObject.Properties) { [Environment]::SetEnvironmentVariable(`$p.Name, [string]`$p.Value, 'Process') }
& `$spec.script -Mode Worker -Architecture `$spec.architecture -EvidenceDirectory `$spec.evidence -OwnerToken `$spec.token
exit `$LASTEXITCODE
"@
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($workerCommand))
        $taskName = "KwilLifecycle-$OwnerToken"
        $action = New-ScheduledTaskAction -Execute (Join-Path $PSHOME 'pwsh.exe') -Argument "-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand $encoded" -WorkingDirectory $Repository
        Assert-Case ($action.Arguments -cmatch '(?:^| )-WindowStyle Hidden(?: |$)') 'task worker lacks explicit hidden window style'
        $principal = New-ScheduledTaskPrincipal -UserId (Get-Sid) -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds $Budget.WorkerTask) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        $null = Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings -ErrorAction Stop
        Add-Receipt "scheduledTask=$taskName timeoutSeconds=$($Budget.WorkerTask) workSeconds=$($Budget.WorkerWork) userSid=$(Get-Sid) runLevel=Limited windowStyle=Hidden"
        try {
            Assert-Case ((Get-StageRemaining) -ge $Budget.WorkerWait) 'task registration exceeded20s allowance; worker refused before launch'
            Start-ScheduledTask -TaskName $taskName
            $deadline = [Diagnostics.Stopwatch]::StartNew()
            $done = Join-Path $EvidenceDirectory 'worker-exit.json'
            while (-not (Test-Path -LiteralPath $done) -and $deadline.Elapsed.TotalSeconds -lt $Budget.WorkerWait -and (Get-StageRemaining) -gt 0) { Start-Sleep -Milliseconds 500 }
            Assert-Case (Test-Path -LiteralPath $done) 'limited-token worker did not finish; preserve VM evidence, never retry elevated'
            $exit = Get-Content -LiteralPath $done -Raw | ConvertFrom-Json
            if ($exit.code -ne 0) { $script:RetainDrive=$true }
            Assert-Case ($exit.code -eq 0) "worker failed: $($exit.message)"
        } finally {
            # Do not stop a task/process on a timeout: task has its own execution
            # deadline. Preserve registration/evidence if it could still run.
            if ((Get-ScheduledTask -TaskName $taskName).State -ne 'Running') {
                Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
            } else { $mapped = $false; $script:RetainDrive=$true; Write-Warning 'Worker still running; retaining its drive mapping until VM disposal.' }
        }
    } finally {
        Start-Stage 'final-cleanup' $Budget.FinalCleanup
        Write-Json (Join-Path $EvidenceDirectory 'driver-budget-state.json') @{elapsedSeconds=$script:DriverWatch.Elapsed.TotalSeconds;limitSeconds=$Budget.Driver;retainDrive=$script:RetainDrive;wrapperProcessLaunched=$false}
        if ($mapped -and -not $script:RetainDrive) { Invoke-Checked 'subst.exe' @('E:','/d') 'unmap-drive' 15 }
    }
}

function Open-Hkcu([string]$Relative, [bool]$Writable = $false) {
    # This is always the native 64-bit current-user view, including ARM64.
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, [Microsoft.Win32.RegistryView]::Registry64)
    try { return $base.OpenSubKey($Relative, $Writable) } finally { $base.Dispose() }
}

function New-ForeignKey([string]$Relative) {
    # Remember EXACTLY the ancestors this single fixture creates. Creating a
    # command key can also create its claimed shell parent: leaving that parent
    # behind would invalidate all later cases. Never delete shared ancestors.
    $script:ForeignKeys = [Collections.Generic.List[string]]::new()
    $cursor = $Relative
    while ($cursor) {
        $key = Open-Hkcu $cursor
        if ($key) { $key.Dispose(); break }
        $script:ForeignKeys.Add($cursor)
        $index = $cursor.LastIndexOf('\')
        if ($index -lt 0) { break }
        $cursor = $cursor.Substring(0,$index)
    }
    Assert-Case ($script:ForeignKeys.Count -gt 0) 'foreign fixture key was already present'
    Write-Json (Join-Path $EvidenceDirectory 'foreign-key-owner.json') @{token=$OwnerToken;keys=$script:ForeignKeys.ToArray()}
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey('CurrentUser','Registry64')
    try { return $base.CreateSubKey($Relative) } finally { $base.Dispose() }
}

function Remove-ForeignKeys {
    $null = Assert-Owner
    $receipt = Get-Content -LiteralPath (Join-Path $EvidenceDirectory 'foreign-key-owner.json') -Raw | ConvertFrom-Json
    Assert-Case ($receipt.token -ceq $OwnerToken -and ($receipt.keys -join '|') -ceq ($script:ForeignKeys -join '|')) 'foreign fixture key ownership changed'
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey('CurrentUser','Registry64')
    try {
        foreach ($relative in $script:ForeignKeys) {
            $key = Open-Hkcu $relative
            try { Assert-Case ($key -and $key.ValueCount -eq 0 -and $key.SubKeyCount -eq 0) 'refusing to remove a nonempty fixture key' }
            finally { if ($key) { $key.Dispose() } }
            $base.DeleteSubKey($relative,$true)
        }
    } finally { $base.Dispose() }
}

function Get-ValueName($Row) { if ($Row.name -ceq '@') { '' } else { $Row.name.Substring(2) } }
function Get-Data($Row) { if ($Row.data -ceq '~empty~') { '' } else { $Row.data.Replace('${I}', $script:InstallRoot.TrimEnd('\') + '\') } }
function Get-ResourcePath($Row) { Join-Path $(if ($Row.root -ceq 'I') { $script:InstallRoot } else { $script:Programs }) $Row.relative }

function Assert-Resources($Package, [bool]$Absent = $false) {
    foreach ($row in $Package.inventory) {
        if ($row.kind -ceq 'F') {
            $path = Get-ResourcePath $row
            if ($Absent) { Assert-Case (-not (Test-Path -LiteralPath $path)) "uninstall left owned file $($row.id)"; continue }
            Assert-Plain $path
            Assert-Case (Test-Path -LiteralPath $path -PathType Leaf) "missing installed file $($row.id)"
            Assert-Case ((Get-Item -LiteralPath $path).Length -eq [long]$row.size -and (Get-Hash $path) -ceq $row.sha256) "installed bytes mismatch $($row.id)"
        } elseif ($row.kind -ceq 'R') {
            $key = Open-Hkcu $row.relative
            try {
                $name = Get-ValueName $row
                $exists = $null -ne $key -and $key.GetValueNames() -ccontains $name
                if ($Absent) { Assert-Case (-not $exists) "uninstall left authored value $($row.id)"; continue }
                Assert-Case $exists "missing authored value $($row.id)"
                Assert-Case ($key.GetValueKind($name) -eq [Microsoft.Win32.RegistryValueKind]::String -and $key.GetValue($name, $null, 'DoNotExpandEnvironmentNames') -ceq (Get-Data $row)) "authored value differs $($row.id)"
            } finally { if ($key) { $key.Dispose() } }
        } elseif (-not $Absent) {
            $key = Open-Hkcu $row.relative
            try { Assert-Case ($null -ne $key) "missing declared key $($row.id)" } finally { if ($key) { $key.Dispose() } }
        }
    }
    if (-not $Absent) { Assert-Case (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot 'kwil.portable'))) 'MSI installed portable marker' }
}

function Get-Registration([string]$ProductCode) {
    # Read-only Windows Installer product API; never Win32_Product (which repairs).
    try { return $script:Installer.ProductInfo($ProductCode, 'LocalPackage') }
    catch {
        # ProductState distinguishes unknown from an unexpected API failure.
        Assert-Case ($script:Installer.ProductState($ProductCode) -eq -1) 'product API failed for a registered product'
        return $null
    }
}

function Assert-Registration($Package, [bool]$Absent = $false) {
    $cache = Get-Registration $Package.productCode
    if ($Absent) { Assert-Case (-not $cache) 'product registration remains'; return }
    Assert-Case ($cache -and (Test-Path -LiteralPath $cache -PathType Leaf)) 'cached original MSI absent'
    Assert-Case ($script:Installer.ProductInfo($Package.productCode, 'VersionString') -ceq $Package.msiVersion) 'registered version mismatch'
    Assert-Case ($script:Installer.ProductInfo($Package.productCode, 'PackageCode') -ceq $Package.packageCode) 'registered PackageCode mismatch'
    Assert-Case ([IO.Path]::GetFullPath($script:Installer.ProductInfo($Package.productCode, 'InstallLocation')).TrimEnd('\') -ieq $script:InstallRoot.TrimEnd('\')) 'Apps & Features location mismatch'
    $arp = Open-Hkcu ('Software\Microsoft\Windows\CurrentVersion\Uninstall\' + $Package.productCode)
    try {
        Assert-Case ($arp -and $arp.GetValue('DisplayName') -ceq (Get-DistReleaseIdentity $Package.releaseVersion).productName -and $arp.GetValue('DisplayVersion') -ceq $Package.msiVersion) 'per-user Apps & Features entry mismatch'
        Assert-Case ([int]$arp.GetValue('WindowsInstaller') -eq 1) 'Apps & Features is not MSI registration'
        Assert-Case ([int]$arp.GetValue('NoModify') -eq 1 -and [int]$arp.GetValue('NoRepair') -eq 1) 'advertised maintenance UI is not suppressed'
    } finally { if ($arp) { $arp.Dispose() } }
}

function Invoke-Msi {
    param([string]$Label, [string]$Path, [ValidateSet('Install','Repair','Uninstall')][string]$Operation = 'Install', [switch]$Refuse, [string[]]$Properties = @(), [string]$ExpectedDiagnostic = 'Kwil installer preflight:', [string]$ExpectedResource = '', [string]$Coverage = 'direct', [switch]$RollbackFault)
    $null = Assert-Owner
    Assert-Case (-not ($Refuse -and $RollbackFault) -and (-not $RollbackFault -or $Operation -ceq 'Install')) 'rollback fault is a distinct install outcome, never a preflight refusal'
    $rawLog = Join-Path $EvidenceDirectory "$Label.msi.native.log"
    $log = Join-Path $EvidenceDirectory "$Label.msi.log"
    $verb = if ($Operation -ceq 'Uninstall') { '/x' } else { '/i' }
    $arguments = @($verb,$Path,'/qn','/norestart','REBOOT=ReallySuppress','/L*V!',$rawLog)
    if ($Operation -ceq 'Repair') { $arguments += @('REINSTALL=ALL','REINSTALLMODE=amus') }
    $arguments += $Properties
    $code = Invoke-Owned (Join-Path $env:SystemRoot 'System32/msiexec.exe') $arguments $Label 120
    Assert-Case (Test-Path -LiteralPath $rawLog) 'missing MSI log'
    # Keep native bytes intact, plus a normalized evidence log whose first line
    # identifies the source. Detect MSI's encoding rather than mixing encodings.
    $reader = [IO.StreamReader]::new($rawLog,$true)
    $writer = [IO.StreamWriter]::new($log,$false,[Text.UTF8Encoding]::new($false))
    try {
        $writer.WriteLine("commit=$($env:GITHUB_SHA) label=$Label raw=$([IO.Path]::GetFileName($rawLog))")
        $buffer = [char[]]::new(4096)
        while (($length=$reader.Read($buffer,0,$buffer.Length)) -gt 0) { $writer.Write($buffer,0,$length) }
    } finally { $reader.Dispose(); $writer.Dispose() }
    $text = [IO.File]::ReadAllText($log)
    if ($RollbackFault) {
        Assert-KwilRollbackFixtureLog $code $text $script:Original.productCode
    } elseif ($Refuse) {
        Assert-Case ($code -eq 1603) "$Label must refuse with 1603, got $code"
        Assert-Case ($text -match [regex]::Escape($ExpectedDiagnostic)) "$Label did not produce expected diagnostic: $ExpectedDiagnostic"
        if ($ExpectedResource) {
            $specific = [regex]::Escape($ExpectedDiagnostic + ' ' + $ExpectedResource) + '\. '
            Assert-Case ($text -match $specific) "$Label refusal was masked by another resource; expected $ExpectedResource"
        }
        Assert-Case ($text -notmatch 'Action start [^\r\n]*: InstallInitialize\.') "$Label entered transaction before refusal"
    } else {
        Assert-Case ($code -eq 0) "$Label failed exit=$code; reboot-required is not silently accepted"
        Assert-Case ($text -match 'Action start [^\r\n]*: KwilPreflight\.') "$Label did not run actual guard"
    }
    $script:Results.Add([ordered]@{case=$Label;operation=$Operation;exitCode=$code;expectedRefusal=[bool]$Refuse;expectedRollbackFailure=[bool]$RollbackFault;expectedResource=$ExpectedResource;coverage=$Coverage;log=[IO.Path]::GetFileName($log)})
    return $text
}

function Read-Package([string]$Path) {
    $side = Read-DistInstallerJson ($Path + '.manifest.json')
    Assert-Case ($side.schema -is [long] -and $side.schema -eq 2 -and $side.familyGuid -ceq $Family -and $side.architecture -ceq $Architecture) 'sidecar protocol mismatch'
    Assert-Case ($side.inventory.Count -eq 29 -and $side.signed -eq $false -and $side.nativeLifecycle -ceq 'not-run') 'invalid schema-2 sidecar'
    Assert-Case ($side.msi.sha256 -ceq (Get-Hash $Path) -and [long]$side.msi.size -eq (Get-Item -LiteralPath $Path).Length) 'MSI sidecar hash/size mismatch'
    Assert-Case ($side.inspection.database -ceq 'passed' -and $side.inspection.cabinet -ceq 'passed' -and $side.inspection.staticImports -ceq 'passed') 'backend inspection missing'
    return $side
}

function New-FixturePackage([string]$Name, [string]$ReleaseVersion, [string]$Commit, [switch]$ChangedNotice) {
    $payload = Join-Path $EvidenceDirectory "payload-$Name"
    $null = [IO.Directory]::CreateDirectory($payload)
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'F' -and $_.root -ceq 'I' }) {
        Copy-Item -LiteralPath (Join-Path $script:Portable $row.relative) -Destination (Join-Path $payload $row.relative)
    }
    if ($ChangedNotice) {
        # Test-only package: preserve the real notices, append a deterministic
        # fixture statement, and keep the same logical resource/component. Do
        # not modify signed/shipped inputs or add an unsupported schema-1 file.
        $notice = Join-Path $payload 'LICENSES.txt'
        [IO.File]::AppendAllText($notice,"`nHosted MSI replacement fixture: $Name release $ReleaseVersion.`n",[Text.UTF8Encoding]::new($false))
        Assert-Case ((Get-Hash $notice) -cne (Get-Hash (Join-Path $script:Portable 'LICENSES.txt'))) 'higher fixture did not change payload bytes'
    }
    # Existing real wrapper independently reconstructs model/import evidence.
    $metadata = Initialize-DistInstallerInput $payload $script:Guard $Architecture $ReleaseVersion $Commit $script:Original.target
    $receipt = @(Get-DistInstallerInputRecords $payload)
    $path = Join-Path $EvidenceDirectory "$Name.msi"
    Invoke-Checked (Join-Path $PSHOME 'pwsh.exe') @('-NoLogo','-NoProfile','-File',(Join-Path $PSScriptRoot 'windows-msi.ps1'),'-PayloadDirectory',$payload,'-OutputPath',$path,'-ReleaseVersion',$ReleaseVersion,'-Architecture',$Architecture,'-Commit',$Commit,'-IconPath',(Join-Path $payload 'kwil.ico')) "backend-$Name" 120 -SelfManagedBackend
    Assert-DistInstallerInputUnchanged $payload $receipt
    $null = Assert-DistInstallerSidecar $path $metadata
    return [pscustomobject]@{Path=$path;Package=(Read-Package $path)}
}

function Assert-Preserved {
    foreach ($item in $script:Sentinels) { Assert-Case ((Get-Hash $item.path) -ceq $item.sha256) "preserved data changed: $($item.path)" }
    $key = Open-Hkcu $script:ShellKey
    try { Assert-Case ($key -and $key.GetValue('LifecycleUnknown') -ceq $OwnerToken) 'unknown registry value lost' } finally { if ($key) { $key.Dispose() } }
    $child = Open-Hkcu ($script:ShellKey + '\LifecycleUnknown')
    try { Assert-Case ($child -and $child.GetValue('sentinel') -ceq $OwnerToken) 'unknown registry subkey lost' } finally { if ($child) { $child.Dispose() } }
    foreach ($relative in $script:RegistrySentinels) {
        $key = Open-Hkcu $relative
        try { Assert-Case ($key -and $key.GetValue('LifecycleExtra') -ceq $OwnerToken) 'unknown value in an authored key lost' } finally { if ($key) { $key.Dispose() } }
        $child = Open-Hkcu ($relative + '\LifecycleExtra')
        try { Assert-Case ($child -and $child.GetValue('sentinel') -ceq $OwnerToken) 'unknown child of an authored key lost' } finally { if ($child) { $child.Dispose() } }
    }
    Assert-Case ([Environment]::GetEnvironmentVariable('PATH','User') -ceq $script:UserPath -and [Environment]::GetEnvironmentVariable('PATH','Machine') -ceq $script:MachinePath) 'persistent PATH changed'
}

function Invoke-Worker {
    # Cooperative launch cutoff leaves60s for exact-child/image cleanup and
    # status/log writes before the scheduled task's unchanged900s hard fence.
    Start-Stage 'lifecycle-work' $Budget.WorkerWork
    $owner = Assert-Owner
    Add-Receipt "worker pid=$PID workSeconds=$($Budget.WorkerWork) taskSeconds=$($Budget.WorkerTask) sid=$(Get-Sid) commit=$($owner.commit) stage=token-check"
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    Assert-Case (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) 'worker token is elevated; never retry with elevation'
    Add-Receipt "worker pid=$PID workSeconds=$($Budget.WorkerWork) taskSeconds=$($Budget.WorkerTask) sid=$(Get-Sid) elevated=false commit=$($owner.commit)"
    $script:Results = [Collections.Generic.List[object]]::new()
    $script:Gaps = @(
        'rollback after early predecessor removal: separate fixed-failure MSI fixture authored; native restoration proof still required',
        'newly claimed resources DURING upgrade, including a new R in a verified owned K: component schema 1 fixes all 29 resources; coordinated schema fixture required',
        'fresh foreign-R direct empty/wrong-type refusal controls authored; native exact-value proof still required',
        'Explorer invocation/icon and GUI first frame: static LNK/verb bytes only in this harness',
        'registry-link/reparse ancestry, corrupt cached original and architecture-switch refusal not yet authored'
    )
    $script:Installer = New-Object -ComObject WindowsInstaller.Installer
    $script:InstallRoot = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Programs/Kwil'
    $script:Programs = [Environment]::GetFolderPath('Programs')
    $script:SettingsRoot = Join-Path ([Environment]::GetFolderPath('ApplicationData')) 'kwil'
    $script:ShellKey = 'Software\Classes\*\shell\Kwil.3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A'
    $familyKey = "Software\Kwil\Installer\$Family"
    foreach ($path in $script:InstallRoot,(Join-Path $script:Programs 'Kwil.lnk'),$script:SettingsRoot) {
        Assert-Plain $path
        Assert-Case (-not (Test-Path -LiteralPath $path)) "not an isolated current-user fixture: $path already exists"
    }
    foreach ($relative in $script:ShellKey,$familyKey) {
        $key = Open-Hkcu $relative
        try { Assert-Case ($null -eq $key) "preexisting claimed registry key $relative" } finally { if ($key) { $key.Dispose() } }
    }
    # No enumeration of other applications, profiles, user registry or cache.
    $related = $script:Installer.RelatedProducts($Family)
    Assert-Case ($related.Count -eq 0) 'existing related product; refusing current-user fixture ownership'
    Write-Json (Join-Path $EvidenceDirectory 'current-user-fixture.json') @{sid=Get-Sid;token=$OwnerToken;installRoot=$script:InstallRoot;programs=$script:Programs;settings=$script:SettingsRoot;claimedResourcesInitiallyAbsent=$true}
    $script:UserPath = [Environment]::GetEnvironmentVariable('PATH','User')
    $script:MachinePath = [Environment]::GetEnvironmentVariable('PATH','Machine')
    . (Join-Path $PSScriptRoot 'dist.ps1')
    . (Join-Path $PSScriptRoot 'installer-rollback-fixture.ps1')
    $msis = @(Get-ChildItem -LiteralPath (Join-Path $EvidenceDirectory 'original') -Filter '*.msi' -File)
    Assert-Case ($msis.Count -eq 1) 'one retained original MSI required'
    $msi = $msis[0].FullName
    $script:Original = Read-Package $msi
    Assert-Case ($script:Original.commit -ceq $owner.commit) 'original MSI source commit differs'
    $outer = Get-Content -LiteralPath (Join-Path $EvidenceDirectory 'original/manifest.json') -Raw | ConvertFrom-Json
    Assert-Case ($outer.SourceCommit -ceq $owner.commit -and $outer.WorkingTreeCodeIncluded -eq $false) 'dist provenance mismatch'
    foreach ($line in Get-Content -LiteralPath (Join-Path $EvidenceDirectory 'original/SHA256SUMS')) {
        Assert-Case ($line -cmatch '^([0-9a-f]{64})  ([A-Za-z0-9._-]+)$') 'invalid dist checksum line'
        Assert-Case ((Get-Hash (Join-Path $EvidenceDirectory "original/$($Matches[2])")) -ceq $Matches[1]) 'dist checksum mismatch'
    }
    $zips = @(Get-ChildItem -LiteralPath (Join-Path $EvidenceDirectory 'original') -Filter '*-portable.zip' -File)
    Assert-Case ($zips.Count -eq 1) 'one real portable ZIP required'
    $script:Portable = Join-Path $EvidenceDirectory 'portable'
    [IO.Compression.ZipFile]::ExtractToDirectory($zips[0].FullName, $script:Portable)
    Assert-Case ((Get-Item -LiteralPath (Join-Path $script:Portable 'kwil.portable')).Length -eq 0) 'portable marker bytes'
    Assert-Case (-not (Test-Path -LiteralPath (Join-Path $script:Portable '.installer'))) 'portable ZIP leaked installer inputs'
    $script:Guard = Join-Path $EvidenceDirectory "ephemeral-drive/Development/_cache/cargo-target/kwil-dist-guard/$($script:Original.target)/release/kwil_installer_guard.dll"
    Assert-Case ((Get-Hash $script:Guard) -ceq $script:Original.guard.sha256) 'actual dist guard mismatch'
    # Preserve the original unchanged. Fixture versions affect only newly authored
    # metadata/MSIs, never Cargo.toml, compiled provenance or the release channel.
    $identity = Get-DistReleaseIdentity $script:Original.releaseVersion
    Assert-Case ($identity.msiVersion -ceq $script:Original.msiVersion -and $identity.channel -ceq $script:Original.channel) 'original release mapping mismatch'
    if ($identity.channel -ceq 'beta') {
        $ordinal = [uint32]$script:Original.releaseVersion.Substring('0.1.0-beta.'.Length)
        $higherRelease = if ($ordinal -lt 65535) { '0.1.0-beta.' + ($ordinal+1).ToString([Globalization.CultureInfo]::InvariantCulture) } else { '0.1.0' }
    } else { throw 'No higher supported ReleaseVersion; coordinated contract update required' }
    $higher = New-FixturePackage 'higher-version' $higherRelease $script:Original.commit -ChangedNotice
    $rollback = New-KwilRollbackFixture $script:Installer $higher.Path (Join-Path $EvidenceDirectory 'rollback-fault-fixture.msi') $Architecture $higher.Package.packageCode
    Write-Json (Join-Path $EvidenceDirectory 'rollback-fixture.json') $rollback
    $oldNotice = @($script:Original.inventory | Where-Object { $_.kind -ceq 'F' -and $_.root -ceq 'I' -and $_.relative -ceq 'LICENSES.txt' })[0]
    $newNotice = @($higher.Package.inventory | Where-Object { $_.kind -ceq 'F' -and $_.root -ceq 'I' -and $_.relative -ceq 'LICENSES.txt' })[0]
    Assert-Case ($oldNotice.component -ceq $newNotice.component -and $oldNotice.id -ceq $newNotice.id -and $oldNotice.fileKey -ceq $newNotice.fileKey) 'replacement fixture changed logical component identity'
    Assert-Case ($oldNotice.sha256 -cne $newNotice.sha256 -and [long]$newNotice.size -gt [long]$oldNotice.size) 'replacement fixture has no observable byte change'
    Write-Json (Join-Path $EvidenceDirectory 'upgrade-replacement-oracle.json') @{relative='LICENSES.txt';component=$oldNotice.component;oldSha256=$oldNotice.sha256;newSha256=$newNotice.sha256;oldSize=$oldNotice.size;newSize=$newNotice.size;fixtureOnly=$true}
    $repack = New-FixturePackage 'same-product-repack' $script:Original.releaseVersion $script:Original.commit
    Assert-Case ($repack.Package.productCode -ceq $script:Original.productCode -and $repack.Package.packageCode -cne $script:Original.packageCode) 'repack fixture identity'
    $differentCommit = if ($script:Original.commit[0] -ceq '0') { '1' + $script:Original.commit.Substring(1) } else { '0' + $script:Original.commit.Substring(1) }
    $equal = New-FixturePackage 'same-version-distinct-build' $script:Original.releaseVersion $differentCommit
    Assert-Case ($equal.Package.productCode -cne $script:Original.productCode) 'distinct build fixture identity'

    # Each collision is a resource owned by this fixture, removed EXACTLY after
    # proving refusal/preservation. Never recursively clean a product directory.
    $null = [IO.Directory]::CreateDirectory($script:InstallRoot)
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'F' }) {
        $path = Get-ResourcePath $row
        foreach ($content in 'different','identical') {
            if ($content -ceq 'different') { [IO.File]::WriteAllText($path, "foreign-$OwnerToken") }
            elseif ($row.root -ceq 'P') { [IO.File]::WriteAllBytes($path,(Get-DistStartMenuLink)) }
            else { Copy-Item -LiteralPath (Join-Path $script:Portable $row.relative) -Destination $path }
            $hash = Get-Hash $path
            $null = Invoke-Msi "foreign-file-$content-$($row.id)" $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: foreign-resource' -ExpectedResource (Get-DistResourceIdentity $row)
            Assert-Case ((Get-Hash $path) -ceq $hash) 'foreign file was changed'
            Assert-Registration $script:Original $true
            [IO.File]::Delete($path)
        }
    }
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'K' }) {
        $key = New-ForeignKey $row.relative
        $key.Dispose()
        $blockingKey = @($script:Original.inventory | Where-Object { $_.kind -ceq 'K' -and $script:ForeignKeys.Contains($_.relative) })[0]
        $null = Invoke-Msi "foreign-empty-key-$($row.id)" $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: foreign-resource' -ExpectedResource (Get-DistResourceIdentity $blockingKey)
        $key = Open-Hkcu $row.relative
        try { Assert-Case ($key -and $key.ValueCount -eq 0 -and $key.SubKeyCount -eq 0) 'empty foreign key changed' } finally { if ($key) { $key.Dispose() } }
        Remove-ForeignKeys
    }
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'R' }) {
        foreach ($kind in 'String','DWord') {
            $key = New-ForeignKey $row.relative
            try { $key.SetValue((Get-ValueName $row), $(if ($kind -ceq 'String') { '' } else { 7 }), [Microsoft.Win32.RegistryValueKind]::$kind) } finally { $key.Dispose() }
            # The containing key is foreign too. The guard observes concrete
            # file/value collisions before keys, so THIS exact empty/wrong-type
            # R must be named. A generic key refusal cannot satisfy this case.
            $null = Invoke-Msi "foreign-value-direct-$kind-$($row.id)" $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: foreign-resource' -ExpectedResource (Get-DistResourceIdentity $row) -Coverage 'direct-R-with-foreign-K'
            $key = Open-Hkcu $row.relative $true
            try {
                Assert-Case ($key.GetValueKind((Get-ValueName $row)).ToString() -ceq $kind) 'foreign value type changed'
                Assert-Case ($key.GetValue((Get-ValueName $row)) -ceq $(if ($kind -ceq 'String') { '' } else { 7 })) 'foreign value changed'
                $key.DeleteValue((Get-ValueName $row), $true)
            } finally { $key.Dispose() }
            Remove-ForeignKeys
        }
    }
    $portableMarker = Join-Path $script:InstallRoot 'kwil.portable'
    [IO.File]::WriteAllBytes($portableMarker, [byte[]]::new(0))
    $null = Invoke-Msi 'portable-marker' $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: portable-marker'
    Assert-Case ((Get-Item -LiteralPath $portableMarker).Length -eq 0) 'portable marker changed'
    [IO.File]::Delete($portableMarker)
    $null = [IO.Directory]::CreateDirectory($portableMarker)
    $null = Invoke-Msi 'portable-marker-directory' $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: portable-marker'
    Assert-Case (Test-Path -LiteralPath $portableMarker -PathType Container) 'portable marker directory changed'
    [IO.Directory]::Delete($portableMarker,$false)
    $null = Invoke-Msi 'effective-install-location' $msi -Refuse -Properties @("INSTALLDIR=$(Join-Path $EvidenceDirectory 'forbidden-location')") -ExpectedDiagnostic 'Kwil installer preflight: session-override'
    $null = Invoke-Msi 'rollback-disabled' $msi -Refuse -Properties @('DISABLEROLLBACK=1') -ExpectedDiagnostic 'Kwil requires Windows Installer rollback.'
    Assert-Registration $script:Original $true

    $null = Invoke-Msi 'fresh-install' $msi
    Assert-Resources $script:Original
    Assert-Registration $script:Original
    Assert-Case (@(Get-ChildItem -LiteralPath $script:InstallRoot -Force -File).Count -eq 7) 'fresh root has unexpected files'
    # LNK is a regular File, never advertised. Verify immutable 868-byte contract
    # independently of installed ownership checks; no shortcut COM resolution.
    $link = [IO.File]::ReadAllBytes((Join-Path $script:Programs 'Kwil.lnk'))
    Assert-Case ($link.Length -eq 868 -and [BitConverter]::ToUInt32($link,20) -eq 0x02000200 -and [BitConverter]::ToUInt32($link,864) -eq 0) 'static nonadvertised link layout'
    Assert-Case ([Text.Encoding]::Unicode.GetString($link,344,520).TrimEnd([char]0) -ceq '%LOCALAPPDATA%\Programs\Kwil\kwil.exe') 'Start Menu target is not GUI exe'
    $script:Sentinels = [Collections.Generic.List[object]]::new()
    $script:RegistrySentinels = [Collections.Generic.List[string]]::new()
    foreach ($path in (Join-Path $script:InstallRoot 'unknown-sibling.txt'),(Join-Path $script:SettingsRoot 'settings.toml'),(Join-Path $script:SettingsRoot 'workspace/recovery-sentinel.bin'),(Join-Path $EvidenceDirectory 'edited-document.txt')) {
        $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
        [IO.File]::WriteAllText($path, "preserve-$OwnerToken")
        $script:Sentinels.Add(@{path=$path;sha256=Get-Hash $path})
    }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey('CurrentUser','Registry64')
    try {
        $key = $base.OpenSubKey($script:ShellKey,$true)
        try { $key.SetValue('LifecycleUnknown',$OwnerToken,'String') } finally { $key.Dispose() }
        $key = $base.CreateSubKey($script:ShellKey + '\LifecycleUnknown')
        try { $key.SetValue('sentinel',$OwnerToken,'String') } finally { $key.Dispose() }
    } finally { $base.Dispose() }
    $null = Invoke-Msi 'same-product-new-package-repair' $repack.Path -Operation Repair -Refuse -ExpectedDiagnostic 'Kwil installer preflight: package-mismatch'
    $null = Invoke-Msi 'same-product-new-package-uninstall' $repack.Path -Operation Uninstall -Refuse -ExpectedDiagnostic 'Kwil installer preflight: package-mismatch'
    $null = Invoke-Msi 'equal-version-distinct-build' $equal.Path -Refuse -ExpectedDiagnostic 'Kwil installer preflight: version-conflict'
    $null = Invoke-Msi 'registered-normal-install' $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: repair-required'
    Assert-Resources $script:Original
    Assert-Preserved
    # Remove every owned resource individually, explicitly repair using original.
    # Covers pair, shortcut, marker, licences, shell/Owner values and missing K.
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -cne 'K' }) {
        if ($row.kind -ceq 'F') { [IO.File]::Delete((Get-ResourcePath $row)) }
        else { $key = Open-Hkcu $row.relative $true; try { $key.DeleteValue((Get-ValueName $row),$true) } finally { $key.Dispose() } }
        $null = Invoke-Msi "repair-missing-$($row.id)" $msi -Operation Repair
        Assert-Resources $script:Original
        Assert-Registration $script:Original
        Assert-Preserved
    }
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'K' -and $_.relative -like '*\Components\*' }) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey('CurrentUser','Registry64')
        try { $base.DeleteSubKey($row.relative,$true) } finally { $base.Dispose() }
        $null = Invoke-Msi "repair-missing-key-$($row.id)" $msi -Operation Repair
        Assert-Resources $script:Original
        Assert-Preserved
    }
    foreach ($name in 'kwil.exe','kwil.com') { [IO.File]::Delete((Join-Path $script:InstallRoot $name)) }
    $null = Invoke-Msi 'repair-both-pair-members-missing' $msi -Operation Repair
    Assert-Resources $script:Original
    Assert-Preserved
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'K' }) {
        $key = Open-Hkcu $row.relative $true
        try {
            $key.SetValue('LifecycleExtra',$OwnerToken,'String')
            $child = $key.CreateSubKey('LifecycleExtra')
            try { $child.SetValue('sentinel',$OwnerToken,'String') } finally { $child.Dispose() }
        } finally { $key.Dispose() }
        $script:RegistrySentinels.Add($row.relative)
    }
    $null = Invoke-Msi 'repair-preserves-all-unknown-registry-values' $msi -Operation Repair
    Assert-Resources $script:Original
    Assert-Preserved
    # Real R-specific tests: registration verifies ownership of each containing
    # K, while only this declared value changes. Empty REG_SZ and wrong types
    # must be reported against THIS R, not a different changed key or resource.
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'R' }) {
        foreach ($kind in 'String','DWord') {
            Assert-Registration $script:Original
            Assert-Resources $script:Original
            $key = Open-Hkcu $row.relative $true
            try { $key.SetValue((Get-ValueName $row),$(if ($kind -ceq 'String') { '' } else { 7 }),[Microsoft.Win32.RegistryValueKind]::$kind) }
            finally { $key.Dispose() }
            foreach ($operation in 'Repair','Uninstall','Install') {
                $packagePath = if ($operation -ceq 'Install') { $higher.Path } else { $msi }
                $null = Invoke-Msi "owned-value-$kind-$($row.id)-$operation" $packagePath -Operation $operation -Refuse -ExpectedDiagnostic 'Kwil installer preflight: changed-owned-value' -ExpectedResource (Get-DistResourceIdentity $row)
                $key = Open-Hkcu $row.relative
                try {
                    Assert-Case ($key.GetValueKind((Get-ValueName $row)).ToString() -ceq $kind) 'changed owned value type was overwritten'
                    Assert-Case ($key.GetValue((Get-ValueName $row)) -ceq $(if ($kind -ceq 'String') { '' } else { 7 })) 'changed owned value was overwritten'
                } finally { $key.Dispose() }
                Assert-Registration $script:Original
                Assert-Preserved
            }
            $key = Open-Hkcu $row.relative $true
            try { $key.SetValue((Get-ValueName $row),(Get-Data $row),'String') } finally { $key.Dispose() }
            Assert-Resources $script:Original
        }
    }
    foreach ($row in $script:Original.inventory | Where-Object { $_.kind -ceq 'F' }) {
        $path = Get-ResourcePath $row
        $backup = Join-Path $EvidenceDirectory "owned-backup-$($row.id)"
        Copy-Item -LiteralPath $path -Destination $backup
        [IO.File]::WriteAllText($path,"changed-$OwnerToken")
        $hash = Get-Hash $path
        foreach ($operation in 'Repair','Uninstall','Install') {
            $packagePath = if ($operation -ceq 'Install') { $higher.Path } else { $msi }
            $null = Invoke-Msi "changed-file-$($row.id)-$operation" $packagePath -Operation $operation -Refuse -ExpectedDiagnostic 'Kwil installer preflight: changed-owned-file'
            Assert-Case ((Get-Hash $path) -ceq $hash) 'changed authored file was overwritten'
        }
        Copy-Item -LiteralPath $backup -Destination $path
        Assert-Resources $script:Original
        Assert-Preserved
    }
    # Changed present authored data must block destructive operations. Restore
    # only this fixture's exact saved value before proceeding to later cases.
    $shellRow = @($script:Original.inventory | Where-Object { $_.kind -ceq 'R' -and $_.relative -ceq $script:ShellKey -and $_.name -ceq '@' })[0]
    $key = Open-Hkcu $script:ShellKey $true
    try { $key.SetValue('',"changed-$OwnerToken",'String') } finally { $key.Dispose() }
    foreach ($operation in 'Uninstall','Repair') { $null = Invoke-Msi "changed-value-$operation" $msi -Operation $operation -Refuse -ExpectedDiagnostic 'Kwil installer preflight: changed-owned-value' -ExpectedResource (Get-DistResourceIdentity $shellRow) }
    $null = Invoke-Msi 'changed-value-upgrade' $higher.Path -Refuse -ExpectedDiagnostic 'Kwil installer preflight: changed-owned-value' -ExpectedResource (Get-DistResourceIdentity $shellRow)
    $key = Open-Hkcu $script:ShellKey $true
    try { Assert-Case ($key.GetValue('') -ceq "changed-$OwnerToken") 'changed authored value was overwritten'; $key.SetValue('',(Get-Data $shellRow),'String') } finally { $key.Dispose() }
    Assert-Resources $script:Original
    Assert-Preserved

    # Real rollback, not a refusal before InstallInitialize. The separate
    # incoming MSI runs BOTH guards, removes the verified predecessor, then its
    # fixed Type19 action fails. Registration, every old byte and all unknown
    # fixture-owned data must return unchanged. The normal higher MSI is intact.
    $oldCacheHash = Get-Hash (Get-Registration $script:Original.productCode)
    $null = Invoke-Msi 'rollback-after-predecessor-removal' $rollback.Path -RollbackFault
    Assert-Resources $script:Original
    Assert-Registration $script:Original
    Assert-Registration $higher.Package $true
    Assert-Case ((Get-Hash (Get-Registration $script:Original.productCode)) -ceq $oldCacheHash) 'rollback changed the cached original package'
    Assert-Case ((Get-Hash $higher.Path) -ceq $rollback.SourceSha256) 'rollback fixture changed the normal upgrade MSI'
    Assert-Preserved
    Invoke-PairCases $msi $higher.Path
    $installedNotice = Join-Path $script:InstallRoot 'LICENSES.txt'
    Assert-Case ((Get-Hash $installedNotice) -ceq $oldNotice.sha256) 'replacement test did not start with original bytes'
    $upgradeLog = Invoke-Msi 'higher-numeric-version-upgrade' $higher.Path
    Assert-Case ([regex]::Matches($upgradeLog, 'Action start [^\r\n]*: KwilPreflight\.').Count -ge 2) 'upgrade did not run both incoming and cached-original guards'
    Assert-Resources $higher.Package
    $installedNoticeHash = Get-Hash $installedNotice
    Assert-Case ($installedNoticeHash -ceq $newNotice.sha256 -and $installedNoticeHash -cne $oldNotice.sha256) 'upgrade left the old payload bytes despite new registration'
    Assert-Registration $higher.Package
    Assert-Registration $script:Original $true
    Assert-Preserved
    $null = Invoke-Msi 'downgrade-refusal' $msi -Refuse -ExpectedDiagnostic 'Kwil installer preflight: version-conflict'
    Assert-Resources $higher.Package
    Assert-Registration $higher.Package
    Assert-Preserved
    $null = Invoke-Msi 'uninstall-owned-values-only' $higher.Path -Operation Uninstall
    Assert-Resources $higher.Package $true
    Assert-Registration $higher.Package $true
    Assert-Preserved
    Assert-Case ((Get-StageRemaining) -ge 0) 'lifecycle cases exceeded the840s work stage;900s task fence reserves cleanup only'
    Write-Json (Join-Path $EvidenceDirectory 'result.json') @{status='implemented-cases-passed-with-acceptance-gaps';fullLifecycleAccepted=$false;commit=$owner.commit;architecture=$Architecture;cases=$script:Results.ToArray();gaps=$script:Gaps}
    # Deliberately fail full qualification until ROOT resolves the named seams.
    throw 'Implemented cases completed, but full lifecycle qualification has explicit acceptance gaps; see result.json'
}

function Invoke-PairCases([string]$OriginalPath, [string]$HigherPath) {
    # Separate mapped-image cases from actively executing hidden GUI/TUI cases.
    # TUI gets an owned ConPTY (not a desktop console or redirected stdin).
    # No process is discovered or killed by name, window title or timing.
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;
public static class KwilLifecycleImage {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct SI {
        public int cb; public string reserved, desktop, title; public int x,y,cx,cy,xChars,yChars,fill,flags;
        public short show, reservedBytes; public IntPtr reservedData,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] public struct EX { public SI startup; public IntPtr attributes; }
    [StructLayout(LayoutKind.Sequential)] public struct COORD { public short x,y; }
    [StructLayout(LayoutKind.Sequential)] public struct PI { public IntPtr process,thread; public int pid,tid; }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool CreateProcessW(string app, StringBuilder command, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd, ref SI si, out PI pi);
    [DllImport("kernel32.dll", EntryPoint="CreateProcessW", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern bool CreateProcessEx(string app, StringBuilder command, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd, ref EX si, out PI pi);
    [DllImport("kernel32.dll", SetLastError=true)] private static extern bool CreatePipe(out IntPtr read, out IntPtr write, IntPtr security, uint size);
    [DllImport("kernel32.dll")] private static extern int CreatePseudoConsole(COORD size, IntPtr input, IntPtr output, uint flags, out IntPtr console);
    [DllImport("kernel32.dll")] private static extern void ClosePseudoConsole(IntPtr console);
    [DllImport("kernel32.dll", SetLastError=true)] private static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, uint flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError=true)] private static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previous, IntPtr returned);
    [DllImport("kernel32.dll")] private static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")] public static extern uint WaitForSingleObject(IntPtr handle, uint ms);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr handle);
    public sealed class Cleanup {
        public int pid;
        public bool deadlineExpired, terminationAttempted, processExited, threadClosed, processClosed, ptyDisposed;
        public string diagnostic="";
        public bool Success { get { return processExited && threadClosed && processClosed && ptyDisposed && diagnostic.Length==0; } }
    }
    public sealed class AppLease {
        private readonly object gate=new object();
        private readonly PI owned;
        private readonly Pty pty;
        private readonly System.Diagnostics.Stopwatch watch;
        private readonly Cleanup result;
        private System.Threading.Timer timer;
        private long lifetimeMs;
        private bool closed;
        public AppLease(PI process,Pty console,System.Diagnostics.Stopwatch elapsed) {
            owned=process; pty=console; watch=elapsed; result=new Cleanup {pid=process.pid,ptyDisposed=console==null};
        }
        public long RemainingMilliseconds { get { lock(gate) { return Math.Max(0,lifetimeMs-watch.ElapsedMilliseconds); } } }
        public Cleanup Snapshot { get { lock(gate) { return result; } } }
        public void Arm(int milliseconds) {
            lock(gate) {
                if (timer!=null || closed || milliseconds<1) throw new InvalidOperationException("app deadline state");
                lifetimeMs=milliseconds;
                // No disk writes/runspace callbacks. The watchdog owns only the
                // exact CreateProcess handle and remains effective if logging stalls.
                timer=new System.Threading.Timer(_ => {
                    lock(gate) {
                        if (closed) return;
                        result.deadlineExpired=true;
                        try { StopProcess(); }
                        catch (Exception e) { Note("deadline cleanup: "+e.Message); }
                    }
                },null,(int)Math.Max(1,lifetimeMs-watch.ElapsedMilliseconds),System.Threading.Timeout.Infinite);
            }
        }
        private void Note(string message) { lock(gate) { result.diagnostic+=(result.diagnostic.Length==0?"":"; ")+message; } }
        private void StopProcess() {
            uint status=WaitForSingleObject(owned.process,0);
            if (status==258) {
                result.terminationAttempted=true;
                if (!TerminateProcess(owned.process,0)) Note("TerminateProcess error="+Marshal.GetLastWin32Error());
                status=WaitForSingleObject(owned.process,5000);
            }
            result.processExited=status==0;
            if (!result.processExited) Note("owned process not reaped, wait="+status);
        }
        public Cleanup Close() {
            lock(gate) {
                if (closed) return result;
                // Cleanup cannot be interrupted by a receipt writer: every step
                // is independent and diagnostics accumulate in memory.
                try { StopProcess(); } catch (Exception e) { Note("process cleanup: "+e.Message); }
                if (result.processExited) {
                    try { result.threadClosed=CloseHandle(owned.thread); if (!result.threadClosed) Note("thread CloseHandle failed"); }
                    catch (Exception e) { Note("thread close: "+e.Message); }
                    try { result.processClosed=CloseHandle(owned.process); if (!result.processClosed) Note("process CloseHandle failed"); }
                    catch (Exception e) { Note("process close: "+e.Message); }
                    closed=true;
                    if (timer!=null) { timer.Dispose(); timer=null; }
                }
                // If the OS could not reap it, retain the exact handles/watchdog
                // and report unresolved ownership. Never close and claim success.
            }
            try { if (pty!=null) pty.Dispose(); result.ptyDisposed=true; }
            catch (Exception e) { Note("ConPTY cleanup: "+e.Message); }
            return result;
        }
    }
    public sealed class Pty : IDisposable {
        public PI process;
        public IntPtr console, input;
        internal Task drain;
        public void Dispose() {
            if (input != IntPtr.Zero) { CloseHandle(input); input=IntPtr.Zero; }
            // The output reader stays active during ClosePseudoConsole, as the
            // API can flush buffered output synchronously. The worker deadline
            // bounds an OS API stall; no extra process is searched or stopped.
            if (console != IntPtr.Zero) { ClosePseudoConsole(console); console=IntPtr.Zero; }
            if (drain != null && !drain.Wait(5000)) throw new TimeoutException("ConPTY output drain");
        }
    }
    public static Pty StartPty(string app, string command, IntPtr environment, string cwd, string outputLog) {
        var p = new Pty();
        IntPtr inputRead=IntPtr.Zero, outputRead=IntPtr.Zero, outputWrite=IntPtr.Zero, attributes=IntPtr.Zero;
        bool initialized=false;
        try {
            if (!CreatePipe(out inputRead,out p.input,IntPtr.Zero,0)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if (!CreatePipe(out outputRead,out outputWrite,IntPtr.Zero,0)) throw new Win32Exception(Marshal.GetLastWin32Error());
            var reader = new SafeFileHandle(outputRead,true); outputRead=IntPtr.Zero;
            p.drain = Task.Run(() => {
                using (reader)
                using (var stream = new FileStream(reader,FileAccess.Read,4096,false))
                using (var log = new FileStream(outputLog,FileMode.CreateNew,FileAccess.Write,FileShare.Read)) {
                    var bytes = new byte[4096]; long saved=0; int n;
                    while ((n=stream.Read(bytes,0,bytes.Length)) != 0) {
                        int retain = (int)Math.Min(n,Math.Max(0,1048576-saved));
                        if (retain>0) { log.Write(bytes,0,retain); saved+=retain; }
                    }
                }
            });
            int hr = CreatePseudoConsole(new COORD {x=80,y=25},inputRead,outputWrite,0,out p.console);
            if (hr<0) Marshal.ThrowExceptionForHR(hr);
            CloseHandle(inputRead); inputRead=IntPtr.Zero;
            CloseHandle(outputWrite); outputWrite=IntPtr.Zero;
            IntPtr size=IntPtr.Zero;
            InitializeProcThreadAttributeList(IntPtr.Zero,1,0,ref size);
            if (size.ToInt64()<=0 || size.ToInt64()>65536) throw new InvalidOperationException("ConPTY attribute size");
            attributes=Marshal.AllocHGlobal(size);
            if (!InitializeProcThreadAttributeList(attributes,1,0,ref size)) throw new Win32Exception(Marshal.GetLastWin32Error());
            initialized=true;
            // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE uses the HPCON value itself.
            if (!UpdateProcThreadAttribute(attributes,0,new IntPtr(0x00020016),p.console,new IntPtr(IntPtr.Size),IntPtr.Zero,IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error());
            var startup = new EX {startup=new SI {cb=Marshal.SizeOf<EX>(),flags=1,show=0}, attributes=attributes};
            if (!CreateProcessEx(app,new StringBuilder(command),IntPtr.Zero,IntPtr.Zero,false,0x00080400,environment,cwd,ref startup,out p.process)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return p;
        } catch {
            if (inputRead!=IntPtr.Zero) { CloseHandle(inputRead); inputRead=IntPtr.Zero; }
            if (outputWrite!=IntPtr.Zero) { CloseHandle(outputWrite); outputWrite=IntPtr.Zero; }
            p.Dispose(); throw;
        } finally {
            if (inputRead!=IntPtr.Zero) CloseHandle(inputRead);
            if (outputRead!=IntPtr.Zero) CloseHandle(outputRead);
            if (outputWrite!=IntPtr.Zero) CloseHandle(outputWrite);
            if (initialized) DeleteProcThreadAttributeList(attributes);
            if (attributes!=IntPtr.Zero) Marshal.FreeHGlobal(attributes);
        }
    }
}
'@
    Test-LateAppAdmission
    foreach ($name in 'kwil.exe','kwil.com') {
        foreach ($state in 'mapped','active') {
            # Real hosted-only setup, same creation path/owner as normal cases.
            # The FIRST receipt writer throws, before startup wait or any MSI.
            $null = Invoke-PairImageCase $name $state $OriginalPath $HigherPath -InjectReceiptFailure
            $null = Invoke-PairImageCase $name $state $OriginalPath $HigherPath
        }
    }
    # A short mapped-image probe proves the native watchdog actually reaps the
    # exact owned process; no400s wait or CPU load generator is needed.
    $null = Invoke-PairImageCase 'kwil.exe' 'mapped' $OriginalPath $HigherPath -ProbeDeadline
}

function Invoke-PairImageCase {
    param([string]$Name,[ValidateSet('mapped','active')][string]$State,[string]$OriginalPath,[string]$HigherPath,
        [switch]$InjectReceiptFailure,[switch]$ProbeDeadline)
    $null=Assert-Owner # hosted/SID/fixture fences also apply to injected cases
    $requestedLifetimeMs=if ($ProbeDeadline) { 100 } else { 400000 }
    $admission=Get-AppAdmission $requestedLifetimeMs ([long](Get-StageRemaining)*1000)
    $path=Join-Path $script:InstallRoot $Name
    $label="$State-$Name"
    if ($InjectReceiptFailure) { $label='receipt-failure-'+$label }
    if ($ProbeDeadline) { $label='deadline-'+$label }
    $profile=Join-Path $EvidenceDirectory "app-fixture-$label"
    foreach ($directory in 'config','workspace','temp') { $null=[IO.Directory]::CreateDirectory((Join-Path $profile $directory)) }
    # Only explicit nonsecret environment; no credentials/profile enumeration.
    $environment=[ordered]@{
        APPDATA=(Join-Path $profile 'config'); KWIL_NO_UPDATE='1'; KWIL_WORKSPACE_ROOT=(Join-Path $profile 'workspace')
        LOCALAPPDATA=[Environment]::GetFolderPath('LocalApplicationData'); PATH=(Join-Path $env:SystemRoot 'System32')
        SystemRoot=$env:SystemRoot; TEMP=(Join-Path $profile 'temp'); TMP=(Join-Path $profile 'temp'); WINDIR=$env:SystemRoot
    }
    $block=(@($environment.Keys | ForEach-Object { "$_=$($environment[$_])" }) -join [char]0)+[char]0+[char]0
    $si=[KwilLifecycleImage+SI]::new(); $si.cb=[Runtime.InteropServices.Marshal]::SizeOf($si); $si.flags=1; $si.show=0
    $pi=[KwilLifecycleImage+PI]::new()
    $pointer=[IntPtr]::Zero; $pty=$null; $lease=$null; $created=$false
    $primaryFailure=$null; $cleanup=$null; $cleanupFailure=$null
    $diagnosticFailures=[Collections.Generic.List[string]]::new()
    $previousLease=$script:ActiveAppLease
    $appWatch=[Diagnostics.Stopwatch]::new()
    $lifetimeMs=$admission.LifetimeMilliseconds
    $receiptWriter={param([string]$Message) Add-Receipt $Message}
    if ($InjectReceiptFailure) { $receiptWriter={param([string]$Message) throw [IO.IOException]::new('Kwil injected app receipt write failure')} }
    # Scope is established BEFORE either native creation API. No post-creation
    # allocation, receipt, assertion or environment free can bypass this finally.
    try {
        $pointer=[Runtime.InteropServices.Marshal]::StringToHGlobalUni($block)
        $command=$null; $flags=0x08000404
        if ($State -ceq 'active') {
            if ($Name -ceq 'kwil.exe') { $command='"'+$path+'" --hidden --gui --renderer cpu'; $flags=0x08000400 }
            else { $command='"'+$path+'" --hidden --tui' }
        }
        # Recheck AFTER profile/environment preparation, immediately before
        # native creation. Arm uses this same elapsed clock and effective cap.
        $admission=Get-AppAdmission $requestedLifetimeMs ([long](Get-StageRemaining)*1000)
        $lifetimeMs=$admission.LifetimeMilliseconds
        $appWatch.Start()
        if ($State -ceq 'active' -and $Name -ceq 'kwil.com') {
            $outputName=if ($InjectReceiptFailure) { "$label.tui.output.log" } else { 'active-tui.output.log' }
            $pty=[KwilLifecycleImage]::StartPty($path,$command,$pointer,$EvidenceDirectory,(Join-Path $EvidenceDirectory $outputName))
            $pi=$pty.process; $created=$true
        } else {
            $buffer=if ($null -eq $command) { $null } else { [Text.StringBuilder]::new($command) }
            $created=[KwilLifecycleImage]::CreateProcessW($path,$buffer,[IntPtr]::Zero,[IntPtr]::Zero,$false,$flags,$pointer,$EvidenceDirectory,[ref]$si,[ref]$pi)
        }
        Assert-Case $created "cannot create exact owned $label process"
        # In-memory PID/handle ownership and watchdog precede ALL receipt IO.
        $lease=[KwilLifecycleImage+AppLease]::new($pi,$pty,$appWatch)
        $lease.Arm($lifetimeMs)
        $script:ActiveAppLease=$lease
        $null=& $receiptWriter "label=$label pid=$($pi.pid) timeoutMilliseconds=$lifetimeMs hidden=true state=$State"
        if ($ProbeDeadline) {
            $waitMs=Get-AppWait 2000 $lease.RemainingMilliseconds -CleanupObservation
            Assert-Case ([KwilLifecycleImage]::WaitForSingleObject($pi.process,$waitMs) -eq 0) 'app watchdog did not enforce the effective injected deadline'
        } else {
            $waitMs=Get-AppWait 1000 $lease.RemainingMilliseconds
            Assert-Case ([KwilLifecycleImage]::WaitForSingleObject($pi.process,$waitMs) -eq 258) "$label exited before in-use tests"
            foreach ($operation in 'Install','Repair','Uninstall') {
                $packagePath=if ($operation -ceq 'Install') { $HigherPath } else { $OriginalPath }
                $null=Invoke-Msi "$label-$operation" $packagePath -Operation $operation -Refuse -ExpectedDiagnostic 'Kwil installer preflight: in-use'
                Assert-Case ([KwilLifecycleImage]::WaitForSingleObject($pi.process,0) -eq 258) 'application exited or reached its enforced deadline during in-use test'
                Assert-Resources $script:Original
                Assert-Registration $script:Original
                Assert-Preserved
            }
        }
    } catch { $primaryFailure=$_ }
    finally {
        $script:ActiveAppLease=$previousLease
        # Cleanup contains NO receipt IO. Even watchdog-arm/receipt exceptions
        # use the original returned handles; no process lookup or tree kill.
        try {
            if ($null -eq $lease -and ($created -or $null -ne $pty)) {
                if ($null -ne $pty) { $pi=$pty.process }
                $lease=[KwilLifecycleImage+AppLease]::new($pi,$pty,$appWatch)
            }
            if ($null -ne $lease) { $cleanup=$lease.Close() }
        } catch {
            $cleanupFailure=$_
            if ($null -ne $lease) { $cleanup=$lease.Snapshot }
        }
        try { if ($pointer -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($pointer) } }
        catch { $diagnosticFailures.Add('environment release: '+$_.Exception.Message) }
        $diagnostic=[ordered]@{commit=$env:GITHUB_SHA;label=$label;pid=$pi.pid;created=$created;deadlineMilliseconds=$lifetimeMs;admission=$admission
            primaryFailure=$(if ($primaryFailure) { $primaryFailure.Exception.Message } else { '' })
            cleanup=$cleanup;cleanupFailure=$(if ($cleanupFailure) { $cleanupFailure.Exception.Message } else { '' })}
        # Cleanup diagnostics are secondary. Logging cannot skip cleanup or
        # replace an existing receipt/startup/MSI failure with another IO error.
        try { Write-Json (Join-Path $EvidenceDirectory "$label.app-cleanup.json") $diagnostic }
        catch { $diagnosticFailures.Add('cleanup JSON: '+$_.Exception.Message) }
        try { Add-Receipt "label=$label pid=$($pi.pid) cleanupCompleted=$($null -ne $cleanup -and $cleanup.Success) primaryFailure=$($null -ne $primaryFailure)" }
        catch { $diagnosticFailures.Add('cleanup receipt: '+$_.Exception.Message) }
        if ($diagnosticFailures.Count -gt 0) {
            try { [Console]::Error.WriteLine("Kwil app cleanup label=$label pid=$($pi.pid) primary=$($diagnostic.primaryFailure) cleanup=$($cleanup | ConvertTo-Json -Depth 3 -Compress) logging=$($diagnosticFailures -join '; ')") }
            catch { } # failure-reporting IO cannot replace the primary error
        }
    }
    $cleanupOk=$null -ne $cleanup -and $cleanup.Success -and $null -eq $cleanupFailure
    if (-not $cleanupOk) { $script:RetainDrive=$true }
    if ($primaryFailure) {
        try {
            $primaryFailure.Exception.Data['KwilAppCleanup']=$diagnostic
            $primaryFailure.Exception.Data['KwilAppCleanupLogging']=$diagnosticFailures.ToArray()
        } catch { } # even diagnostic attachment cannot replace the primary
        if ($InjectReceiptFailure -and $primaryFailure.Exception.Message -ceq 'Kwil injected app receipt write failure' -and $cleanupOk -and -not $cleanup.deadlineExpired -and $diagnosticFailures.Count -eq 0) {
            # Observable native evidence: process exited BEFORE original thread/
            # process handles closed; ConPTY disposed; original IO error retained.
            $script:Results.Add(@{case=$label;kind='injected-receipt-failure';primaryFailure=$primaryFailure.Exception.Message;cleanup=$cleanup;passed=$true})
            return $diagnostic
        }
        throw $primaryFailure
    }
    Assert-Case (-not $InjectReceiptFailure) 'injected receipt writer did not fail'
    Assert-Case $cleanupOk "owned app cleanup failed: $($diagnostic | ConvertTo-Json -Depth 5 -Compress)"
    Assert-Case ($diagnosticFailures.Count -eq 0) ('app cleanup diagnostic write failed: '+($diagnosticFailures -join '; '))
    Assert-Case ($cleanup.deadlineExpired -eq [bool]$ProbeDeadline) 'app deadline state did not match the case'
    if ($ProbeDeadline) {
        Assert-Case $cleanup.terminationAttempted 'deadline probe did not terminate its exact owned mapped process'
        $script:Results.Add(@{case=$label;kind='enforced-app-deadline';deadlineMilliseconds=$lifetimeMs;cleanup=$cleanup;passed=$true})
    }
    return $diagnostic
}

Assert-Hosted
Assert-BudgetPlan
$script:RetainDrive = $false
if ($Mode -ceq 'Driver') {
    Invoke-Driver
} else {
    # Invalid ownership is refused before even failure-log/status writes.
    $null = Assert-Owner
    $code = 1; $message = 'worker did not complete'
    try { Invoke-Worker; $code=0; $message='completed' }
    catch {
        $message = $_.Exception.Message
        try { [IO.File]::WriteAllText((Join-Path $EvidenceDirectory 'failure.txt'), ($_ | Out-String)) }
        catch { try { [Console]::Error.WriteLine("Kwil worker primary failure: $message; failure-log IO also failed: $($_.Exception.Message)") } catch { } }
        if (Get-Variable Results -Scope Script -ErrorAction SilentlyContinue) {
            try { Write-Json (Join-Path $EvidenceDirectory 'partial-cases.json') $script:Results.ToArray() }
            catch { try { [Console]::Error.WriteLine("Kwil worker primary failure: $message; partial-case log IO also failed: $($_.Exception.Message)") } catch { } }
        }
    } finally {
        # No finally-uninstall: a failed gate retains the exact installed evidence.
        try { Write-Json (Join-Path $EvidenceDirectory 'worker-exit.json') @{code=$code;message=$message;pid=$PID} }
        catch { try { [Console]::Error.WriteLine("Kwil worker exit=$code primary=$message; exit-log IO also failed: $($_.Exception.Message)") } catch { } }
    }
    exit $code
}
