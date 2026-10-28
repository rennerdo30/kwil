# Direct finite parent Cargo grant/exclusive package-reader hold required.
# The admitted runner routes Cargo through pinned rustup and child-only RUSTC.
param([ValidateSet('Base', 'Sql', 'Shell')][string]$Case = 'Base')
. (Join-Path $PSScriptRoot 'verify-support.ps1')
$evidence = 'E:/AgentData/scratch/kwil-v1-wave-formatter-r10-05edc5f/harness'
$snapshot = 'E:/AgentData/scratch/kwil-v1-wave-formatter-r10-05edc5f/r9-snapshot'
$historicalHashes = @{
    'languages.rs' = '720E0BA6772A0FA57A87367905A4EF9CBCE5392DDD3F53523A618C73CC190283'
    'literals.rs' = '1F102F7800C0E6B6E06DE2EC17AD7B0195F520D8B0984D8B136D922C101F1A75'
}
$fault = switch ($Case) {
    'Base' { @{
        Paths = @('languages.rs', 'literals.rs'); Filter = ''; Failed = 4
        Base = 'exact previously validated R9 source snapshot on HEAD05edc5f8, frozen SHA provenance'
    } }
    'Sql' { @{
        Paths = @('languages.rs'); Filter = 'sql_'; Failed = 2
        Base = 'independent SQL CR boundary proof bypass'
        From = "if b.get(i) == Some(&b'\r') {"; To = "if false && b.get(i) == Some(&b'\r') {"
    } }
    'Shell' { @{
        Paths = @('literals.rs'); Filter = 'shell_'; Failed = 2
        Base = 'independent shell logical delimiter bypass using retained physical scanner'
        From = 'if self.kind == Kind::Shell {'; To = 'if self.kind == Kind::Shell && false {'
    } }
}
$before = @{}
foreach ($name in $fault.Paths) {
    $path = Join-Path $PSScriptRoot "src/$name"
    $before[$path] = (Get-FileHash -LiteralPath $path).Hash
}
try {
    $result = Invoke-FormatterWitness -Name "round10-$Case" -TestTarget 'round10_regressions' -Filter $fault.Filter -Passed 0 -Failed $fault.Failed -Paths $fault.Paths -Base $fault.Base -EvidenceDirectory $evidence -Prepare {
        foreach ($name in $fault.Paths) {
            $path = Join-Path $PSScriptRoot "src/$name"
            if ($Case -eq 'Base') {
                $saved = Join-Path $snapshot $name
                if ((Get-FileHash -LiteralPath $saved).Hash -ne $historicalHashes[$name]) { throw "R9 snapshot changed: $name" }
                [IO.File]::WriteAllBytes($path, [IO.File]::ReadAllBytes($saved))
            } else {
                $source = [IO.File]::ReadAllText($path)
                if ($source.Split([string[]]@($fault.From), [StringSplitOptions]::None).Count -ne 2) { throw 'R10 fault anchor missing/ambiguous' }
                [IO.File]::WriteAllText($path, $source.Replace($fault.From, $fault.To))
            }
        }
    }
    $result | Write-Output
    $combined = $result -join "`n"
    if ($combined -notmatch 'Compiling kwil-core-format') { throw 'R10 fault must freshly compile the formatter' }
    if ($Case -in @('Base', 'Sql') -and [regex]::Matches($combined, 'SQL literal bytes changed after CR comment').Count -ne 2) {
        throw 'R10 SQL witnesses must fail both significant-byte assertions'
    }
    if ($Case -in @('Base', 'Shell') -and [regex]::Matches($combined, 'shell raw body bytes changed after continued delimiter').Count -ne 2) {
        throw 'R10 shell witnesses must fail both significant-byte assertions'
    }
} finally {
    foreach ($path in $before.Keys) {
        $sha = (Get-FileHash -LiteralPath $path).Hash
        if ($sha -ne $before[$path]) { throw "R10 source restoration failed: $path" }
        $restoredTime = [IO.File]::GetLastWriteTimeUtc($path)
        [IO.File]::SetLastWriteTimeUtc($path, [DateTime]::UtcNow)
        if (Test-Path $evidence) {
            Add-Content (Join-Path $evidence 'restored-freshness.log') "Case=$Case SHA256=$sha exact-restored-mtime=$($restoredTime.ToString('o')) next-compile-mtime=$([IO.File]::GetLastWriteTimeUtc($path).ToString('o')) path=$path"
        }
    }
}
