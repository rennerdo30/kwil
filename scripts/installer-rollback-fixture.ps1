# Test-only authoring/evidence helpers. Dot sourcing performs no native work.
# The hosted lifecycle creates a SEPARATE MSI clone; release packages and guards
# have no fault flag, property, export or bypass.
. (Join-Path $PSScriptRoot 'windows-msi.ps1')

function Get-KwilRollbackFileHash([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Set-KwilRollbackFixtureTables {
    param($Tables, [string]$OriginalPackageCode, [string]$PackageCode)
    $guid = '^\{[0-9A-F]{8}-[0-9A-F]{4}-4[0-9A-F]{3}-[89AB][0-9A-F]{3}-[0-9A-F]{12}\}$'
    Assert-Kwil ($OriginalPackageCode -cmatch $guid -and
        $PackageCode -cmatch $guid -and $PackageCode -cne $OriginalPackageCode) 'rollback fixture requires a fresh PackageCode'
    Assert-Kwil ($Tables.KwilPackage.Rows.Count -eq 1 -and
        $Tables.KwilPackage.Rows[0][0] -eq 1 -and
        $Tables.KwilPackage.Rows[0][1] -eq 2 -and
        $Tables.KwilPackage.Rows[0][2] -eq 1 -and
        $Tables.KwilPackage.Rows[0][10] -eq 2 -and
        $Tables.KwilPackage.Rows[0][15] -ceq $OriginalPackageCode) 'rollback fixture package identity'
    Assert-Kwil ($Tables.CustomAction.Rows.Count -eq 2) 'rollback fixture requires the unmodified action set'
    $guard = @($Tables.CustomAction.Rows | Where-Object { $_[0] -ceq 'KwilPreflight' })
    Assert-Kwil ($guard.Count -eq 1 -and $guard[0][1] -eq 1 -and
        $guard[0][2] -ceq 'KwilPreflight' -and $guard[0][3] -ceq 'KwilPreflightV2') 'rollback fixture guard unchanged'
    foreach ($entry in @(@('KwilPreflight',1490),@('InstallInitialize',1500),@('RemoveExistingProducts',1501),@('ProcessComponents',1600))) {
        $row = @($Tables.InstallExecuteSequence.Rows | Where-Object { $_[0] -ceq $entry[0] })
        Assert-Kwil ($row.Count -eq 1 -and $row[0][2] -eq $entry[1]) 'rollback fixture requires the original transaction sequence'
    }
    Assert-Kwil (@($Tables.InstallExecuteSequence.Rows | Where-Object { $_[2] -eq 1502 -or $_[0] -ceq 'KwilRollbackProbe' }).Count -eq 0) 'rollback fixture slot already used'
    # No command, DLL or script executes here. Type 19 returns a fixed error.
    Add-KwilRow $Tables CustomAction @('KwilRollbackProbe',19,$null,'Kwil lifecycle fixture: failure after predecessor removal.')
    Add-KwilRow $Tables InstallExecuteSequence @('KwilRollbackProbe','NOT Installed AND KWIL_OLDER',1502)
    $Tables.KwilPackage.Rows[0][15] = $PackageCode
}

function New-KwilRollbackFixture {
    param($Installer, [string]$Source, [string]$Output, [string]$Architecture, [string]$OriginalPackageCode)
    Assert-Kwil ([IO.Path]::IsPathFullyQualified($Source) -and [IO.Path]::IsPathFullyQualified($Output) -and
        [IO.Path]::GetFullPath($Source) -ine [IO.Path]::GetFullPath($Output)) 'rollback fixture needs separate absolute paths'
    $sourceHash = Get-KwilRollbackFileHash $Source
    # File.Copy(overwrite:false) refuses an existing destination.
    [IO.File]::Copy($Source,$Output,$false)
    $packageCode = [Guid]::NewGuid().ToString('B').ToUpperInvariant()
    $tables = Get-KwilTableSpecs
    $db = $Installer.OpenDatabase($Output,1)
    try {
        $names = Read-KwilQuery $db 'SELECT `Name` FROM `_Tables`' @($false) 64
        Assert-Kwil ($names.Count -eq $tables.Count) 'rollback fixture table count'
        foreach ($name in $names) { Assert-Kwil ($tables.Contains($name[0])) 'rollback fixture unknown table' }
        foreach ($table in $tables.Keys) {
            $spec = $tables[$table]
            $columns = Read-KwilQuery $db ('SELECT `Number`,`Name`,`Type` FROM `_Columns` WHERE `Table`='''+$table+''' ORDER BY `Number`') @($true,$false,$true) 32
            Assert-KwilColumnSchema $table $spec $columns
            if ($table -cin @('Binary','Icon')) {
                foreach ($row in (Read-KwilQuery $db ('SELECT `Name` FROM `'+$table+'`') @($false) 2)) {
                    Add-KwilRow $tables $table @($row[0],$null)
                }
            } else {
                $columns = (@($spec.Columns | ForEach-Object { '`'+$_.Name+'`' }) -join ',')
                $integers = @($spec.Columns | ForEach-Object { $_.Type[0].ToString().ToLowerInvariant() -ceq 'i' })
                foreach ($row in (Read-KwilQuery $db ('SELECT '+$columns+' FROM `'+$table+'`') $integers 512)) {
                    Add-KwilRow $tables $table $row
                }
            }
        }
        Set-KwilRollbackFixtureTables $tables $OriginalPackageCode $packageCode
        # The fixed mutation contains no external SQL text. All other table
        # rows and the original canonical build/product identity are read back.
        Invoke-KwilSql $db ('UPDATE `KwilPackage` SET `PackageCode`='''+$packageCode+''' WHERE `Id`=1')
        Invoke-KwilSql $db "INSERT INTO ``CustomAction`` (``Action``,``Type``,``Target``) VALUES ('KwilRollbackProbe',19,'Kwil lifecycle fixture: failure after predecessor removal.')"
        Invoke-KwilSql $db "INSERT INTO ``InstallExecuteSequence`` (``Action``,``Condition``,``Sequence``) VALUES ('KwilRollbackProbe','NOT Installed AND KWIL_OLDER',1502)"
        $summary = $db.SummaryInformation(1)
        try {
            $null = $summary.GetType().InvokeMember('Property',[Reflection.BindingFlags]::SetProperty,$null,$summary,@(9,$packageCode))
            $null = $summary.Persist()
        } finally { Release-KwilCom $summary }
        $null = $db.Commit()
    } finally { Release-KwilCom $db }
    Assert-KwilDatabase $Installer $Output $tables $Architecture $packageCode
    Initialize-KwilNative
    foreach ($stream in @(
        @('cabinet','SELECT `Data` FROM `_Streams` WHERE `Name`=''kwil.cab''',1090519040),
        @('guard','SELECT `Data` FROM `Binary` WHERE `Name`=''KwilPreflight''',16777216),
        @('icon','SELECT `Data` FROM `Icon` WHERE `Name`=''KwilIcon''',536870912)
    )) {
        $before = $Output+'.original-'+$stream[0]
        $after = $Output+'.fixture-'+$stream[0]
        Export-KwilMsiStream $Source $stream[1] $before $stream[2]
        Export-KwilMsiStream $Output $stream[1] $after $stream[2]
        Assert-Kwil ((Get-Item -LiteralPath $before).Length -eq (Get-Item -LiteralPath $after).Length -and
            (Get-KwilRollbackFileHash $before) -ceq (Get-KwilRollbackFileHash $after)) 'rollback fixture changed an embedded stream'
    }
    Assert-Kwil ((Get-KwilRollbackFileHash $Source) -ceq $sourceHash) 'rollback fixture changed original MSI'
    return [pscustomobject]@{Path=$Output;PackageCode=$packageCode;SourceSha256=$sourceHash;Sha256=(Get-KwilRollbackFileHash $Output);FixtureOnly=$true}
}

function Assert-KwilRollbackFixtureLog {
    param([int]$ExitCode, [string]$Text, [string]$PredecessorProductCode)
    Assert-Kwil ($Text.Length -le 16777216) 'rollback evidence log bound'
    Assert-Kwil ($PredecessorProductCode -cmatch '^\{[0-9A-F-]{36}\}$') 'rollback predecessor identity'
    Assert-Kwil ($ExitCode -eq 1603) 'rollback fixture must fail naturally with 1603'
    Assert-Kwil ([regex]::Matches($Text,'(?m)^Action start [^\r\n]*: KwilPreflight\.\r?$').Count -ge 2) 'rollback requires incoming and original guards'
    Assert-Kwil ($Text -match ('(?m)^Property\([SC]\): KWIL_OLDER = '+[regex]::Escape($PredecessorProductCode)+'\r?$')) 'rollback exact predecessor was not selected'
    $cursor = 0
    foreach ($pattern in @(
        '(?m)^Action start [^\r\n]*: InstallInitialize\.\r?$',
        '(?m)^Action start [^\r\n]*: RemoveExistingProducts\.\r?$',
        '(?m)^Action ended [^\r\n]*: RemoveExistingProducts\. Return value 1\.\r?$',
        '(?m)^Action start [^\r\n]*: KwilRollbackProbe\.\r?$',
        'Kwil lifecycle fixture: failure after predecessor removal\.',
        '(?m)^Action ended [^\r\n]*: KwilRollbackProbe\. Return value 3\.\r?$'
    )) {
        $match = [regex]::Match($Text.Substring($cursor),$pattern)
        Assert-Kwil $match.Success 'rollback fault did not follow successful predecessor removal'
        $cursor += $match.Index + $match.Length
    }
    Assert-Kwil ($Text -notmatch '(?m)^Action ended [^\r\n]*: InstallFinalize\. Return value 1\.\r?$') 'rollback fixture committed installation'
    # This log is necessary, never sufficient: the hosted caller must also
    # verify all original bytes, registration and unknown data after rollback.
}
