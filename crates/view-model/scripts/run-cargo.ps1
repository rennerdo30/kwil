param(
    [Parameter(Mandatory=$true)][string[]]$CargoArgs,
    [string]$WorkingDirectory = (Resolve-Path "$PSScriptRoot/../../..").Path,
    [string]$TargetDirectory = 'E:/Development/_cache/cargo-target/kwil-w2-editing-model',
    [string]$LogName = 'check',
    [int]$TimeoutSeconds = 300
)
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = $TargetDirectory
$evidence = Join-Path $WorkingDirectory 'fixtures/w2-a1-evidence'
New-Item -ItemType Directory -Force -Path $evidence | Out-Null
$outPath = Join-Path $evidence "$LogName.out"
$errPath = Join-Path $evidence "$LogName.err"
$logPath = Join-Path $evidence "$LogName.log"
$commit = & git -C $WorkingDirectory rev-parse HEAD 2>$null
if (-not $commit) { $commit = 'deliberately-broken-copy' }
Set-Content -LiteralPath $logPath -Value "commit=$commit cargo $($CargoArgs -join ' ')"
$cargo = Start-Process -FilePath (Get-Command cargo).Source -ArgumentList $CargoArgs -WorkingDirectory $WorkingDirectory -WindowStyle Hidden -RedirectStandardOutput $outPath -RedirectStandardError $errPath -PassThru
Write-Output "cargo_pid=$($cargo.Id) timeout_seconds=$TimeoutSeconds log=$logPath"
if (-not $cargo.WaitForExit($TimeoutSeconds * 1000)) {
    Stop-Process -Id $cargo.Id
    throw "Owned cargo PID $($cargo.Id) exceeded timeout"
}
$cargo.Refresh()
Get-Content -LiteralPath $outPath,$errPath | Add-Content -LiteralPath $logPath
Get-Content -LiteralPath $logPath
exit $cargo.ExitCode
