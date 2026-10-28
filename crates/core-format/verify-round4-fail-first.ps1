# Requires direct ROOT Cargo grant and exclusive source-reader hold.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'round4' -TestTarget 'round4_regressions' -Passed 1 -Failed 3 -Paths @('literals.rs', 'languages.rs') -Base '9ade72ae2c5465c528a268a56d681c2592482466' -Prepare {
    Set-FormatterBase -Commit '9ade72ae2c5465c528a268a56d681c2592482466' -Paths @('literals.rs', 'languages.rs')
}
