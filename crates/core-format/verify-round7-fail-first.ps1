# Source-only until direct parent Cargo grant and exclusive package-reader hold.
# Each invocation admits one historical or independent compiling fault witness.
param([ValidateSet('Base', 'Sql', 'Php')][string]$Case = 'Base')
. (Join-Path $PSScriptRoot 'verify-support.ps1')
$evidence = 'E:/AgentData/scratch/kwil-v1-wave-formatter-r7-c8a173e/harness'
$historical = 'c8a173e0cbdaea5c87aed18c898c23e3bb8edb49'
$fault = switch ($Case) {
    'Base' { @{
        Paths = @('languages.rs', 'literals.rs'); Filter = ''; Failed = 4; Base = $historical
    } }
    'Sql' { @{
        Paths = @('languages.rs'); Filter = 'sql_'; Failed = 2; Base = 'independent SQL comment-boundary fault'
        From = 'if ambiguous_line_comment {'; To = 'if false && ambiguous_line_comment {'
    } }
    'Php' { @{
        Paths = @('literals.rs'); Filter = 'php_'; Failed = 2; Base = 'independent PHP halt-case fault'
        From = 'name.eq_ignore_ascii_case("__halt_compiler")'; To = 'name == "__halt_compiler"'
    } }
}
$before = @{}
foreach ($name in $fault.Paths) {
    $path = Join-Path $PSScriptRoot "src/$name"
    $before[$path] = (Get-FileHash -LiteralPath $path).Hash
}
try {
    Invoke-FormatterWitness -Name "round7-$Case" -TestTarget 'round7_regressions' -Filter $fault.Filter -Passed 0 -Failed $fault.Failed -Paths $fault.Paths -Base $fault.Base -EvidenceDirectory $evidence -Prepare {
        if ($Case -eq 'Base') {
            Set-FormatterBase -Commit $historical -Paths $fault.Paths
        } else {
            $path = Join-Path $PSScriptRoot "src/$($fault.Paths[0])"
            $source = [IO.File]::ReadAllText($path)
            if ($source.Split([string[]]@($fault.From), [StringSplitOptions]::None).Count -ne 2) {
                throw "Expected exactly one $Case fault anchor"
            }
            [IO.File]::WriteAllText($path, $source.Replace($fault.From, $fault.To))
        }
    }
} finally {
    # The common wrapper restores exact bytes/SHA/mtime. Then explicitly advance
    # only restored-source timestamps so a following positive run freshly builds
    # the repaired source instead of reusing a cached mutant executable.
    foreach ($path in $before.Keys) {
        $sha = (Get-FileHash -LiteralPath $path).Hash
        if ($sha -ne $before[$path]) { throw "R7 source restoration failed: $path" }
        $restoredTime = [IO.File]::GetLastWriteTimeUtc($path)
        [IO.File]::SetLastWriteTimeUtc($path, [DateTime]::UtcNow)
        if (Test-Path $evidence) {
            Add-Content (Join-Path $evidence 'restored-freshness.log') "Case=$Case SHA256=$sha exact-restored-mtime=$($restoredTime.ToString('o')) next-compile-mtime=$([IO.File]::GetLastWriteTimeUtc($path).ToString('o')) path=$path"
        }
    }
}
