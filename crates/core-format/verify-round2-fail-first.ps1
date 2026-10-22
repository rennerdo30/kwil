# Run only in this implementation job's isolated, idle worktree.
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-7-format'
$paths = @('literals.rs', 'languages.rs', 'lib.rs', 'process_tree.rs')
$saved = @{}
$outLog = "$env:CARGO_TARGET_DIR/round2-mutation.stdout"
$errLog = "$env:CARGO_TARGET_DIR/round2-mutation.stderr"
$log = "$env:CARGO_TARGET_DIR/round2-mutation.log"
$header = 'Commit under test: ' + (git rev-parse HEAD) + ' + deliberately broken round-2 source copy'
try {
    foreach ($name in $paths) {
        $path = Join-Path $PSScriptRoot "src/$name"
        $saved[$name] = [IO.File]::ReadAllBytes($path)
    }
    $path = Join-Path $PSScriptRoot 'src/literals.rs'
    $source = [IO.File]::ReadAllText($path)
    if (-not $source.Contains('let mut span = scanner.next();')) { throw 'Tokenizer mutation anchor missing' }
    [IO.File]::WriteAllText($path, $source.Replace('let mut span = scanner.next();', 'let mut span = None::<Range<usize>>;'))

    $path = Join-Path $PSScriptRoot 'src/languages.rs'
    $broken = git show 'a4350e4:crates/core-format/src/languages.rs'
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read broken CSS implementation' }
    [IO.File]::WriteAllText($path, ($broken -join "`n") + "`n")

    $path = Join-Path $PSScriptRoot 'src/lib.rs'
    $source = [IO.File]::ReadAllText($path)
    if (-not $source.Contains('let bytes = out.text.into_bytes();')) { throw 'Trim mutation anchor missing' }
    [IO.File]::WriteAllText($path, $source.Replace('let bytes = out.text.into_bytes();', 'let bytes = input.to_vec();'))

    $path = Join-Path $PSScriptRoot 'src/process_tree.rs'
    $source = [IO.File]::ReadAllText($path)
    $source = $source.Replace('JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;', '0;')
    $source = $source.Replace('windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1)', '1')
    $source = $source.Replace('libc::killpg(pgid, libc::SIGKILL)', '0')
    [IO.File]::WriteAllText($path, $source)

    [IO.File]::WriteAllText($log, "$header`n")
    Write-Output $header
    $process = Start-Process cargo -ArgumentList @('test', '-p', 'kwil-core-format', '-j', '4', '--test', 'round2_regressions', '--', '--nocapture') -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    Write-Output ('Owned cargo PID ' + $process.Id)
    if (-not $process.WaitForExit(90000)) { $process.Kill($true); $process.WaitForExit(); throw 'Mutation run timed out' }
    $output = [IO.File]::ReadAllText($outLog)
    $errors = [IO.File]::ReadAllText($errLog)
    [IO.File]::AppendAllText($log, "$output`n$errors")
    Write-Output $output
    Write-Output $errors
    if ($process.ExitCode -eq 0 -or $output -notmatch '1 passed; 9 failed') { throw 'Expected nine defect tests to fail and the unknown-type control to pass' }
} finally {
    foreach ($name in $saved.Keys) { [IO.File]::WriteAllBytes((Join-Path $PSScriptRoot "src/$name"), $saved[$name]) }
}
