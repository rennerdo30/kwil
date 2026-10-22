# Run only in the implementation job's isolated, idle worktree.
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-7-format'
$base = '3bae06c58fec0dbfbf3cef8f5c86da31e496d38d'
$paths = @('lib.rs', 'languages.rs', 'external.rs')
$saved = @{}
$outLog = 'E:/Development/_cache/cargo-target/kwil-kw7-review-fail-first.stdout'
$errLog = 'E:/Development/_cache/cargo-target/kwil-kw7-review-fail-first.stderr'
$evidenceLog = 'E:/Development/_cache/cargo-target/kwil-kw7-review-fail-first.log'
try {
    foreach ($name in $paths) {
        $path = Join-Path $PSScriptRoot "src/$name"
        $saved[$path] = [IO.File]::ReadAllBytes($path)
        $broken = git show "${base}:crates/core-format/src/$name"
        if ($LASTEXITCODE -ne 0) { throw "Cannot read broken base: $name" }
        [IO.File]::WriteAllText($path, ($broken -join "`n") + "`n")
    }
    $header = "Commit under test: $base (deliberately broken source copy; current regression fixtures)"
    Write-Output $header
    $process = Start-Process cargo -ArgumentList @('test', '-p', 'kwil-core-format', '-j', '4', '--test', 'review_regressions', '--', '--nocapture') -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    Write-Output ("Owned cargo PID " + $process.Id)
    if (-not $process.WaitForExit(90000)) { $process.Kill($true); $process.WaitForExit(); throw 'Mutation run timed out' }
    $output = [IO.File]::ReadAllText($outLog)
    $errors = [IO.File]::ReadAllText($errLog)
    [IO.File]::WriteAllText($evidenceLog, "$header`n$output`n$errors")
    Write-Output $output
    Write-Output $errors
    if ($process.ExitCode -eq 0 -or $output -notmatch '1 passed; 12 failed') {
        throw 'Expected all 12 defect tests to fail and the idempotence control to pass'
    }
} finally {
    foreach ($path in $saved.Keys) { [IO.File]::WriteAllBytes($path, $saved[$path]) }
}
