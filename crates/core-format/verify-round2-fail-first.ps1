. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'round2' -TestTarget 'round2_regressions' -Passed 1 -Failed 9 -Paths @('literals.rs', 'languages.rs', 'lib.rs', 'process_tree.rs') -Prepare {
    $path = Join-Path $PSScriptRoot 'src/literals.rs'
    $source = [IO.File]::ReadAllText($path)
    if (-not $source.Contains('let mut span = scanner.next();')) { throw 'Tokenizer mutation anchor missing' }
    [IO.File]::WriteAllText($path, $source.Replace('let mut span = scanner.next();', 'let mut span = None::<Range<usize>>;'))

    Set-FormatterBase -Commit 'a4350e4' -Paths @('languages.rs')

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

}
