param(
    [Parameter(Mandatory=$true)][string[]]$CargoArgs,
    [int]$TimeoutSeconds = 180
)
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-22-search'
$env:CARGO_BUILD_JOBS = '4'
$out = Join-Path $env:TEMP "kwil-search-$PID-out.log"
$err = Join-Path $env:TEMP "kwil-search-$PID-err.log"
Write-Output "Commit: $(git rev-parse HEAD) (working tree); cargo $CargoArgs"
$process = Start-Process cargo -ArgumentList $CargoArgs -PassThru -WindowStyle Hidden -RedirectStandardOutput $out -RedirectStandardError $err
Write-Output "Started PID $($process.Id), timeout ${TimeoutSeconds}s"
if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
    Stop-Process -Id $process.Id
    throw "Timed out owned cargo PID $($process.Id)"
}
$process.Refresh()
Get-Content -LiteralPath $out
Get-Content -LiteralPath $err
exit $process.ExitCode
