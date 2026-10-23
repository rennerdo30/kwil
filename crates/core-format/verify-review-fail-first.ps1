# Requires direct ROOT Cargo grant and exclusive source-reader hold.
# R9: the approved dollar-refusal expectation turns the old SQL idempotence
# control into a thirteenth behavioral failure on this unchanged historical base.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'review' -TestTarget 'review_regressions' -Passed 0 -Failed 13 -Paths @('lib.rs', 'languages.rs', 'external.rs') -Base '3bae06c58fec0dbfbf3cef8f5c86da31e496d38d' -Prepare {
    Set-FormatterBase -Commit '3bae06c58fec0dbfbf3cef8f5c86da31e496d38d' -Paths @('lib.rs', 'languages.rs', 'external.rs')
}
