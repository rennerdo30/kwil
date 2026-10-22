# Run in this isolated idle worktree only after the parent grants the Cargo lane.
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-7-format'
$base = '4540af270c4e3b08387549f6f66c0d2ebbde9360'
$saved = @{}
$outLog = "$env:CARGO_TARGET_DIR/round5-red.stdout"
$errLog = "$env:CARGO_TARGET_DIR/round5-red.stderr"
$log = "$env:CARGO_TARGET_DIR/round5-red.log"
$header = "Commit under test: $base (unchanged round-5-broken source copy; current regression fixtures)"
try {
    foreach ($name in @('literals.rs', 'languages.rs')) {
        $path = Join-Path $PSScriptRoot "src/$name"
        $saved[$path] = [IO.File]::ReadAllBytes($path)
        $broken = git show "${base}:crates/core-format/src/$name"
        if ($LASTEXITCODE -ne 0) { throw "Cannot read broken base: $name" }
        [IO.File]::WriteAllText($path, ($broken -join "`n") + "`n")
    }
    [IO.File]::WriteAllText($log, "$header`n")
    Write-Output $header
    $process = Start-Process cargo -ArgumentList @('test', '-p', 'kwil-core-format', '--locked', '-j', '4', '--test', 'round5_regressions', '--', '--nocapture') -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    Write-Output ('Owned cargo PID ' + $process.Id)
    if (-not $process.WaitForExit(90000)) { $process.Kill($true); $process.WaitForExit(); throw 'Fail-first run timed out' }
    $output = [IO.File]::ReadAllText($outLog)
    $errors = [IO.File]::ReadAllText($errLog)
    [IO.File]::AppendAllText($log, "$output`n$errors")
    Write-Output $output
    Write-Output $errors
    if ($process.ExitCode -eq 0 -or $output -notmatch '0 passed; 3 failed') { throw 'All three round-5 regressions must reject the broken base' }
} finally {
    foreach ($path in $saved.Keys) { [IO.File]::WriteAllBytes($path, $saved[$path]) }
}
