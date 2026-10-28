# Requires direct ROOT Cargo grant and exclusive source-reader hold.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'round3' -TestTarget 'round3_regressions' -Passed 0 -Failed 6 -Paths @('literals.rs', 'languages.rs') -Base 'f9dff6f0a3dc48aa516b4ba60d9149b4a69029dd' -Prepare {
    Set-FormatterBase -Commit 'f9dff6f0a3dc48aa516b4ba60d9149b4a69029dd' -Paths @('literals.rs', 'languages.rs')
}
