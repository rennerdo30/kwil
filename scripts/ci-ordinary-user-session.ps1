#requires -Version 7.4
<#
Ordinary B proof only: C is the complete profile anchor, W is its one normal
CreateProcessW child. No account, credential, desktop write, MSI or fallback here.
All operative entry points require the protected fixed source/spec and nonce.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Controller','Worker')][string]$Role,
    [Parameter(Mandatory)][string]$SpecPath,
    [Parameter(Mandatory)][long]$Deadline,
    [uint32]$ControllerPid=0,
    [long]$ControllerCreated=0
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'
# The shared library declares functions/C# TEXT only. -LibraryOnly never Add-Type.
$sessionRole=$Role; $sessionPath=$PSCommandPath; $sessionSpecPath=$SpecPath
$sessionDeadline=$Deadline; $expectedControllerPid=$ControllerPid; $expectedControllerCreated=$ControllerCreated
$bootstrap=Join-Path $PSScriptRoot 'ci-ordinary-user-bootstrap.ps1'
. $bootstrap -LibraryOnly
$Role=$sessionRole; $SpecPath=$sessionSpecPath; $Deadline=$sessionDeadline
$ControllerPid=$expectedControllerPid; $ControllerCreated=$expectedControllerCreated
function Get-OUReceipt([string]$Kind,$Spec,$Facts) {
    [ordered]@{
        schema=1;purpose='OrdinaryUserSessionProof';kind=$Kind;status='ready-natural-exit'
        base=$Spec.base;commit=$Spec.commit;run=$Spec.run;attempt='1';architecture=$Spec.architecture;nonce=$Spec.nonce
        bootstrapHash=$Spec.bootstrapHash;sessionHash=$Spec.sessionHash;workflowHash=$Spec.workflowHash
        pid=[uint32]$PID;created=[KwilOrdinary]::CurrentCreated();facts=$Facts
        installerQualification='not-run';fullLifecycleAccepted=$false
    }
}
function Assert-OUBContext($Spec) {
    Assert-OU ($Spec.sid -cmatch '\AS-1-5-21-[0-9]+-[0-9]+-[0-9]+-[1-9][0-9]*\z' -and -not $Spec.sid.EndsWith('-500')) 'session-created-sid'
    Assert-OU ($Deadline -gt [Environment]::TickCount64 -and $Deadline -le [Environment]::TickCount64+90000) 'session-deadline'
    $arch=[Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString().ToLowerInvariant()
    Assert-OU ($arch -ceq $Spec.architecture -and [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant() -ceq $arch) 'session-native-architecture'
    Assert-OU ([IO.Path]::GetFullPath($sessionPath) -ceq (Join-Path $Spec.root 'source/ci-ordinary-user-session.ps1')) 'session-source-path'
    Assert-OUPrivateSource $Spec $bootstrap
    # lpEnvironment=NULL at the one profile logon supplies B's OS profile
    # environment. Reject any runner control/credential environment propagation.
    foreach($item in [Environment]::GetEnvironmentVariables().Keys) {
        Assert-OU ([string]$item -notmatch '\A(?:GITHUB_|ACTIONS_|RUNNER_|KWIL_)') 'runner-environment-in-B'
    }
    $facts=[KwilOrdinary]::Current($true)
    Assert-OUProfileFacts $facts
    Assert-OU ($facts.UserSid -ceq $Spec.sid -and [IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\') -ceq $facts.Profile -and [IO.Path]::GetFullPath($env:LOCALAPPDATA).TrimEnd('\') -ceq $facts.Local) 'session-profile-environment'
    return $facts
}
function Invoke-OUController($Spec,$Before) {
    Assert-OU ($ControllerPid -eq 0 -and $ControllerCreated -eq 0) 'controller-not-worker'
    $worker=$null; $ok=$false
    try {
        [void](Get-OURemaining $Deadline 30000)
        $wEnd=[Math]::Min([Environment]::TickCount64+20000,$Deadline-30000)
        $app=Join-Path $PSHOME 'pwsh.exe'
        $command=New-OUCommand $app $sessionPath ('-Role Worker -SpecPath "'+$SpecPath+'" -Deadline '+$wEnd+' -ControllerPid '+$PID+' -ControllerCreated '+[KwilOrdinary]::CurrentCreated())
        try { $worker=[KwilOrdinary]::SameLaunch($app,$command,(Join-Path $Spec.root 'data'),$wEnd,20000) }
        finally { if($null -eq $worker) { $worker=[KwilOrdinary]::PendingSame } }
        $pending=$worker.Suspended(); [KwilOrdinary]::Ordinary($pending)
        [KwilOrdinary]::Same($Before,$pending)
        foreach($name in @('Profile','Local','Programs')) { Assert-OU ($Before.$name -ceq $pending.$name) 'suspended-worker-profile' }
        # Same native token and same profile were proved BEFORE this single resume.
        $worker.Resume()
        [void]$worker.WaitNatural()
        $closed=$worker.Close()
        Assert-OU ($closed.Success -and $closed.ExitCode -eq 0 -and -not $closed.DeadlineExpired -and -not $closed.TerminationAttempted) 'worker-natural-owned-cleanup'
        $proof=Read-OUProof (Join-Path $Spec.root 'data/worker.json') $Spec 'worker'
        Assert-OU ($proof.pid -eq $worker.Pid -and $proof.controllerPid -eq $PID -and $proof.controllerCreated -eq [KwilOrdinary]::CurrentCreated()) 'worker-owned-receipt-pids'
        Assert-OUFactsJson $pending $proof.facts -Profile
        $after=[KwilOrdinary]::Current($true)
        Assert-OUProfileFacts $after
        [KwilOrdinary]::Same($Before,$after)
        foreach($name in @('Profile','Local','Programs')) { Assert-OU ($Before.$name -ceq $after.$name) 'profile-anchor-continuity' }
        [void](Get-OURemaining $Deadline)
        $receipt=Get-OUReceipt 'controller' $Spec $after
        $receipt.supervisorPid=[uint32]$Spec.supervisorPid
        $receipt.workerPid=[uint32]$proof.pid; $receipt.workerCreated=[long]$proof.created
        $receipt.workerSuspendedFacts=$pending; $receipt.workerActualFacts=$proof.facts
        $receipt.workerNaturalExit=$true; $receipt.workerHandlesClosed=$true; $receipt.profileContinuous=$true
        # C is STILL RUNNING, holding the OS logon/profile lifetime, while W is
        # completely reaped and these final HKCU/known-folder observations occur.
        Write-OUJson (Join-Path $Spec.root 'data/controller.json') $receipt
        $ok=$true
    } finally {
        if($null -ne $worker) {
            $cleanup=$worker.Close()
            if(-not $cleanup.Success) { $ok=$false }
        }
    }
    Assert-OU $ok 'controller-incomplete'
}
try {
    Assert-OU ($IsWindows -and [IO.Path]::IsPathFullyQualified($SpecPath)) 'session-platform'
    $spec=Read-OUSpec $SpecPath
    Initialize-OUNative
    $before=Assert-OUBContext $spec
    if($Role -ceq 'Controller') {
        Invoke-OUController $spec $before
    } else {
        Assert-OU ($ControllerPid -gt 0 -and $ControllerCreated -gt 0 -and $ControllerPid -ne $PID) 'worker-parent'
        Assert-OU ($Deadline -le [Environment]::TickCount64+20000) 'worker-twenty-second-cap'
        [KwilOrdinary]::VerifyProcess($ControllerPid,$ControllerCreated)
        [void](Get-OURemaining $Deadline)
        $receipt=Get-OUReceipt 'worker' $spec $before
        $receipt.controllerPid=$ControllerPid; $receipt.controllerCreated=$ControllerCreated
        Write-OUJson (Join-Path $spec.root 'data/worker.json') $receipt
    }
} catch {
    # No exception text, environment, descriptor or arguments enter the receipt.
    exit 1
}
# This is a real natural process exit. Parent-owned handles measure the exit code.
exit 0
