# Requires direct ROOT Cargo grant and exclusive source-reader hold.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'review' -TestTarget 'review_regressions' -Passed 1 -Failed 12 -Paths @('lib.rs', 'languages.rs', 'external.rs') -Base '3bae06c58fec0dbfbf3cef8f5c86da31e496d38d' -Prepare {
    Set-FormatterBase -Commit '3bae06c58fec0dbfbf3cef8f5c86da31e496d38d' -Paths @('lib.rs', 'languages.rs', 'external.rs')
}
