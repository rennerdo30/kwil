# Source-only until direct parent Cargo grant and exclusive package-reader hold.
# Route Cargo through the approved pinned-toolchain/child-RUSTC runner, as in R7.
param([ValidateSet('Base', 'Sql', 'Php')][string]$Case = 'Base')
. (Join-Path $PSScriptRoot 'verify-support.ps1')
$evidence = 'E:/AgentData/scratch/kwil-v1-wave-formatter-r8-de1ef14/harness'
$historical = 'de1ef1486b94cc7c379041caa88214760d5ccfae'
$fault = switch ($Case) {
    'Base' { @{
        Paths = @('languages.rs', 'literals.rs'); Filter = ''; Failed = 4; Base = $historical
    } }
    'Sql' { @{
        Paths = @('languages.rs'); Filter = 'sql_'; Failed = 2; Base = 'independent universal SQL comment-nesting fault'
    } }
    'Php' { @{
        Paths = @('literals.rs'); Filter = 'php_'; Failed = 2; Base = 'independent unbounded PHP opening-prefix fault'
        From = 'long && boundary'; To = 'long'
    } }
}
$before = @{}
foreach ($name in $fault.Paths) {
    $path = Join-Path $PSScriptRoot "src/$name"
    $before[$path] = (Get-FileHash -LiteralPath $path).Hash
}
try {
    Invoke-FormatterWitness -Name "round8-$Case" -TestTarget 'round8_regressions' -Filter $fault.Filter -Passed 0 -Failed $fault.Failed -Paths $fault.Paths -Base $fault.Base -EvidenceDirectory $evidence -Prepare {
        if ($Case -eq 'Base') {
            Set-FormatterBase -Commit $historical -Paths $fault.Paths
        } else {
            $path = Join-Path $PSScriptRoot "src/$($fault.Paths[0])"
            $source = [IO.File]::ReadAllText($path)
            if ($Case -eq 'Sql') {
                $startAnchor = 'if sql && text[i..].starts_with("/*") {'
                $endAnchor = '} else if text[i..].starts_with("*/") {'
                $start = $source.IndexOf($startAnchor)
                if ($start -lt 0 -or $source.LastIndexOf($startAnchor) -ne $start) { throw 'SQL fault anchor missing/ambiguous' }
                $end = $source.IndexOf($endAnchor, $start)
                if ($end -lt $start) { throw 'SQL fault closing anchor missing' }
                $source = $source.Substring(0, $start) + $startAnchor + "`n                    nesting += 1;`n                    i += 2;`n                " + $source.Substring($end)
            } else {
                if ($source.Split([string[]]@($fault.From), [StringSplitOptions]::None).Count -ne 2) { throw 'PHP fault anchor missing/ambiguous' }
                $source = $source.Replace($fault.From, $fault.To)
            }
            [IO.File]::WriteAllText($path, $source)
        }
    }
} finally {
    # Common wrapper restores exact bytes/SHA/mtime, followed by a separately
    # recorded source-time advance to force normal freshness for repaired source.
    foreach ($path in $before.Keys) {
        $sha = (Get-FileHash -LiteralPath $path).Hash
        if ($sha -ne $before[$path]) { throw "R8 source restoration failed: $path" }
        $restoredTime = [IO.File]::GetLastWriteTimeUtc($path)
        [IO.File]::SetLastWriteTimeUtc($path, [DateTime]::UtcNow)
        if (Test-Path $evidence) {
            Add-Content (Join-Path $evidence 'restored-freshness.log') "Case=$Case SHA256=$sha exact-restored-mtime=$($restoredTime.ToString('o')) next-compile-mtime=$([IO.File]::GetLastWriteTimeUtc($path).ToString('o')) path=$path"
        }
    }
}
