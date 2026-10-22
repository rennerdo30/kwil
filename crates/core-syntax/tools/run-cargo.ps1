param(
    [Parameter(Mandatory=$true)][string[]]$CargoArgs,
    [string]$Target = 'E:/Development/_cache/cargo-target/kwil-kw-26-syntax',
    [int]$TimeoutSeconds = 600
)
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = $Target
$start = [System.Diagnostics.ProcessStartInfo]::new('cargo')
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
foreach ($argument in $CargoArgs) { $start.ArgumentList.Add($argument) }
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $start
$timer = [System.Diagnostics.Stopwatch]::StartNew()
[void]$process.Start()
$stdout = $process.StandardOutput.ReadToEndAsync()
$stderr = $process.StandardError.ReadToEndAsync()
Write-Output "cargo PID=$($process.Id) target=$Target timeout=${TimeoutSeconds}s args=$CargoArgs"
if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
    $process.Kill($true)
    throw "Owned cargo process $($process.Id) exceeded timeout"
}
Write-Output $stdout.GetAwaiter().GetResult()
Write-Output $stderr.GetAwaiter().GetResult()
Write-Output "cargo exit=$($process.ExitCode) elapsed=$($timer.Elapsed.TotalSeconds.ToString('F3'))s"
exit $process.ExitCode
