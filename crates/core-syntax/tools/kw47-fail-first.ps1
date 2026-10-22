param(
    [string]$TargetDirectory = 'E:/Development/_cache/cargo-target/kwil',
    [string[]]$Only
)
# Run only after the parent's serial shared-cache Cargo START grant.
# One compiling mutant per isolated source copy, no additional Git worktrees.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot/../../..").Path
$commit = & git -C $root rev-parse HEAD
$evidence = Join-Path $root ('fixtures/kw47-evidence/fail-first-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $evidence -Force | Out-Null
"commit=$commit" | Set-Content -LiteralPath (Join-Path $evidence 'summary.log')

function Mutant([string]$name, [string]$file, [string]$before, [string]$after, [string]$package, [string]$test, [string]$integration = '') {
    if ($Only -and $name -notin $Only) { return }
    $copy = Join-Path $evidence $name
    New-Item -ItemType Directory -Path $copy | Out-Null
    foreach ($item in @('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', '.cargo', 'crates')) {
        Copy-Item -LiteralPath (Join-Path $root $item) -Destination $copy -Recurse
    }
    $path = Join-Path $copy $file
    $original = [IO.File]::ReadAllText($path)
    if (-not $original.Contains($before)) { throw "Missing mutation anchor: $name" }
    [IO.File]::WriteAllText($path, $original.Replace($before, $after))
    $log = Join-Path $evidence "$name.log"
    "commit=$commit mutant=$name source_copy=$copy test=$test" | Set-Content -LiteralPath $log
    Get-FileHash -LiteralPath $path -Algorithm SHA256 | ForEach-Object { "mutated_source_sha256=$($_.Hash)" } | Add-Content -LiteralPath $log
    $start = [Diagnostics.ProcessStartInfo]::new((Get-Command cargo).Source)
    $start.WorkingDirectory = $copy
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.Environment['CARGO_TARGET_DIR'] = $TargetDirectory
    $arguments = @('test', '--locked', '--offline', '-j', '4', '-p', $package)
    if ($integration) { $arguments += @('--test', $integration) } else { $arguments += '--lib' }
    $arguments += @($test, '--', '--exact', '--nocapture')
    foreach ($argument in $arguments) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    [void]$process.Start()
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $receipt = "mutant=$name cargo_pid=$($process.Id) timeout_seconds=180 target=$TargetDirectory"
    Write-Output $receipt
    $receipt | Add-Content -LiteralPath $log
    if (-not $process.WaitForExit(180000)) { $process.Kill($true); throw "Owned cargo PID $($process.Id) timed out" }
    $out = $stdout.GetAwaiter().GetResult()
    $err = $stderr.GetAwaiter().GetResult()
    @($out, $err) | Add-Content -LiteralPath $log
    if ($process.ExitCode -ne 101 -or $err -match 'could not compile|error\[E' -or $out -notmatch ([regex]::Escape("test $test ... FAILED"))) {
        throw "Mutant $name must compile and fail its assertion; see $log"
    }
    "EXPECTED RED: $name / $test" | Tee-Object -FilePath (Join-Path $evidence 'summary.log') -Append
}

Mutant 'precedence' 'crates/core-syntax/src/detection.rs' 'if let Some(language) = picker {' 'if let Some(language) = None::<Language> {' 'kwil-core-syntax' 'detection_precedence' 'kw47'
Mutant 'generic' 'crates/core-syntax/src/engine.rs' 'Ok(Highlights { revision, requested, queried, spans, mode, incremental })' 'Ok(Highlights { revision, requested, queried, spans: Vec::new(), mode, incremental })' 'kwil-core-syntax' 'generic_real_spans' 'kw47'
Mutant 'bounded-read' 'crates/core-syntax/src/detection.rs' 'FIRST_LINE_LIMIT: usize = 64 * 1024' 'FIRST_LINE_LIMIT: usize = 128 * 1024' 'kwil-core-syntax' 'bounded_first_line_read' 'kw47'
Mutant 'stale-document' 'crates/view-model/src/highlighting.rs' '|| key != current {' '|| (key.revision != current.revision || key.viewport_generation != current.viewport_generation) {' 'kwil-view-model' 'highlighting::tests::stale_document_id_result'
Mutant 'stale-viewport' 'crates/view-model/src/highlighting.rs' '|| key != current {' '|| (key.document_id != current.document_id || key.revision != current.revision) {' 'kwil-view-model' 'highlighting::tests::stale_viewport_result'
Mutant 'snapshot-chunk' 'crates/view-model/src/highlighting.rs' 'max_len.min(SOURCE_CHUNK_BYTES)' 'max_len' 'kwil-view-model' 'highlighting::tests::snapshot_chunks_are_borrowed_bounded_and_immutable'
Mutant 'parser-document' 'crates/view-model/src/highlighting.rs' 'engine.reset_document();' '/* deliberately retain another document tree */' 'kwil-view-model' 'highlighting::tests::same_revision_different_document_resets_parser'
Mutant 'retirement' 'crates/view-model/src/highlighting.rs' 'state.retired[index] = state.pending.take();' 'drop(state.pending.take());' 'kwil-view-model' 'highlighting::tests::latest_bounded_queue_retires_on_worker'
Mutant 'result-cap' 'crates/view-model/src/highlighting.rs' 'highlights.spans.len() > MAX_RESULT_SPANS' 'false' 'kwil-view-model' 'highlighting::tests::result_and_config_caps'
Write-Output "Fail-first evidence: $evidence"
