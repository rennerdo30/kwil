# Dot-source only. A direct parent Cargo grant and exclusive package-reader hold
# are prerequisites. Uses repository .cargo/config.toml without environment edits.
$ErrorActionPreference = 'Stop'

function Set-FormatterBase {
    param([string]$Commit, [string[]]$Paths)
    foreach ($name in $Paths) {
        $destination = Join-Path $PSScriptRoot "src/$name"
        $info = [Diagnostics.ProcessStartInfo]::new('git', "show ${Commit}:crates/core-format/src/$name")
        $info.UseShellExecute = $false
        $info.CreateNoWindow = $true
        $info.RedirectStandardOutput = $true
        $process = [Diagnostics.Process]::Start($info)
        Add-Content $script:FormatterWitnessLog "Owned git PID=$($process.Id) start=$([DateTime]::UtcNow.ToString('o'))"
        # Read asynchronously so the 10-second bound also covers output copying.
        $stream = [IO.MemoryStream]::new()
        try {
            $copy = $process.StandardOutput.BaseStream.CopyToAsync($stream)
            if (-not $process.WaitForExit(10000)) {
                $process.Kill($true)
                $process.WaitForExit()
                throw "Git object read timed out: $Commit $name"
            }
            $copy.GetAwaiter().GetResult()
            if ($process.ExitCode -ne 0) { throw "Cannot read broken source: $Commit $name" }
            [IO.File]::WriteAllBytes($destination, $stream.ToArray())
        } finally {
            Add-Content $script:FormatterWitnessLog "Owned git PID=$($process.Id) end=$([DateTime]::UtcNow.ToString('o')) exit=$($process.ExitCode)"
            $stream.Dispose()
            $process.Dispose()
        }
    }
}

function Invoke-FormatterWitness {
    param(
        [string]$Name,
        [string]$TestTarget,
        [int]$Passed,
        [int]$Failed,
        [string[]]$Paths,
        [scriptblock]$Prepare,
        [string]$Base = 'independent compiling mutation',
        [string]$Filter = '',
        [string]$EvidenceDirectory = 'E:/AgentData/scratch/kwil-v1-wave-formatter-cb48f62/harness'
    )
    if ($env:CARGO_TARGET_DIR -and $env:CARGO_TARGET_DIR.Replace('\','/').TrimEnd('/') -ne 'E:/Development/_cache/cargo-target/kwil') {
        throw 'Remove the alternate target-dir override before using the configured shared cache'
    }
    $evidence = Join-Path $EvidenceDirectory ($Name + '-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $evidence | Out-Null
    $head = git rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify HEAD' }
    $script:FormatterWitnessLog = Join-Path $evidence 'run.log'
    [IO.File]::WriteAllText($script:FormatterWitnessLog, "Commit under test: $head + owned formatter source snapshot; fault base=$Base`n")
    Get-ChildItem -LiteralPath $PSScriptRoot -Recurse -File | ForEach-Object {
        @{ Path = $_.FullName; SHA256 = (Get-FileHash -LiteralPath $_.FullName).Hash; Mtime = $_.LastWriteTimeUtc.ToString('o') }
    } | ConvertTo-Json | Set-Content (Join-Path $evidence 'candidate-sources.json')
    $saved = @{}
    $times = @{}
    $hashes = @{}
    foreach ($name in $Paths) {
        $path = Join-Path $PSScriptRoot "src/$name"
        $saved[$path] = [IO.File]::ReadAllBytes($path)
        $times[$path] = [IO.File]::GetLastWriteTimeUtc($path)
        $hashes[$path] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    }
    $hashes | ConvertTo-Json | Set-Content (Join-Path $evidence 'before-sha256.json')
    $process = $null
    try {
        & $Prepare
        foreach ($path in $saved.Keys) {
            Add-Content $script:FormatterWitnessLog "Fault source SHA256=$((Get-FileHash -LiteralPath $path).Hash) path=$path"
        }
        $outLog = Join-Path $evidence 'stdout.log'
        $errLog = Join-Path $evidence 'stderr.log'
        $arguments = @('test', '-p', 'kwil-core-format', '--offline', '--locked', '-j', '4', '--test', $TestTarget)
        if ($Filter) { $arguments += $Filter }
        $arguments += @('--', '--nocapture', '--test-threads=1')
        $process = Start-Process cargo -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
        $started = "Owned cargo PID=$($process.Id) start=$([DateTime]::UtcNow.ToString('o')) timeout=180s args=$arguments"
        Add-Content $script:FormatterWitnessLog $started
        Write-Output $started
        if (-not $process.WaitForExit(180000)) {
            $process.Kill($true)
            $process.WaitForExit()
            throw 'Fail-first process exceeded 180 seconds'
        }
        $output = [IO.File]::ReadAllText($outLog)
        $errors = [IO.File]::ReadAllText($errLog)
        Add-Content $script:FormatterWitnessLog "Owned cargo PID=$($process.Id) end=$([DateTime]::UtcNow.ToString('o')) exit=$($process.ExitCode)"
        [IO.File]::AppendAllText($script:FormatterWitnessLog, "$output`n$errors")
        Write-Output $output
        Write-Output $errors
        # Compiler errors, skipped/zero-case runs and timeouts cannot prove red.
        $summary = "test result: FAILED\. $Passed passed; $Failed failed; 0 ignored; 0 measured;"
        if ($process.ExitCode -ne 101 -or $errors -match 'could not compile|error\[E\d+\]' -or $output -notmatch $summary) {
            throw "Expected a compiling behavioral failure: passed=$Passed failed=$Failed; see $evidence"
        }
    } finally {
        # Restore exact bytes and timestamps even if preparation or Cargo fails.
        foreach ($path in $saved.Keys) {
            [IO.File]::WriteAllBytes($path, $saved[$path])
            [IO.File]::SetLastWriteTimeUtc($path, $times[$path])
        }
        foreach ($path in $saved.Keys) {
            $after = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
            Add-Content $script:FormatterWitnessLog "Restored SHA256=$after mtime=$([IO.File]::GetLastWriteTimeUtc($path).ToString('o')) path=$path"
            if ($after -ne $hashes[$path] -or [IO.File]::GetLastWriteTimeUtc($path) -ne $times[$path]) {
                throw "Exact source restoration failed: $path"
            }
        }
        if ($null -ne $process) { $process.Dispose() }
        Write-Output "Evidence: $evidence"
    }
}
