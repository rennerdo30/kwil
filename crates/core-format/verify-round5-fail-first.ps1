# Requires direct ROOT Cargo grant and exclusive source-reader hold.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'round5' -TestTarget 'round5_regressions' -Passed 0 -Failed 3 -Paths @('literals.rs', 'languages.rs') -Base '4540af270c4e3b08387549f6f66c0d2ebbde9360' -Prepare {
    Set-FormatterBase -Commit '4540af270c4e3b08387549f6f66c0d2ebbde9360' -Paths @('literals.rs', 'languages.rs')
}
