#requires -Version 7.2
<#
Authored source fixtures; NOT RUN by the implementation job.

ROOT entry points (run only with execution admission):
  pwsh -NoLogo -NoProfile -File scripts/test-windows-msi.ps1
  pwsh -NoLogo -NoProfile -File scripts/test-windows-msi.ps1 -PackagingRoot E:/Development/kwil/fixtures

Default tests use memory-only JSON/PE/CAB/identity fixtures; no COM, native DLL
loading, child launch or installation. PackagingRoot explicitly adds Windows MSI
database authoring/readback and makecab/expand on tiny synthetic PE fixtures.
Those fixtures are NOT runnable guards or editor builds. Never install them.
All packaging fixtures/logs are retained in an exclusive directory for ROOT.
There is no msiexec, registry/profile access, network, app launch or lifecycle
test here. Hosted native x64 AND ARM64 lifecycle acceptance is separate.
#>
[CmdletBinding()]
param([string]$PackagingRoot)

$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
. ([IO.Path]::Combine($PSScriptRoot,'windows-msi.ps1'))
$script:KwilTestCount=0

function Test-KwilEqual {
    param($Actual,$Expected,[string]$Reason)
    # Array -ceq filters rather than yielding one bool. Report a polluted result
    # directly instead of masking the original failure with bool conversion.
    Assert-Kwil ($Actual -isnot [array] -and $Expected -isnot [array]) "fixture $Reason requires scalar results; unexpected success-pipeline array"
    Assert-Kwil ($Actual -ceq $Expected) "fixture $Reason; actual=$Actual expected=$Expected"
    $script:KwilTestCount++
}

function Test-KwilReject {
    param([scriptblock]$Action,[string]$ReasonPattern)
    $caught=$false
    try{& $Action | Out-Null}catch{
        $caught=$true
        Assert-Kwil ($_.Exception.Message -match $ReasonPattern) "fixture failed for unexpected reason: $($_.Exception.Message)"
    }
    Assert-Kwil $caught "fixture accepted invalid input ($ReasonPattern)"
    $script:KwilTestCount++
}

function Test-KwilQueryOutputShape {
    # Memory-only regression for PowerShell success-stream semantics. The field
    # accessor override is LOCAL to this function and expires on return. The
    # PackagingRoot branch still authors and inspects a real COM MSI database.
    function Get-KwilComField {
        param($Record,[int]$Index,[bool]$Integer=$false)
        return $Record.Cells[$Index-1]
    }
    foreach($count in @(0,1,2)) {
        $view=[pscustomobject]@{Records=[Collections.Generic.Queue[object]]::new();CloseCalls=0}
        if($count -ge 1){$view.Records.Enqueue([pscustomobject]@{Cells=[object[]]@($null,7,'last')})}
        if($count -ge 2){$view.Records.Enqueue([pscustomobject]@{Cells=[object[]]@('first',$null,$null)})}
        $view | Add-Member -MemberType ScriptMethod -Name Execute -Value {param($Record);return $null,73}
        $view | Add-Member -MemberType ScriptMethod -Name Close -Value {$this.CloseCalls++;return $null,'ignored-close-result'}
        $view | Add-Member -MemberType ScriptMethod -Name Fetch -Value {if($this.Records.Count -eq 0){return $null};return $this.Records.Dequeue()}
        $database=[pscustomobject]@{View=$view}
        $database | Add-Member -MemberType ScriptMethod -Name OpenView -Value {param($Sql);return $this.View}
        $result=Read-KwilQuery $database 'fixture-only' @($false,$true,$false) 2
        Test-KwilEqual ($result -is [object[]]) $true 'query returns one outer row collection'
        Test-KwilEqual $result.Count $count 'query discards Execute/Close results, including for zero rows'
        Test-KwilEqual $view.CloseCalls 1 'query still closes its view'
        if($count -ge 1) {
            Test-KwilEqual $result[0].Count 3 'nullable first cell retains row width'
            Test-KwilEqual $result[0][0] $null 'nullable first cell remains null'
            Test-KwilEqual $result[0][1] 7 'integer column stays in position'
            Test-KwilEqual $result[0][2] 'last' 'last cell stays in position'
        }
        if($count -ge 2) {
            Test-KwilEqual $result[1].Count 3 'nullable trailing cells retain row width'
            Test-KwilEqual $result[1][0] 'first' 'second row remains distinct'
            Test-KwilEqual $result[1][1] $null 'middle null remains null'
            Test-KwilEqual $result[1][2] $null 'trailing null remains null'
        }
        $procedureOutput=@(Invoke-KwilSql $database 'fixture-only')
        Test-KwilEqual $procedureOutput.Count 0 'SQL procedure emits no data, even from cleanup'
        Test-KwilEqual $view.CloseCalls 2 'SQL procedure still closes its view'
    }
}

function Test-KwilReceiptFailureCleanup {
    # Injected process/clock objects only; no OS process is launched or killed.
    foreach($case in @([pscustomobject]@{Expired=$false;Failure='all'},[pscustomobject]@{Expired=$true;Failure='all'},[pscustomobject]@{Expired=$true;Failure='timeout'})) {
        $context=[pscustomobject]@{Watch=[pscustomobject]@{ElapsedMilliseconds=0};Failure=$case.Failure;Messages=[Collections.Generic.List[string]]::new()}
        $process=[pscustomobject]@{Id=424242;ExitCode=0;NormalExit=(-not $case.Expired);KillCalls=0;KillArgumentCount=0;Waits=[Collections.Generic.List[int]]::new()}
        $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
            param([int]$Milliseconds)
            $this.Waits.Add($Milliseconds)
            if($Milliseconds -eq 1000){return $true}
            return $this.NormalExit
        }
        $process | Add-Member -MemberType ScriptMethod -Name Kill -Value {$this.KillCalls++;$this.KillArgumentCount=$args.Count}
        $timer=[pscustomobject]@{ElapsedMilliseconds=$(if($case.Expired){60000}else{0})}
        $writer={
            param($ReceiptContext,$Message)
            $ReceiptContext.Messages.Add($Message)
            if($ReceiptContext.Failure -ceq 'all' -or $Message.Contains('timeout=true')){throw [IO.IOException]::new('injected receipt failure')}
        }
        $reason=if($case.Expired){'timed out'}else{'receipt write failed'}
        Test-KwilReject {Wait-KwilPackagingChild $context $process 'fixture' 'fixture.exe' $timer $writer} $reason
        Test-KwilEqual $process.Waits[0] 100 'receipt failure still reaches bounded wait'
        Test-KwilEqual $process.KillArgumentCount 0 'timeout cleanup never requests a process tree'
        if($case.Expired) {
            Test-KwilEqual $process.KillCalls 1 'receipt failures cannot bypass exact owned timeout cleanup'
            Test-KwilEqual $process.Waits.Count 2 'cleanup performs one bounded post-kill wait'
            Test-KwilEqual $process.Waits[1] 1000 'post-kill wait remains bounded'
            Test-KwilEqual $context.Messages.Count 3 'start, timeout and cleanup receipts all attempted'
            Test-KwilEqual ($context.Messages[2].Contains('cleanupExited=True')) $true 'cleanup outcome receipt attempted after writer failure'
        } else {
            Test-KwilEqual $process.KillCalls 0 'receipt failure alone does not authorize termination'
            Test-KwilEqual $process.Waits.Count 1 'normally exited child needs no cleanup wait'
            Test-KwilEqual $context.Messages.Count 2 'exit receipt attempted after start receipt failure'
        }
    }
}

function New-KwilTestPe {
    param([string]$Architecture='x64',[string]$Kind='guard',[string]$Normal='kernel32.dll',[string]$Delay='user32.dll',[bool]$DelayVa=$false)
    # Offsets independently authored from PE32+ layout; never loaded/executed.
    $b=[byte[]]::new(4608)
    $b[0]=77;$b[1]=90;[BitConverter]::GetBytes([uint32]128).CopyTo($b,60)
    [BitConverter]::GetBytes([uint32]0x4550).CopyTo($b,128)
    [BitConverter]::GetBytes([uint16]$(if($Architecture -ceq 'x64'){0x8664}else{0xaa64})).CopyTo($b,132)
    [BitConverter]::GetBytes([uint16]1).CopyTo($b,134)
    [BitConverter]::GetBytes([uint16]240).CopyTo($b,148)
    [BitConverter]::GetBytes([uint16]$(if($Kind -ceq 'guard'){0x2022}else{0x22})).CopyTo($b,150)
    [BitConverter]::GetBytes([uint16]0x20b).CopyTo($b,152)
    [BitConverter]::GetBytes([uint64]0x10000000).CopyTo($b,176)
    [BitConverter]::GetBytes([uint16]$(if($Kind -ceq 'exe'){2}else{3})).CopyTo($b,220)
    [BitConverter]::GetBytes([uint32]16).CopyTo($b,260)
    [Text.Encoding]::ASCII.GetBytes('.rdata').CopyTo($b,392)
    [BitConverter]::GetBytes([uint32]4096).CopyTo($b,400)
    [BitConverter]::GetBytes([uint32]4096).CopyTo($b,404)
    [BitConverter]::GetBytes([uint32]4096).CopyTo($b,408)
    [BitConverter]::GetBytes([uint32]512).CopyTo($b,412)
    [BitConverter]::GetBytes([uint32]4096).CopyTo($b,272)
    [BitConverter]::GetBytes([uint32]40).CopyTo($b,276)
    [BitConverter]::GetBytes([uint32]4352).CopyTo($b,524)
    [Text.Encoding]::ASCII.GetBytes($Normal).CopyTo($b,768)
    if($Delay.Length -gt 0) {
        [BitConverter]::GetBytes([uint32]4160).CopyTo($b,368)
        [BitConverter]::GetBytes([uint32]64).CopyTo($b,372)
        [BitConverter]::GetBytes([uint32]$(if($DelayVa){0}else{1})).CopyTo($b,576)
        [BitConverter]::GetBytes([uint32]$(if($DelayVa){0x10001140}else{4416})).CopyTo($b,580)
        [Text.Encoding]::ASCII.GetBytes($Delay).CopyTo($b,832)
    }
    if($Kind -ceq 'guard') {
        [BitConverter]::GetBytes([uint32]4608).CopyTo($b,264)
        [BitConverter]::GetBytes([uint32]192).CopyTo($b,268)
        [BitConverter]::GetBytes([uint32]1).CopyTo($b,1044)
        [BitConverter]::GetBytes([uint32]1).CopyTo($b,1048)
        [BitConverter]::GetBytes([uint32]4736).CopyTo($b,1052)
        [BitConverter]::GetBytes([uint32]4752).CopyTo($b,1056)
        [BitConverter]::GetBytes([uint32]4760).CopyTo($b,1060)
        [BitConverter]::GetBytes([uint32]7936).CopyTo($b,1152)
        [BitConverter]::GetBytes([uint32]4768).CopyTo($b,1168)
        [Text.Encoding]::ASCII.GetBytes('KwilPreflightV1').CopyTo($b,1184)
    }
    return ,$b
}

function New-KwilTestRepeatedImports {
    param([int]$Count=2,[string]$Last='KERNEL32.dll',[string]$Delay='', [switch]$BadLastRva,
        [string]$Architecture='x64')
    $original=New-KwilTestPe $Architecture 'guard' 'kernel32.dll' $Delay
    $b=[byte[]]::new(8704); $original.CopyTo($b,0)
    [BitConverter]::GetBytes([uint32]8192).CopyTo($b,400)
    [BitConverter]::GetBytes([uint32]8192).CopyTo($b,408)
    [BitConverter]::GetBytes([uint32]0x1600).CopyTo($b,272)
    [BitConverter]::GetBytes([uint32](($Count+1)*20)).CopyTo($b,276)
    [Text.Encoding]::ASCII.GetBytes($Last+[char]0).CopyTo($b,0x1800)
    for($i=0;$i -lt $Count;$i++) {
        $nr=if($i -eq $Count-1){if($BadLastRva){0xfffffff}else{0x2600}}else{4352}
        [BitConverter]::GetBytes([uint32]$nr).CopyTo($b,(0x800+$i*20+12))
    }
    return ,$b
}

function Read-KwilTestImports {
    param([byte[]]$Bytes,[string]$Architecture='x64',[string]$Kind='guard')
    $s=[IO.MemoryStream]::new($Bytes,$false)
    try{return ,(Get-KwilPeImports $s $Architecture $Kind)}finally{$s.Dispose()}
}

function New-KwilTestFiles {
    $files=[ordered]@{}
    foreach($n in @('kwil.exe','kwil.com','kwil.ico','LICENSE','LICENSES.md','LICENSES.txt','kwil.no-update')) {
        $files[$n]=[pscustomobject]@{Size=3;Sha256='ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'}
    }
    return $files
}

function New-KwilTestMetadata {
    param($Model,[string]$Architecture='x64',[string]$GuardHash=('a'*64),[object[]]$Imports=@([ordered]@{load='N';dll='kernel32.dll'}))
    return [ordered]@{schema=1;componentSchema=1;familyGuid='{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}';version='1.2.3';commit=('b'*40);architecture=$Architecture;target=$(if($Architecture -ceq 'x64'){'x86_64-pc-windows-msvc'}else{'aarch64-pc-windows-msvc'});guard=[ordered]@{abi=1;sha256=$GuardHash;imports=$Imports};inventory=$Model.Rows}
}

function ConvertTo-KwilTestJson {
    param($Metadata)
    return ,[Text.UTF8Encoding]::new($false,$true).GetBytes(($Metadata | ConvertTo-Json -Depth 8 -Compress))
}

function New-KwilReferenceInventory {
    param([object[]]$Rows)
    # Independent LP encoder; deliberately does not use backend Write-KwilLP.
    $bytes=[Collections.Generic.List[byte]]::new()
    $bytes.AddRange([Text.Encoding]::ASCII.GetBytes('KWR1'))
    $bytes.AddRange([BitConverter]::GetBytes([uint32]$Rows.Count))
    foreach($r in $Rows) {
        foreach($key in @('id','kind','root','relative','name','data','size','sha256','component','source','fileKey','registryKey','valueType')) {
            $v=[Text.Encoding]::UTF8.GetBytes($r[$key]);$bytes.AddRange([BitConverter]::GetBytes([uint32]$v.Length));$bytes.AddRange($v)
        }
    }
    return ,$bytes.ToArray()
}

function New-KwilReferenceBuild {
    param($Metadata,[byte[]]$Inventory)
    $bytes=[Collections.Generic.List[byte]]::new()
    $bytes.AddRange([Text.Encoding]::ASCII.GetBytes('KWB1'))
    foreach($v in @('1','1',$Metadata.familyGuid,$Metadata.architecture,$Metadata.version,$Metadata.commit,$Metadata.target,'1',$Metadata.guard.sha256)) {
        $s=[Text.Encoding]::UTF8.GetBytes($v);$bytes.AddRange([BitConverter]::GetBytes([uint32]$s.Length));$bytes.AddRange($s)
    }
    $bytes.AddRange([BitConverter]::GetBytes([uint32]$Metadata.guard.imports.Count))
    foreach($im in $Metadata.guard.imports) {
        foreach($v in @($im.load,$im.dll)){$s=[Text.Encoding]::UTF8.GetBytes($v);$bytes.AddRange([BitConverter]::GetBytes([uint32]$s.Length));$bytes.AddRange($s)}
    }
    $bytes.AddRange([BitConverter]::GetBytes([uint32]$Inventory.Length));$bytes.AddRange($Inventory)
    return ,$bytes.ToArray()
}

Test-KwilQueryOutputShape
Test-KwilReceiptFailureCleanup
Test-KwilReject {Test-KwilEqual ([object[]]@(0,1)) 0 'polluted backend result'} 'scalar.*pipeline array'

# Full CLI tokens only: no binder aliases, positional, repeated or extra input.
$valid=@('-PayloadDirectory','E:\input','-OutputPath','E:\out\kwil.msi','-Version','1.2.3','-Architecture','x64','-Commit',('b'*40),'-IconPath','E:\input\kwil.ico')
Test-KwilEqual (Get-KwilArguments $valid).Version '1.2.3' 'CLI success'
foreach($case in @(@(4,'-V'),@(4,'-version'),@(4,'-PayloadDirectory'),@(5,'01.2.3'),@(5,'256.0.0'),@(5,'1.2.65536'),@(5,'1.2.3-beta'),@(7,'X64'),@(9,('B'*40)),@(5,"1.2.3`n"),@(7,"x64`n"),@(9,(('b'*40)+"`n")))) {
    $tokens=[object[]]$valid.Clone();$tokens[$case[0]]=$case[1]
    Test-KwilReject {Get-KwilArguments $tokens} 'argument|Version|Architecture|Commit'
}
Test-KwilReject {Get-KwilArguments ($valid+@('-Other','x'))} 'exactly six'
Test-KwilReject {Get-KwilArguments @('E:\payload')} 'exactly six'

Test-KwilEqual (Get-KwilHash ([Text.Encoding]::ASCII.GetBytes('abc'))) 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad' 'SHA256 vector'
Test-KwilEqual (New-KwilUuid5 '{6BA7B810-9DAD-11D1-80B4-00C04FD430C8}' ([Text.Encoding]::ASCII.GetBytes('www.widgets.com'))) '{21F7F8DE-8051-5B89-8680-0195EF798B6A}' 'RFC UUIDv5 network-order vector'
Test-KwilEqual ([Convert]::ToHexString((New-KwilEncoding 'AB' @('','x')))) '4142000000000100000078' 'LP empty/string vector'

$lnk=New-KwilShortcut
$reference=[byte[]]::new(868)
[Convert]::FromHexString('4C0000000114020000000000C00000000000004600020002').CopyTo($reference,0)
$reference[60]=1
[Convert]::FromHexString('14030000010000A0').CopyTo($reference,76)
[Text.Encoding]::ASCII.GetBytes('%LOCALAPPDATA%\Programs\Kwil\kwil.exe').CopyTo($reference,84)
[Text.Encoding]::Unicode.GetBytes('%LOCALAPPDATA%\Programs\Kwil\kwil.exe').CopyTo($reference,344)
Test-KwilEqual $lnk.Length 868 'LNK exact size'
Test-KwilEqual ([Convert]::ToBase64String($lnk)) ([Convert]::ToBase64String($reference)) 'LNK complete independent bytes including zero padding'

$files=New-KwilTestFiles;$model=New-KwilModel 'x64' $files
Test-KwilEqual $model.Rows.Count 29 'schema1 total'
foreach($pair in @(@('F',8),@('R',11),@('K',10))){Test-KwilEqual @($model.Rows|Where-Object{$_.kind -ceq $pair[0]}).Count $pair[1] 'kind count'}
Test-KwilEqual $model.Components.Count 11 'component count'
Test-KwilEqual $model.Rows[0].id 'r000001' 'first resource ID'
$last='';$index=0
foreach($r in $model.Rows){$identity=Get-KwilIdentity $r;Assert-Kwil ([StringComparer]::Ordinal.Compare($identity,$last) -gt 0) 'fixture identity ordering';$last=$identity;$index++;Test-KwilEqual $r.id ('r'+$index.ToString('D6')) 'dense resource ID'}
Test-KwilEqual ([Convert]::ToBase64String((Get-KwilInventoryBytes $model.Rows))) ([Convert]::ToBase64String((New-KwilReferenceInventory $model.Rows))) 'canonical inventory full field order'
$changed=New-KwilTestFiles;$changed['kwil.exe'].Sha256='c'*64
$other=New-KwilModel 'x64' $changed
Test-KwilEqual ($other.Rows | Where-Object{$_.relative -ceq 'kwil.exe'}).component ($model.Rows | Where-Object{$_.relative -ceq 'kwil.exe'}).component 'component excludes content'
Assert-Kwil ((Get-KwilComponent 'arm64' 'F' 'I:kwil.exe').Id -cne (Get-KwilComponent 'x64' 'F' 'I:kwil.exe').Id) 'architecture separates components'
Test-KwilReject {New-KwilModel 'x64' ([ordered]@{})} 'seven installed'

$metadata=New-KwilTestMetadata $model
$typed=Read-KwilJson (ConvertTo-KwilTestJson $metadata)
$a=Get-KwilArguments $valid
Assert-KwilMetadata $typed $a $model ('a'*64) @([ordered]@{load='N';dll='kernel32.dll'})
$script:KwilTestCount++
$inv=Get-KwilInventoryBytes $model.Rows;$build=Get-KwilBuildId $typed $inv
Test-KwilEqual $build (Get-KwilHash (New-KwilReferenceBuild $typed $inv)) 'independent full KWB1 field order/counts'
Assert-Kwil ((Get-KwilBuildId (New-KwilTestMetadata $other) (Get-KwilInventoryBytes $other.Rows)) -cne $build) 'fixture changed installed bytes must change BuildID'
$pretty=[Text.Encoding]::UTF8.GetBytes(($metadata|ConvertTo-Json -Depth 8))
Test-KwilEqual (Get-KwilBuildId (Read-KwilJson $pretty) $inv) $build 'JSON whitespace is not identity'
$typed.inventory[0].data='foreign'
Test-KwilReject {Assert-KwilMetadata $typed $a $model ('a'*64) @([ordered]@{load='N';dll='kernel32.dll'})} 'inventory mismatch'
foreach($invalid in @('{"schema":1,"schema":1}','{"schema":1,"\u0073chema":1}','{"x":1.0}','{"x":NaN}','{"x":"\u0000"}','{"x":1}{}','{"x":1,}')) {
    Test-KwilReject {Read-KwilJson ([Text.Encoding]::UTF8.GetBytes($invalid))} 'duplicate|integer|JSON|invalid|depth|Kwil|LineNumber'
}
Test-KwilReject {Read-KwilJson ([byte[]]@(239,187,191,123,125))} 'BOM'
Test-KwilReject {Read-KwilJson ([byte[]]@(123,34,120,34,58,34,255,34,125))} 'Unable to translate|invalid|fallback'
Test-KwilReject {Read-KwilJson ([byte[]]::new(1048577))} 'byte limit'
$extra=New-KwilTestMetadata $model;$extra.extra='not permitted';$extra=Read-KwilJson (ConvertTo-KwilTestJson $extra)
Test-KwilReject {Assert-KwilMetadata $extra $a $model ('a'*64) @([ordered]@{load='N';dll='kernel32.dll'})} 'object shape'
$wrong=New-KwilTestMetadata $model;$wrong.guard.imports=@([ordered]@{load='D';dll='kernel32.dll'});$wrong=Read-KwilJson (ConvertTo-KwilTestJson $wrong)
Test-KwilReject {Assert-KwilMetadata $wrong $a $model ('a'*64) @([ordered]@{load='N';dll='kernel32.dll'})} 'normal/delay import mismatch'
$wrong=New-KwilTestMetadata $model;$wrong.guard.imports=@($wrong.guard.imports[0],$wrong.guard.imports[0]);$wrong=Read-KwilJson (ConvertTo-KwilTestJson $wrong)
Test-KwilReject {Assert-KwilMetadata $wrong $a $model ('a'*64) @([ordered]@{load='N';dll='kernel32.dll'})} 'guard import count mismatch'
$wrong=New-KwilTestMetadata $model;$wrong.schema='1';$wrong=Read-KwilJson (ConvertTo-KwilTestJson $wrong)
Test-KwilReject {Assert-KwilMetadata $wrong $a $model ('a'*64) @([ordered]@{load='N';dll='kernel32.dll'})} 'invalid schema'

foreach($architecture in @('x64','arm64')) {
    $im=Read-KwilTestImports (New-KwilTestRepeatedImports -Architecture $architecture) $architecture
    Test-KwilEqual $im.Count 1 'physical case aliases yield one canonical pair'
    Test-KwilEqual ($im[0].load+':'+$im[0].dll) 'N:kernel32.dll' 'exact canonical set'
    $im=Read-KwilTestImports (New-KwilTestRepeatedImports -Architecture $architecture -Delay 'kernel32.dll') $architecture
    Test-KwilEqual $im.Count 2 'load classes stay distinct after physical deduplication'
    Test-KwilEqual ($im[0].load+':'+$im[0].dll) 'D:kernel32.dll' 'delay canonical pair'
    Test-KwilEqual ($im[1].load+':'+$im[1].dll) 'N:kernel32.dll' 'normal canonical pair'
    $im=Read-KwilTestImports (New-KwilTestPe $architecture) $architecture
    Test-KwilEqual $im.Count 2 'normal and delay both inspected'
    Test-KwilEqual ($im[0].load+':'+$im[0].dll) 'D:user32.dll' 'ordinal import ordering'
    Test-KwilEqual ($im[1].load+':'+$im[1].dll) 'N:kernel32.dll' 'normal import evidence'
    Test-KwilEqual (Read-KwilTestImports (New-KwilTestPe $architecture 'guard' 'kernel32.dll' '') $architecture).Count 1 'explicit no-delay import directory'
    Test-KwilEqual (Read-KwilTestImports (New-KwilTestPe $architecture 'guard' 'kernel32.dll' 'user32.dll' $true) $architecture).Count 2 'delay VA conversion'
    foreach($kind in @('exe','com')){$null=Read-KwilTestImports (New-KwilTestPe $architecture $kind) $architecture $kind;$script:KwilTestCount++}
}
Test-KwilEqual (Read-KwilTestImports (New-KwilTestRepeatedImports -Count 128)).Count 1 '128 physical aliases admitted'
Test-KwilReject {Read-KwilTestImports (New-KwilTestRepeatedImports -Count 129)} 'directory limit|physical imports'
Test-KwilReject {Read-KwilTestImports (New-KwilTestRepeatedImports -Count 128 -Delay 'kernel32.dll')} 'physical imports'
Test-KwilReject {Read-KwilTestImports (New-KwilTestRepeatedImports -Count 3 -Last 'kernel32.dll/')} 'invalid DLL basename'
Test-KwilReject {Read-KwilTestImports (New-KwilTestRepeatedImports -Count 3 -Last 'UCRTBASE.dll')} 'dynamic runtime'
Test-KwilReject {Read-KwilTestImports (New-KwilTestRepeatedImports -Count 3 -BadLastRva)} 'outside backed'
foreach($bad in @('vcruntime140.dll','msvcp140.dll','msvcr120.dll','ucrtbase.dll','api-ms-win-crt-runtime-l1-1-0.dll')) {
    Test-KwilReject {Read-KwilTestImports (New-KwilTestPe 'x64' 'guard' $bad)} 'dynamic runtime'
    Test-KwilReject {Read-KwilTestImports (New-KwilTestPe 'x64' 'guard' 'kernel32.dll' $bad)} 'dynamic runtime'
}
Test-KwilReject {Read-KwilTestImports (New-KwilTestPe 'arm64')} 'machine mismatch'
$bad=New-KwilTestPe;$bad[1184]=88
Test-KwilReject {Read-KwilTestImports $bad} 'export missing'
$bad=New-KwilTestPe;[BitConverter]::GetBytes([uint32]4608).CopyTo($bad,1152)
Test-KwilReject {Read-KwilTestImports $bad} 'forwarded'
$bad=New-KwilTestPe;[BitConverter]::GetBytes([uint32]20).CopyTo($bad,276)
Test-KwilReject {Read-KwilTestImports $bad} 'unterminated'
$bad=New-KwilTestPe;[BitConverter]::GetBytes([uint32]0xfffffff).CopyTo($bad,524)
Test-KwilReject {Read-KwilTestImports $bad} 'outside backed'
$bad=New-KwilTestPe;[BitConverter]::GetBytes([uint32]0xffffff).CopyTo($bad,372)
Test-KwilReject {Read-KwilTestImports $bad} 'directory limit'

# Independent type vectors include ROOT's retained r2 raw _Columns values and
# nullable/key flag combinations. No native database is opened by these cases.
foreach($vector in @(@('i2',$false,0x0502),@('I2',$false,0x1502),@('i2',$true,0x2502),@('I2',$true,0x3502),@('i4',$false,0x0104),@('I4',$false,0x1104),@('i4',$true,0x2104),@('s72',$false,0x0D48),@('S72',$false,0x1D48),@('s72',$true,0x2D48),@('v0',$false,0x0900))) {
    Test-KwilEqual (Get-KwilMsiColumnType $vector[0] $vector[1]) $vector[2] 'exact canonical MSI column type'
}
$schemaSpecs=Get-KwilTableSpecs
$columnFixture=@(@(1,'Component',0x2D48),@(2,'ComponentId',0x1D26),@(3,'Directory_',0x0D48),@(4,'Attributes',0x0502),@(5,'Condition',0x1DFF),@(6,'KeyPath',0x1D48))
Assert-KwilColumnSchema 'Component' $schemaSpecs.Component $columnFixture
$script:KwilTestCount++
foreach($badType in @(0x0102,0x0104,0x1502,0x2502,0x0D02,0x4502,0x0702)) {
    $mutated=[object[]]$columnFixture.Clone()
    $mutated[3]=@(4,'Attributes',$badType)
    Test-KwilReject {Assert-KwilColumnSchema 'Component' $schemaSpecs.Component $mutated} 'typed schema mismatch Component/Attributes'
}
$mutated=[object[]]$columnFixture.Clone();$mutated[3]=@(5,'Attributes',0x0502)
Test-KwilReject {Assert-KwilColumnSchema 'Component' $schemaSpecs.Component $mutated} 'typed schema mismatch Component/Attributes'
$mutated=[object[]]$columnFixture.Clone();$mutated[3]=@(4,'attributes',0x0502)
Test-KwilReject {Assert-KwilColumnSchema 'Component' $schemaSpecs.Component $mutated} 'typed schema mismatch Component/Attributes'
Test-KwilReject {Assert-KwilColumnSchema 'Component' $schemaSpecs.Component $columnFixture[0..4]} 'column count Component'

$tables=New-KwilTables $metadata $model $build (Get-KwilHash $inv) '{11111111-1111-5111-8111-111111111111}' '{22222222-2222-4222-8222-222222222222}' 'E:\guard.dll' 'E:\kwil.ico'
foreach($badOwner in @('{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}|arm64|1|I:kwil.exe','{00000000-0000-0000-0000-000000000000}|x64|1|I:kwil.exe','{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}|x64|1|I:kwil.exe{extra}','{3d9c2c6f-7c59-4d54-92b4-0ba1ec60347a}|x64|1|I:kwil.exe')) {
    $badModel=New-KwilModel 'x64' (New-KwilTestFiles)
    $exe=($badModel.Rows|Where-Object{$_.kind -ceq 'F' -and $_.relative -ceq 'kwil.exe'})
    ($badModel.Rows|Where-Object{$_.kind -ceq 'R' -and $_.component -ceq $exe.component}).data=$badOwner
    Test-KwilReject {New-KwilTables $metadata $badModel $build (Get-KwilHash $inv) '{11111111-1111-5111-8111-111111111111}' '{22222222-2222-4222-8222-222222222222}' 'E:\guard.dll' 'E:\kwil.ico'} 'Owner literal/key mismatch'
}
$badModel=New-KwilModel 'x64' (New-KwilTestFiles)
($badModel.Rows|Where-Object{$_.kind -ceq 'R' -and $_.data -ceq 'Open with Kwil'}).data='Open {with} Kwil'
Test-KwilReject {New-KwilTables $metadata $badModel $build (Get-KwilHash $inv) '{11111111-1111-5111-8111-111111111111}' '{22222222-2222-4222-8222-222222222222}' 'E:\guard.dll' 'E:\kwil.ico'} 'registry dollar/brace literal'
Test-KwilEqual $tables.File.Rows.Count 8 'File bijection'
Test-KwilEqual $tables.Registry.Rows.Count 11 'Registry bijection'
Test-KwilEqual $tables.FeatureComponents.Rows.Count 11 'feature includes every component'
foreach($row in $tables.Component.Rows){Test-KwilEqual $row[3] 260 'native64 registry keypath flags';Assert-Kwil ($row[5] -cmatch '^R[0-9]{6}$') 'fixture component registry keypath'}
foreach($forbidden in @('Shortcut','RemoveRegistry','CreateFolder','Environment','ServiceInstall','InstallUISequence')){Test-KwilEqual $tables.Contains($forbidden) $false 'forbidden table absent'}
Test-KwilEqual $tables.CustomAction.Rows.Count 2 'only guard and ARP setter'
Test-KwilEqual ($tables.Directory.Rows|Where-Object{$_[0] -ceq 'INSTALLDIR'})[2] 'Kwil' 'R2 exact directory spelling'
Test-KwilEqual $tables.LaunchCondition.Rows[0][0] 'NOT RollbackDisabled AND NOT DISABLEROLLBACK' 'R2 rollback condition'
Test-KwilEqual ($tables.InstallExecuteSequence.Rows|Where-Object{$_[0] -ceq 'LaunchConditions'})[2] 100 'rollback condition precedes transaction'
Test-KwilEqual ($tables.InstallExecuteSequence.Rows|Where-Object{$_[0] -ceq 'KwilPreflight'})[1] '1' 'guard unconditional'
Test-KwilEqual ($tables.InstallExecuteSequence.Rows|Where-Object{$_[0] -ceq 'KwilPreflight'})[2] 1490 'guard before Initialize'
Test-KwilEqual ($tables.InstallExecuteSequence.Rows|Where-Object{$_[0] -ceq 'RemoveExistingProducts'})[2] 1501 'early predecessor removal'
Test-KwilEqual @($tables.InstallExecuteSequence.Rows|Where-Object{$_[0] -ceq 'DisableRollback'}).Count 0 'DisableRollback absent'
Test-KwilEqual $tables.RemoveFile.Rows[0][2] $null 'only empty directory removal'
foreach($r in $model.Rows|Where-Object{$_.kind -ceq 'F'}){Assert-Kwil ((Get-KwilShortName $r) -cmatch '^K[0-9]{6}(\.[a-z0-9]{1,3})?\|[A-Za-z0-9._-]+$') 'fixture short|long filename'}

# CAB member table fixture: malicious member names/lengths fail BEFORE expand.
$fr=@($model.Rows|Where-Object{$_.kind -ceq 'F'})
$cab=[byte[]]::new(44+24*8)
[Text.Encoding]::ASCII.GetBytes('MSCF').CopyTo($cab,0)
[BitConverter]::GetBytes([uint32]$cab.Length).CopyTo($cab,8)
[BitConverter]::GetBytes([uint32]44).CopyTo($cab,16)
[BitConverter]::GetBytes([uint16]1).CopyTo($cab,26)
[BitConverter]::GetBytes([uint16]8).CopyTo($cab,28)
$offset=0
for($i=0;$i -lt 8;$i++){$p=44+24*$i;[BitConverter]::GetBytes([uint32]$fr[$i].size).CopyTo($cab,$p);[BitConverter]::GetBytes([uint32]$offset).CopyTo($cab,$p+4);[Text.Encoding]::ASCII.GetBytes($fr[$i].fileKey).CopyTo($cab,$p+16);$offset+=[int]$fr[$i].size}
$s=[IO.MemoryStream]::new($cab,$false)
try{Assert-KwilCabinetHeader $s $fr;$script:KwilTestCount++}finally{$s.Dispose()}
$cab[60]=46;$s=[IO.MemoryStream]::new($cab,$false)
try{Test-KwilReject {Assert-KwilCabinetHeader $s $fr} 'member name'}finally{$s.Dispose()}

if($PSBoundParameters.ContainsKey('PackagingRoot')) {
    Assert-Kwil $IsWindows 'PackagingRoot fixtures require Windows; no emulated lifecycle claim'
    Initialize-KwilNative
    $base=Assert-KwilLocalPath $PackagingRoot $false $true
    $root=[IO.Path]::Combine($base,'windows-msi-fixture-'+$PID+'-'+[Guid]::NewGuid().ToString('N'))
    $null=Assert-KwilLocalPath $root $true;$null=[IO.Directory]::CreateDirectory($root)
    [Console]::Out.WriteLine("fixturePid=$PID retainedFixtureRoot=$root nativeLifecycle=not-run")
    $hardSource=[IO.Path]::Combine($root,'hardlink-source.bin');Write-KwilNewBytes $hardSource ([byte[]]@(1,2,3))
    $null=New-Item -ItemType HardLink -Path ([IO.Path]::Combine($root,'hardlink-alias.bin')) -Target $hardSource
    Test-KwilReject {Open-KwilInput $hardSource 1024} 'single-link'
    $target=[IO.Path]::Combine($root,'junction-target');$null=[IO.Directory]::CreateDirectory($target)
    $junction=[IO.Path]::Combine($root,'junction');$null=New-Item -ItemType Junction -Path $junction -Target $target
    Test-KwilReject {Assert-KwilLocalPath $junction $false $true} 'reparse'
    foreach($architecture in @('x64','arm64')) {
        $payload=[IO.Path]::Combine($root,$architecture);$null=[IO.Directory]::CreateDirectory($payload)
        $metaDir=[IO.Path]::Combine($payload,'.installer');$null=[IO.Directory]::CreateDirectory($metaDir)
        foreach($name in @('kwil.exe','kwil.com','kwil.ico','LICENSE','LICENSES.md','LICENSES.txt','kwil.no-update')) {
            $bytes=switch -CaseSensitive ($name){'kwil.exe'{New-KwilTestPe $architecture 'exe'};'kwil.com'{New-KwilTestPe $architecture 'com'};'kwil.no-update'{[Text.Encoding]::UTF8.GetBytes("Updates are managed by this distribution. Rebuild/reinstall explicitly.`n")};'kwil.ico'{[Convert]::FromHexString('0000010001000101000001002000300000001600000028000000010000000200000001002000000000000400000000000000000000000000000000000000000000FF00000000')};default{[Text.Encoding]::UTF8.GetBytes('fixture license text')}}
            Write-KwilNewBytes ([IO.Path]::Combine($payload,$name)) $bytes
        }
        $gb=New-KwilTestPe $architecture;Write-KwilNewBytes ([IO.Path]::Combine($metaDir,'kwil-installer-guard.dll')) $gb
        $fs=[ordered]@{}
        foreach($name in @('kwil.exe','kwil.com','kwil.ico','LICENSE','LICENSES.md','LICENSES.txt','kwil.no-update')){$f=Open-KwilInput ([IO.Path]::Combine($payload,$name)) 65536;try{$fs[$name]=[pscustomobject]@{Size=$f.Size;Sha256=$f.Sha256}}finally{$f.Stream.Dispose()}}
        $mo=New-KwilModel $architecture $fs;$im=Read-KwilTestImports $gb $architecture
        $md=New-KwilTestMetadata $mo $architecture (Get-KwilHash $gb) $im
        Write-KwilNewBytes ([IO.Path]::Combine($metaDir,'input.json')) (ConvertTo-KwilTestJson $md)
        $out=[IO.Path]::Combine($root,$architecture+'.msi')
        $tokens=@('-PayloadDirectory',$payload,'-OutputPath',$out,'-Version','1.2.3','-Architecture',$architecture,'-Commit',('b'*40),'-IconPath',([IO.Path]::Combine($payload,'kwil.ico')))
        Test-KwilEqual (Invoke-KwilMsiBackend $tokens) 0 'fixture MSI/CAB readback success'
        $manifest=Get-Content -Raw -LiteralPath ($out+'.manifest.json')|ConvertFrom-Json -AsHashtable
        Test-KwilEqual $manifest.nativeLifecycle 'not-run' 'no lifecycle claim'
        Test-KwilEqual $manifest.inspection.cabinet 'passed' 'embedded CAB readback'
        Test-KwilEqual $manifest.signed $false 'unsigned fixture'
        Assert-KwilMembers $manifest @('schema','familyGuid','componentSchema','architecture','target','version','commit','buildId','inventorySha256','productCode','packageCode','guard','inventory','msi','inspection','signed','nativeLifecycle')
        Test-KwilEqual (Invoke-KwilMsiBackend $tokens) 1 'preexisting output refuses without overwrite'
        $unchanged=Open-KwilInput $out 16777216;try{Test-KwilEqual $unchanged.Sha256 $manifest.msi.sha256 'refused output remains byte-identical'}finally{$unchanged.Stream.Dispose()}
        $blocked=[object[]]$tokens.Clone();$blocked[3]=[IO.Path]::Combine($root,$architecture+'-blocked.msi')
        $foreignBytes=[Text.Encoding]::UTF8.GetBytes('foreign output belongs to someone else')
        Write-KwilNewBytes ($blocked[3]+'.manifest.json') $foreignBytes
        Test-KwilEqual (Invoke-KwilMsiBackend $blocked) 1 'preexisting sidecar refuses'
        Test-KwilEqual ([IO.File]::Exists($blocked[3])) $false 'foreign sidecar refusal produces no MSI'
        $foreignSide=Open-KwilInput ($blocked[3]+'.manifest.json') 1024;try{Test-KwilEqual $foreignSide.Sha256 (Get-KwilHash $foreignBytes) 'foreign sidecar preserved'}finally{$foreignSide.Stream.Dispose()}
        $repeat=[object[]]$tokens.Clone();$repeat[3]=[IO.Path]::Combine($root,$architecture+'-repeat.msi')
        Test-KwilEqual (Invoke-KwilMsiBackend $repeat) 0 'repack fixture success'
        $repacked=Get-Content -Raw -LiteralPath ($repeat[3]+'.manifest.json')|ConvertFrom-Json -AsHashtable
        Test-KwilEqual $repacked.buildId $manifest.buildId 'repack BuildID stable'
        Test-KwilEqual $repacked.productCode $manifest.productCode 'repack ProductCode stable'
        Assert-Kwil ($repacked.packageCode -cne $manifest.packageCode) 'fixture PackageCode must be fresh'
        $original=Open-KwilInput $out 16777216;try{$originalHash=$original.Sha256}finally{$original.Stream.Dispose()}
        $tamper=[IO.Path]::Combine($root,$architecture+'-tamper.msi');[IO.File]::Copy($out,$tamper,$false)
        $ci=New-Object -ComObject WindowsInstaller.Installer;$db=$null
        try {
            $db=$ci.OpenDatabase($tamper,1)
            Invoke-KwilSql $db 'UPDATE `Property` SET `Value`=''0'' WHERE `Property`=''ARPNOREPAIR'''
            $null=$db.Commit();Release-KwilCom $db;$db=$null
            $inv=Get-KwilInventoryBytes $mo.Rows
            $tt=New-KwilTables $md $mo $manifest.buildId (Get-KwilHash $inv) $manifest.productCode $manifest.packageCode ([IO.Path]::Combine($metaDir,'kwil-installer-guard.dll')) ([IO.Path]::Combine($payload,'kwil.ico'))
            Test-KwilReject {Assert-KwilDatabase $ci $tamper $tt $architecture $manifest.packageCode} 'row mismatch Property'
        }finally{Release-KwilCom $db;Release-KwilCom $ci}
        # Correct SHORT metadata must not excuse wrong Component.Attributes data.
        # Retain a separate mutated copy; the emitted original is never edited.
        $componentTamper=[IO.Path]::Combine($root,$architecture+'-tamper-component.msi')
        [IO.File]::Copy($out,$componentTamper,$false)
        $ci=New-Object -ComObject WindowsInstaller.Installer;$db=$null
        try {
            $db=$ci.OpenDatabase($componentTamper,1)
            Invoke-KwilSql $db 'UPDATE `Component` SET `Attributes`=256'
            $null=$db.Commit();Release-KwilCom $db;$db=$null
            Test-KwilReject {Assert-KwilDatabase $ci $componentTamper $tt $architecture $manifest.packageCode} 'row mismatch Component'
        }finally{Release-KwilCom $db;Release-KwilCom $ci}
        $original=Open-KwilInput $out 16777216;try{Test-KwilEqual $original.Sha256 $originalHash 'original emitted MSI preserved'}finally{$original.Stream.Dispose()}
        $foreign=[IO.Path]::Combine($payload,'undeclared.txt');Write-KwilNewBytes $foreign ([byte[]]@(1))
        $negative=[object[]]$tokens.Clone();$negative[3]=[IO.Path]::Combine($root,$architecture+'-negative.msi')
        Test-KwilEqual (Invoke-KwilMsiBackend $negative) 1 'undeclared payload input refuses'
        # Intentionally retain even negative fixtures; no recursive cleanup.
    }
}
[Console]::Out.WriteLine("Kwil MSI fixtures passed: $script:KwilTestCount; nativeLifecycle=not-run. Packaging fixtures require separate explicit admission.")
