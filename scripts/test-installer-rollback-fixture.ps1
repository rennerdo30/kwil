# Inert controls only: no COM, MSI, registry, profile, account or child operations.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'installer-rollback-fixture.ps1')
$count = 0
function Check([bool]$Ok,[string]$Message) {
    if (-not $Ok) { throw $Message }
    $script:count++
}
function Reject([scriptblock]$Action,[string]$Reason) {
    $caught = $false
    try { & $Action } catch { $caught = $true; Check ($_.Exception.Message -match $Reason) 'wrong refusal' }
    Check $caught 'expected refusal was accepted'
}
$old = '{11111111-1111-4111-8111-111111111111}'
$fresh = '{22222222-2222-4222-8222-222222222222}'
function Tables {
    $tables = Get-KwilTableSpecs
    Add-KwilRow $tables KwilPackage @(1,2,1,'family','x64','0.1.0-beta.2','beta','0.0.2',('a'*40),'target',2,('b'*64),('c'*64),('d'*64),'product',$old,29)
    Add-KwilRow $tables CustomAction @('KwilPreflight',1,'KwilPreflight','KwilPreflightV2')
    Add-KwilRow $tables CustomAction @('SetKwilArpLocation',51,'ARPINSTALLLOCATION','[INSTALLDIR]')
    foreach ($entry in @(@('KwilPreflight',1490),@('InstallInitialize',1500),@('RemoveExistingProducts',1501),@('ProcessComponents',1600))) {
        Add-KwilRow $tables InstallExecuteSequence @($entry[0],$null,$entry[1])
    }
    return $tables
}
$tables = Tables
$packageBefore = $tables.KwilPackage.Rows[0].Clone()
$actionsBefore = @($tables.CustomAction.Rows | ForEach-Object { Get-KwilRowSignature $_ })
Set-KwilRollbackFixtureTables $tables $old $fresh
Check ($tables.CustomAction.Rows.Count -eq 3 -and $tables.InstallExecuteSequence.Rows.Count -eq 5) 'exactly two added rows'
Check ($tables.CustomAction.Rows[2][1] -eq 19 -and $null -eq $tables.CustomAction.Rows[2][2]) 'fixed error only; no code source'
Check ($tables.InstallExecuteSequence.Rows[4][1] -ceq 'NOT Installed AND KWIL_OLDER' -and $tables.InstallExecuteSequence.Rows[4][2] -eq 1502) 'fault only after removal'
Check ($tables.KwilPackage.Rows[0][15] -ceq $fresh) 'fresh package identity'
for ($i=0; $i -lt 17; $i++) {
    if ($i -ne 15) { Check ($tables.KwilPackage.Rows[0][$i] -ceq $packageBefore[$i]) 'identity field changed' }
}
for ($i=0; $i -lt 2; $i++) { Check ((Get-KwilRowSignature $tables.CustomAction.Rows[$i]) -ceq $actionsBefore[$i]) 'original action changed' }
Reject { Set-KwilRollbackFixtureTables $tables $fresh $old } 'unmodified action set'
Reject { Set-KwilRollbackFixtureTables (Tables) $old $old } 'fresh PackageCode'
Reject { Set-KwilRollbackFixtureTables (Tables) $old '{------------------------------------}' } 'fresh PackageCode'
$changed = Tables; $changed.CustomAction.Rows[0][3] = 'bypass'
Reject { Set-KwilRollbackFixtureTables $changed $old $fresh } 'guard unchanged'
$changed = Tables; $changed.InstallExecuteSequence.Rows[2][2] = 1700
Reject { Set-KwilRollbackFixtureTables $changed $old $fresh } 'original transaction sequence'
$changed = Tables; Add-KwilRow $changed InstallExecuteSequence @('unrelated',$null,1502)
Reject { Set-KwilRollbackFixtureTables $changed $old $fresh } 'slot already used'

$predecessor = '{33333333-3333-5333-8333-333333333333}'
$log = @"
Action start 10:00:00: KwilPreflight.
Action start 10:00:01: InstallInitialize.
Action start 10:00:02: RemoveExistingProducts.
Action start 10:00:03: KwilPreflight.
Action ended 10:00:04: RemoveExistingProducts. Return value 1.
Action start 10:00:05: KwilRollbackProbe.
Kwil lifecycle fixture: failure after predecessor removal.
Action ended 10:00:06: KwilRollbackProbe. Return value 3.
Property(S): KWIL_OLDER = $predecessor
"@
Assert-KwilRollbackFixtureLog 1603 $log $predecessor; $count++
Assert-KwilRollbackFixtureLog 1603 ($log.Replace("`n","`r`n")) $predecessor; $count++
foreach ($exit in 0,3010,1618) { Reject { Assert-KwilRollbackFixtureLog $exit $log $predecessor } 'naturally with 1603' }
foreach ($line in @(
    'Action start 10:00:03: KwilPreflight.',
    'Action start 10:00:01: InstallInitialize.',
    'Action start 10:00:02: RemoveExistingProducts.',
    'Action ended 10:00:04: RemoveExistingProducts. Return value 1.',
    'Action start 10:00:05: KwilRollbackProbe.',
    'Kwil lifecycle fixture: failure after predecessor removal.',
    'Action ended 10:00:06: KwilRollbackProbe. Return value 3.',
    "Property(S): KWIL_OLDER = $predecessor"
)) {
    Reject { Assert-KwilRollbackFixtureLog 1603 ($log.Replace($line,'')) $predecessor } 'guards|predecessor|successful predecessor removal'
}
Reject { Assert-KwilRollbackFixtureLog 1603 ($log.Replace('RemoveExistingProducts. Return value 1.','RemoveExistingProducts. Return value 3.')) $predecessor } 'successful predecessor removal'
$reordered = $log.Replace('Action ended 10:00:04: RemoveExistingProducts. Return value 1.', '') + "`nAction ended 10:00:04: RemoveExistingProducts. Return value 1."
Reject { Assert-KwilRollbackFixtureLog 1603 $reordered $predecessor } 'successful predecessor removal'
Reject { Assert-KwilRollbackFixtureLog 1603 ($log+"`nAction ended 10:00:07: InstallFinalize. Return value 1.") $predecessor } 'committed installation'
Write-Output "Rollback fixture inert controls PASS: $count. Native rollback NOT RUN."
