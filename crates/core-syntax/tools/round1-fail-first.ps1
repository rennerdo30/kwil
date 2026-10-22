# Deliberately break only owned implementation, require assertion failures, restore.
$ErrorActionPreference = 'Stop'
$runner = Join-Path $PSScriptRoot 'run-cargo.ps1'
$root = Get-Location
$original = @{}
foreach ($name in @('worker', 'language')) {
    $original[$name] = [IO.File]::ReadAllText((Join-Path $root "crates/core-syntax/src/$name.rs"))
}
function Write-Source($name, $text) {
    [IO.File]::WriteAllText((Join-Path $root "crates/core-syntax/src/$name.rs"), $text, [Text.UTF8Encoding]::new($false))
}
function Red([string[]]$TestArgs) {
    & $runner -CargoArgs (@('test','-p','kwil-core-syntax','-j','4','--locked') + $TestArgs) -TimeoutSeconds 60
    if ($LASTEXITCODE -ne 101) { throw "Expected assertion failure: $TestArgs, exit=$LASTEXITCODE" }
    Write-Output "EXPECTED RED: $TestArgs"
}
try {
    Write-Source 'worker' $original.worker.Replace('state.retired[index] = state.pending.take();', 'drop(state.pending.take());')
    Red @('--test','contracts','worker_retires_superseded_snapshot')
    Write-Source 'worker' $original.worker.Replace('let _state = self.shared.state.lock().unwrap();', 'let mut state = self.shared.state.lock().unwrap();').Replace('// Leave accepted snapshots/results for the worker to reclaim.', 'drop(state.pending.take());')
    Red @('--test','contracts','worker_retires_cancelled_snapshot')
    Write-Source 'worker' $original.worker.Replace('state.stopped = true;', 'state.stopped = true; drop(state.pending.take());')
    Red @('--test','contracts','worker_retires_dropped_snapshot')
    Red @('--lib','shutdown_retires_pending_on_worker')
    Write-Source 'worker' $original.worker
    Write-Source 'worker' $original.worker.Replace('RETIRE_CAPACITY: usize = 8', 'RETIRE_CAPACITY: usize = 64')
    & $runner -CargoArgs @('test','-p','kwil-core-syntax','-j','4','--locked','--lib','retirement_is_bounded_and_rejection_returns_ownership') -TimeoutSeconds 60
    if ($LASTEXITCODE -ne 101) { throw 'Expected saturation assertion failure' }
    Write-Source 'worker' $original.worker
    # Break the integer and decimal translations separately to prove both asserts.
    foreach ($pair in @(@('[0-9]+$', '%d+$'), @('[0-9]*\\.[0-9]*$', '%d*\\.%d*$'))) {
        $broken = $original.language.Replace($pair[0], $pair[1])
        if ($broken -eq $original.language) { throw 'SQL mutation did not apply' }
        Write-Source 'language' $broken
        & $runner -CargoArgs @('test','-p','kwil-core-syntax','-j','4','--locked','--test','contracts','sql_numeric_literals','--','--exact') -TimeoutSeconds 60
        if ($LASTEXITCODE -ne 101) { throw 'Expected SQL classification assertion failure' }
    }
} finally {
    foreach ($name in $original.Keys) { Write-Source $name $original[$name] }
}
Write-Output 'Round 1 mutations failed; source restored. Run the requested green checks.'
