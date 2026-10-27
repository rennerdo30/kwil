#requires -Version 7.4
<#
INERT controls. No provisioning, credential, token, process, user-object read/write,
service/profile/registry operation or workflow dispatch is invoked here.
Source/Protocol do NOT compile C#. NativePolicy compiles the fixture for ROOT's
separate permission, but invokes only pure readers and fake native-effect boundaries.
The source author did not execute any selector.
#>
[CmdletBinding()]
param([ValidateSet('Source','Protocol','NativePolicy','All')][string]$Selector='Source')
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'
$bootstrapPath=Join-Path $PSScriptRoot 'ci-ordinary-user-bootstrap.ps1'
$sessionPath=Join-Path $PSScriptRoot 'ci-ordinary-user-session.ps1'
$controlsPath=$PSCommandPath
$workflowPath=Join-Path (Split-Path $PSScriptRoot -Parent) '.github/workflows/ordinary-user-proof.yml'
. $bootstrapPath -LibraryOnly
$script:Checks=0
function Check([bool]$Value,[string]$Name) {
    if(-not $Value) { throw "ordinary-user-control:$Name" }
    $script:Checks++
}
function Refuses([scriptblock]$Action,[string]$Name) {
    $refused=$false
    try { & $Action | Out-Null } catch { $refused=$true }
    Check $refused $Name
}
function Test-SourceControls {
    $boot=[IO.File]::ReadAllText($bootstrapPath)
    $session=[IO.File]::ReadAllText($sessionPath)
    $native=Get-OUNativeSource
    $workflow=[IO.File]::ReadAllText($workflowPath)
    Check ($native.Contains('StationGrant=0x22, DesktopGrant=0x83') -and $native.Contains('StationOpen=0x60002, DesktopOpen=0x60081')) 'fixed-specific-masks'
    Check ($native.Contains('CreationFlags=0x08000404')) 'fixed-flags'
    Check ($native.Contains('public IntPtr reserved,desktop,title')) 'desktop-pointer-ABI'
    Check ($native.Contains('CreateProcessWithLogonW(string user,string domain,IntPtr password')) 'pointer-password-ABI'
    Check ($native.Contains('CryptographicOperations.ZeroMemory(new Span<byte>((void*)p,Bytes))')) 'valid-secure-zero-primitive'
    Check ($native.IndexOf('CryptographicOperations.ZeroMemory') -lt $native.IndexOf('Marshal.FreeHGlobal(p)')) 'zero-before-free'
    Check (-not ($native -match 'PtrToStringUni\(pointer|PtrToStringBSTR|SecureString|RtlSecureZeroMemory|NetUserDel|LoadUserProfile|UnloadUserProfile|DeleteProfile|AdjustToken|SetTokenInformation|SetSecurityInfo')) 'no-secret-string-or-policy-fallback'
    Check (([regex]::Matches($native,'SetUserObjectSecurity\(')).Count -eq 2) 'one-DACL-setter-callsite-plus-declaration'
    Check ($native.Contains('public const uint DaclWriteInformation=4;') -and $native.Contains('uint information=DaclWriteInformation;') -and $native.Contains('Need(information==4,"dacl-only-write-information")')) 'DACL-only-write-flags'
    Check ($native.Contains('byte[] check=Read(7)') -and $native.Contains('Original=ParseDacl(Read(4))')) 'owner-group-read-separated-from-D0'
    Check ($native.Contains('access-owner-group-present') -and $native.Contains('ValidateAccessDescriptor(check,Expected,sid)')) 'valid-owner-group-required-before-AccessCheck'
    Check ($native.Contains('new[]{station?StationGrant:DesktopGrant,0x40000U,0x80000U,0x10000U}')) 'forbidden-authority-checks'
    Check ($native.Contains('public const int Characters=64, Bytes=130')) 'bounded-native-secret'
    Check ($native.Contains('flags=0x201') -and $native.Contains('privilege=1')) 'normal-account-no-group-shortcut'
    Check ($native.Contains('value.basic.flags=0x2008') -and $native.Contains('value.basic.active=8')) 'no-breakaway-job'
    Check ($native.Contains('ref si,out lease.pi') -and $native.Contains('lease.Publish(); // PI is owned before zeroing')) 'direct-out-owner-before-fallible-work'
    Check ($native.Contains('lease.ApiEntered=true') -and $native.Contains('Volatile.Write(ref apiReturned,1)')) 'native-inflight-publication'
    Check ($native.Contains('if(Volatile.Read(ref apiReturned)==0) { LaunchAmbiguous=ApiEntered; return; }')) 'inflight-not-childless'
    Check ($native.Contains('if(desktop!=IntPtr.Zero) Marshal.FreeHGlobal(desktop); Dispose();')) 'credential-finally'
    Check ($native.Contains('false,CreationFlags,IntPtr.Zero,cwd') -and $native.Contains('pointer,1,app,new StringBuilder(command),CreationFlags,IntPtr.Zero,cwd')) 'noninheritance-profile-environment'
    Check ($boot.Contains('PipeOptions]::CurrentUserOnly') -and $boot.Contains('$wire.Peer($true) -eq $helper.Pid')) 'kernel-peer-bound-private-IPC'
    Check ($boot.IndexOf('$job.Assign($helper)') -lt $boot.IndexOf('$helper.Resume()')) 'helper-owned-before-resume'
    Check ($boot.IndexOf("Read-OUFrame "+'$wire '+"'owned'") -lt $boot.IndexOf('[KwilOrdinary+Credential]::new')) 'owned-admission-before-secret'
    Check ($boot.IndexOf('$job.Assign($controller)') -lt $boot.IndexOf("Send-OUFrame "+'$wire '+"'assigned'")) 'broker-job-before-resume-ack'
    Check ($boot.Contains('$apiEntered=$true; $ambiguous=$true; $controllerStopped=$false')) 'grant-admission-loss-is-ambiguous'
    Check ($boot.Contains('Test-OUCanRestore $ambiguous $helperStopped $controllerStopped $jobEmpty $planReady')) 'ambiguity-gates-restoration'
    Check ($boot.Contains('Get-OUHash') -and $boot.Contains('KWIL_BOOTSTRAP_SHA256') -and $boot.Contains('GITHUB_WORKFLOW_SHA')) 'source-provenance-gates'
    Check ($boot.Contains("fullLifecycleAccepted="+ '$false') -and $session.Contains("installerQualification='not-run'")) 'not-MSI-qualification'
    Check ($session.Contains('[KwilOrdinary]::Same($Before,$pending)') -and $session.Contains('[KwilOrdinary]::Same($Before,$after)')) 'B-only-same-identity'
    Check ($session.Contains('TickCount64+20000') -and $session.Contains('workerNaturalExit=$true')) 'one-20-second-worker-natural-exit'
    Check ($session.IndexOf('$worker.Close()') -lt $session.IndexOf('$after=[KwilOrdinary]::Current($true)')) 'profile-anchor-through-reap'
    Check ($workflow -match '(?m)^on:\s*\r?\n  workflow_dispatch:' -and $workflow -notmatch '(?m)^  (push|pull_request|workflow_run|workflow_call|schedule):') 'manual-only-trigger'
    Check ($workflow.Contains('contents: read') -and $workflow.Contains('persist-credentials: false') -and $workflow.Contains('timeout-minutes: 15')) 'workflow-least-privilege-and-budget'
    foreach($name in @('ci-ordinary-user-bootstrap.ps1','ci-ordinary-user-session.ps1','ordinary-user-proof.yml','test-ci-ordinary-user.ps1')) { Check ($name -notmatch 'windows') ('old-glob-excludes-'+$name) }
    foreach($pair in @(@('BOOTSTRAP',$bootstrapPath),@('SESSION',$sessionPath),@('CONTROLS',$controlsPath))) {
        $expected=Get-OUHash $pair[1]
        Check ($workflow.Contains('KWIL_'+$pair[0]+'_SHA256: '+$expected)) ('workflow-pin-'+$pair[0])
    }
}
function Test-ProtocolControls {
    Check (Test-OUCanRestore $false $true $true $true $true) 'quiescent-restore-admitted'
    foreach($v in @(
        @($true,$true,$true,$true,$true), # in-flight + empty job cannot prove quiescence
        @($false,$false,$true,$true,$true),
        @($false,$true,$false,$true,$true),
        @($false,$true,$true,$false,$true),
        @($false,$true,$true,$true,$false)
    )) { Check (-not (Test-OUCanRestore @v)) 'incomplete-restore-refused' }
    Check (Test-OUEnvironmentSuccess $true '' $true $true $false $true) 'complete-proof-admitted'
    foreach($v in @(
        @($false,'',$true,$true,$false,$true),
        @($true,'failed',$true,$true,$false,$true),
        @($true,'',$false,$true,$false,$true),
        @($true,'',$true,$false,$false,$true),
        @($true,'',$true,$true,$true,$true),
        @($true,'',$true,$true,$false,$false)
    )) { Check (-not (Test-OUEnvironmentSuccess @v)) 'incomplete-proof-refused' }
    $fields=@('schema','kind')
    $valid=ConvertFrom-OUObject '{"schema":1,"kind":"owned"}' $fields
    Check ($valid.schema -is [long] -and $valid.kind -ceq 'owned') 'strict-integer-acceptance'
    foreach($bad in @('"1"','"text"','false','true','null','{}','[]','0','2','1.0','1e0')) {
        Refuses { ConvertFrom-OUObject ('{"schema":'+$bad+',"kind":"owned"}') $fields } ('schema-refusal-'+$bad)
    }
    foreach($bad in @('{"schema":1,"kind":"a","kind":"b"}','{"schema":1,"kind":"a","ki\u006ed":"b"}','{"schema":1,"kind":false}','{"schema":1,"kind":"a","extra":"x"}')) {
        Refuses { ConvertFrom-OUObject $bad $fields } 'wire-refuses-malformed'
    }
    # Non-native ownership/secret fault model. No credential/buffer/process exists.
    # Every listed injected failure must prevent success and any ambiguous restore.
    foreach($phase in @('before-owned','lost-account-SID','interrupted-logon','late-return','partial-transfer','assignment-failed','live-profile-anchor','extra-child','missing-actual-receipt','nonzero-exit','deadline-expired','close-failed','foreign-DACL')) {
        $unknown=$phase -in @('lost-account-SID','interrupted-logon','late-return','partial-transfer')
        Check (-not (Test-OUEnvironmentSuccess $false $phase $false $false $unknown $false)) ('fault-model-'+$phase)
        if($unknown) { Check (-not (Test-OUCanRestore $true $true $true $true $true)) ('no-empty-job-inference-'+$phase) }
    }
}
function New-ControlDescriptor([string]$Sid='S-1-5-18',[uint32]$Mask=0x20000,[byte]$Flags=0) {
    $id=[Security.Principal.SecurityIdentifier]::new($Sid)
    $bytes=[byte[]]::new($id.BinaryLength); $id.GetBinaryForm($bytes,0)
    $aceSize=8+$bytes.Length; $aclSize=8+$aceSize
    $sd=[byte[]]::new(20+$aclSize)
    $sd[0]=1; [BitConverter]::GetBytes([uint16]0x8004).CopyTo($sd,2)
    [BitConverter]::GetBytes([uint32]20).CopyTo($sd,16)
    $sd[20]=2; [BitConverter]::GetBytes([uint16]$aclSize).CopyTo($sd,22); [BitConverter]::GetBytes([uint16]1).CopyTo($sd,24)
    $sd[29]=$Flags; [BitConverter]::GetBytes([uint16]$aceSize).CopyTo($sd,30)
    [BitConverter]::GetBytes($Mask).CopyTo($sd,32); $bytes.CopyTo($sd,36)
    return ,$sd
}
function Test-NativePureControls {
    # ROOT-only compile/inert selector. NEVER invokes any P/Invoke method.
    Initialize-OUNative
    Check ([KwilOrdinary]::CredentialEffectControls() -eq 12) 'live-credential-failure-effects'
    Check ([KwilOrdinary]::LeaseEffectControls() -eq 20) 'live-lease-failure-effects'
    Check ([KwilOrdinary]::MeasurementAndDeadlineControls() -eq 10) 'live-used-measurement-and-deadline-boundaries'
    $b='S-1-5-21-100-200-300-1001'
    $raw=New-ControlDescriptor
    $d0=[KwilOrdinary]::ParseDacl($raw)
    foreach($v in @(@([uint32]0x22,$true),@([uint32]0x83,$false))) {
        $e=[KwilOrdinary]::Delta($d0,$b,$v[0],$v[1])
        Check ([KwilOrdinary]::AssignmentAndPairControls($d0,$e) -eq 9) 'live-job-and-two-object-effects'
        # Independent byte expectation: existing SYSTEM ACE stays first;
        # exact zero-flags B allow is appended, not replaced or reordered.
        $id=[Security.Principal.SecurityIdentifier]::new($b)
        $sidBytes=[byte[]]::new($id.BinaryLength); $id.GetBinaryForm($sidBytes,0)
        $add=8+$sidBytes.Length; $expected=[byte[]]::new($raw.Length+$add); $raw.CopyTo($expected,0)
        [BitConverter]::GetBytes([uint16]($expected.Length-20)).CopyTo($expected,22)
        [BitConverter]::GetBytes([uint16]2).CopyTo($expected,24)
        $at=$raw.Length; [BitConverter]::GetBytes([uint16]$add).CopyTo($expected,$at+2); [BitConverter]::GetBytes([uint32]$v[0]).CopyTo($expected,$at+4); $sidBytes.CopyTo($expected,$at+8)
        $want=[KwilOrdinary]::ParseDacl($expected)
        Check ([KwilOrdinary+DaclState]::Equal($e,$want)) 'independent-exact-delta'
        Check ([KwilOrdinary]::RestoreDecision($d0,$d0,$e) -ceq 'original') 'restore-D0'
        Check ([KwilOrdinary]::RestoreDecision($e,$d0,$e) -ceq 'expected') 'restore-E'
        $other=[KwilOrdinary]::ParseDacl((New-ControlDescriptor -Mask 0x20001))
        Check ([KwilOrdinary]::RestoreDecision($other,$d0,$e) -ceq 'foreign') 'foreign-third-state-no-restore'
        Check ([KwilOrdinary]::DaclEffectControls($d0,$e,$other) -eq 9) 'live-install-restore-effects'
        Refuses { [KwilOrdinary]::Delta($e,$b,$v[0],$v[1]) } 'preexisting-B-refused'
        Refuses { [KwilOrdinary]::Delta($d0,$b,0x10000000,$v[1]) } 'generic-grant-refused'
    }
    Refuses { [KwilOrdinary]::ValidateAccessDescriptor($raw,$d0,$b) } 'owner-group-required-not-synthesized'
    $owner=[Security.Principal.SecurityIdentifier]::new('S-1-5-18')
    $os=[byte[]]::new($owner.BinaryLength); $owner.GetBinaryForm($os,0)
    $full=[byte[]]::new($raw.Length+2*$os.Length); $raw.CopyTo($full,0)
    [BitConverter]::GetBytes([uint32]$raw.Length).CopyTo($full,4)
    [BitConverter]::GetBytes([uint32]($raw.Length+$os.Length)).CopyTo($full,8)
    $os.CopyTo($full,$raw.Length); $os.CopyTo($full,$raw.Length+$os.Length)
    [KwilOrdinary]::ValidateAccessDescriptor($full,$d0,$b); Check $true 'valid-private-owner-group-reader'
    $missing=[byte[]]$full.Clone(); [Array]::Clear($missing,8,4)
    Refuses { [KwilOrdinary]::ValidateAccessDescriptor($missing,$d0,$b) } 'missing-group-refused'
    $invalid=[byte[]]$full.Clone(); $invalid[$raw.Length]=0
    Refuses { [KwilOrdinary]::ValidateAccessDescriptor($invalid,$d0,$b) } 'invalid-owner-refused'
    $oversize=[byte[]]::new(65537)
    Refuses { [KwilOrdinary]::ParseDacl($oversize) } 'oversized-descriptor-refused'
    $unsupported=[byte[]]$raw.Clone(); $unsupported[20]=4
    Refuses { [KwilOrdinary]::ParseDacl($unsupported) } 'unsupported-ACL-revision-refused'
    $badAce=[byte[]]$raw.Clone(); $badAce[29]=0x80
    Refuses { [KwilOrdinary]::ParseDacl($badAce) } 'unsupported-ACE-flags-refused'
    $facts=[KwilOrdinary+Facts]::new()
    $facts.UserSid=$b; $facts.AuthenticationId='00000000:12345678'; $facts.LogonSid='S-1-5-5-0-1234'; $facts.SessionId=2
    $facts.TokenType=1; $facts.TokenElevationType=1; $facts.IntegritySid='S-1-16-8192'
    [KwilOrdinary]::Ordinary($facts); Check $true 'ordinary-Medium-baseline'
    foreach($field in @('AdminSidPresent','EffectiveAdmin','PowerfulPrivilegePresent')) {
        $facts.$field=$true; Refuses { [KwilOrdinary]::Ordinary($facts) } ('ordinary-refuses-'+$field); $facts.$field=$false
    }
    $facts.IntegritySid='S-1-16-12288'; Refuses { [KwilOrdinary]::Ordinary($facts) } 'High-IL-not-ordinary'; $facts.IntegritySid='S-1-16-8192'
    foreach($field in @('UserSid','AuthenticationId','LogonSid','IntegritySid','SessionId','TokenType','TokenElevation','TokenElevationType')) {
        $other=[KwilOrdinary+Facts]::new()
        foreach($f in $facts.GetType().GetFields()) { $f.SetValue($other,$f.GetValue($facts)) }
        if($field -in @('SessionId','TokenElevation','TokenElevationType')) { $other.$field=[uint32]($facts.$field+1) }
        elseif($field -eq 'TokenType') { $other.$field=2 }
        else { $other.$field=$facts.$field+'-mismatch' }
        Refuses { [KwilOrdinary]::Same($facts,$other) } ('exact-identity-mismatch-'+$field)
    }
    Check ([KwilOrdinary]::DaclWriteInformation -eq 4) 'exact-runtime-DACL-write-information-constant'
}
function Test-ActualReceiptControls {
    # Exercise live readers with bounded local, nonsecret fixture data.
    $spec=[pscustomobject]@{base=('a'*40);commit=('b'*40);run='42';architecture='x64';nonce=('c'*32);bootstrapHash=('d'*64);sessionHash=('e'*64);workflowHash=('f'*64)}
    $wire=[pscustomobject]@{Text='';Writes=0;FailWrite=$false}
    $wire | Add-Member ScriptMethod Read {param($ms) if($ms -le 0){throw 'expired'};return $this.Text}
    $wire | Add-Member ScriptMethod Write {param($text,$ms) $this.Writes++;if($this.FailWrite){throw 'mock-write'};$this.Text=$text}
    $end=[Environment]::TickCount64+10000
    Send-OUFrame $wire 'assigned' $spec ([ordered]@{pid='5';created='42'}) $end
    $frame=Read-OUFrame $wire 'assigned' $spec @('pid','created') $end
    Check ($frame.pid -ceq '5' -and $frame.created -ceq '42' -and $wire.Writes -eq 1) 'live-frame-roundtrip'
    $wire.Text='{"kind":"assigned","nonce":"wrong","pid":"5","created":"42"}'
    Refuses {Read-OUFrame $wire 'assigned' $spec @('pid','created') $end} 'live-lost-or-foreign-assignment-ack'
    $wire.FailWrite=$true
    Refuses {Send-OUFrame $wire 'account' $spec ([ordered]@{sid='S-1-5-21-1-2-3-1001';created='true'}) $end} 'live-account-SID-receipt-failure'
    $wire.FailWrite=$false
    $wire.Text='{"kind":"assigned","nonce":"'+$spec.nonce+'","pid":"5"}'
    Refuses {Read-OUFrame $wire 'assigned' $spec @('pid','created') $end} 'live-missing-ack-owner'
    $dir=Join-Path ([IO.Path]::GetTempPath()) ('kwil-inert-receipts-'+[Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($dir)
    $path=Join-Path $dir 'proof.json'
    $proof=[ordered]@{schema=1;purpose='OrdinaryUserSessionProof';base=$spec.base;kind='worker';status='ready-natural-exit';pid=7;created=43;commit=$spec.commit;run=$spec.run;attempt='1';architecture=$spec.architecture;nonce=$spec.nonce;bootstrapHash=$spec.bootstrapHash;sessionHash=$spec.sessionHash;workflowHash=$spec.workflowHash;fullLifecycleAccepted=$false;installerQualification='not-run'}
    try {
        [IO.File]::WriteAllText($path,($proof|ConvertTo-Json -Compress))
        $actual=Read-OUProof $path $spec 'worker'
        Check ($actual.pid -eq 7 -and $actual.created -eq 43) 'live-proof-reader-positive'
        foreach($field in @('purpose','base','kind','status','commit','run','attempt','architecture','nonce','bootstrapHash','sessionHash','workflowHash')) {
            $old=$proof[$field];$proof[$field]='wrong'
            [IO.File]::WriteAllText($path,($proof|ConvertTo-Json -Compress))
            Refuses {Read-OUProof $path $spec 'worker'} ('live-proof-foreign-'+$field)
            $proof[$field]=$old
        }
        foreach($field in @('schema','pid','created','fullLifecycleAccepted','installerQualification')) {
            $old=$proof[$field];$proof.Remove($field)
            [IO.File]::WriteAllText($path,($proof|ConvertTo-Json -Compress))
            Refuses {Read-OUProof $path $spec 'worker'} ('live-proof-missing-'+$field)
            $proof[$field]=$old
        }
    } finally {
        if([IO.File]::Exists($path)){[IO.File]::Delete($path)}
        [IO.Directory]::Delete($dir,$false)
    }
    $cp=[pscustomobject]@{pid=5;created=42;supervisorPid=10;workerPid=7;workerCreated=43}
    $wp=[pscustomobject]@{pid=7;created=43;controllerPid=5;controllerCreated=42}
    $transfer=[pscustomobject]@{pid='5';created='42'}
    Assert-OUActualReceipts $cp $wp $transfer 10
    Check $true 'live-owned-creation-chain'
    foreach($object in @($cp,$wp)) {
        foreach($property in $object.PSObject.Properties) {
            $old=$property.Value;$property.Value=$old+1
            Refuses {Assert-OUActualReceipts $cp $wp $transfer 10} ('live-owned-mismatch-'+$property.Name)
            $property.Value=$old
        }
    }
    $facts=[KwilOrdinary+Facts]::new()
    $facts.UserSid='S-1-5-21-1-2-3-1001';$facts.AuthenticationId='00000000:00000001';$facts.LogonSid='S-1-5-5-0-1';$facts.IntegritySid='S-1-16-8192'
    $facts.SessionId=2;$facts.TokenType=1;$facts.TokenElevationType=1
    $facts.Profile='E:\fixture\profile';$facts.Local='E:\fixture\profile\local';$facts.Programs='E:\fixture\profile\programs'
    $facts.HkcuLoaded=$true;$facts.Station='WinSta0';$facts.Desktop='Default'
    $observed=$facts|ConvertTo-Json|ConvertFrom-Json
    Assert-OUFactsJson $facts $observed -Profile
    Check $true 'live-profile-facts-positive'
    foreach($property in $observed.PSObject.Properties) {
        $old=$property.Value
        if($old -is [bool]){$property.Value=-not $old}elseif($old -is [string]){$property.Value=$old+'-wrong'}else{$property.Value=$old+1}
        Refuses {Assert-OUFactsJson $facts $observed -Profile} ('live-profile-fact-mismatch-'+$property.Name)
        $property.Value=$old
    }
}
if($Selector -in @('Source','All')) { Test-SourceControls }
if($Selector -in @('Protocol','All')) { Test-ProtocolControls }
if($Selector -in @('NativePolicy','All')) { Test-NativePureControls; Test-ActualReceiptControls }
Write-Host ("OrdinaryUser inert selector={0} checks={1}; no provisioning/native proof; installerQualification=not-run" -f $Selector,$script:Checks)
