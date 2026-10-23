# Requires direct ROOT Cargo grant and exclusive source-reader hold.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'round6' -TestTarget 'round6_regressions' -Passed 0 -Failed 4 -Paths @('lib.rs', 'literals.rs', 'languages.rs') -Base '72f87e00d09184882cd92753562fd03763f9f48f' -Prepare {
    Set-FormatterBase -Commit '72f87e00d09184882cd92753562fd03763f9f48f' -Paths @('lib.rs', 'literals.rs', 'languages.rs')
}
