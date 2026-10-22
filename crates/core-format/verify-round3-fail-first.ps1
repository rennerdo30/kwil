# Run only in the implementation job's isolated, idle worktree after Cargo lane admission.
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-7-format'
$base = 'f9dff6f0a3dc48aa516b4ba60d9149b4a69029dd'
$paths = @('literals.rs', 'languages.rs')
$saved = @{}
$outLog = "$env:CARGO_TARGET_DIR/round3-red.stdout"
$errLog = "$env:CARGO_TARGET_DIR/round3-red.stderr"
$log = "$env:CARGO_TARGET_DIR/round3-red.log"
$header = "Commit under test: $base (unchanged round-3-broken source copy; current regression fixtures)"
try {
    foreach ($name in $paths) {
        $path = Join-Path $PSScriptRoot "src/$name"
        $saved[$path] = [IO.File]::ReadAllBytes($path)
        $broken = git show "${base}:crates/core-format/src/$name"
        if ($LASTEXITCODE -ne 0) { throw "Cannot read broken base: $name" }
        [IO.File]::WriteAllText($path, ($broken -join "`n") + "`n")
    }
    [IO.File]::WriteAllText($log, "$header`n")
    Write-Output $header
    $process = Start-Process cargo -ArgumentList @('test', '-p', 'kwil-core-format', '-j', '4', '--test', 'round3_regressions', '--', '--nocapture') -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    Write-Output ('Owned cargo PID ' + $process.Id)
    if (-not $process.WaitForExit(90000)) { $process.Kill($true); $process.WaitForExit(); throw 'Fail-first run timed out' }
    $output = [IO.File]::ReadAllText($outLog)
    $errors = [IO.File]::ReadAllText($errLog)
    [IO.File]::AppendAllText($log, "$output`n$errors")
    Write-Output $output
    Write-Output $errors
    if ($process.ExitCode -eq 0 -or $output -notmatch '0 passed; 6 failed') { throw 'All six round-3 regression tests must reject the broken base' }
} finally {
    foreach ($path in $saved.Keys) { [IO.File]::WriteAllBytes($path, $saved[$path]) }
}
