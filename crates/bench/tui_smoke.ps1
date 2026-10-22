$ErrorActionPreference = 'Stop'
$env:KWIL_NO_UPDATE = '1'
New-Item -ItemType Directory -Force results | Out-Null
$exe = (Resolve-Path target/release/kwil.exe).Path
$fixture = (Resolve-Path fixtures/log-1g.log).Path
$p = Start-Process -FilePath $exe -ArgumentList @('--tui', '--trace-startup', $fixture) -WindowStyle Hidden -RedirectStandardError results/tui-trace.txt -PassThru
Write-Output "TUI PID $($p.Id)"
if (-not $p.WaitForExit(15000)) {
    Stop-Process -Id $p.Id
    throw 'TUI trace exceeded 15 seconds'
}
if ($p.ExitCode -ne 0) { throw "TUI failed: $($p.ExitCode)" }
$trace = Get-Content -Raw results/tui-trace.txt
foreach ($mark in @('launch', 'first_frame', 'index_done')) {
    if ($trace -notmatch "t_$mark=[0-9.]+ ms") { throw "Missing TUI trace mark: $mark" }
}
Write-Output $trace
