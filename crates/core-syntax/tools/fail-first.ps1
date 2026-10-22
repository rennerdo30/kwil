# Deliberately break only owned source, run acceptance contracts, always restore.
$ErrorActionPreference = 'Stop'
$root = (Get-Location).Path
$runner = Join-Path $PSScriptRoot 'run-cargo.ps1'
$files = @('engine', 'language', 'worker')
$original = @{}
foreach ($file in $files) { $original[$file] = [IO.File]::ReadAllText("$root/crates/core-syntax/src/$file.rs") }
function Write-Source($file, $text) { [IO.File]::WriteAllText("$root/crates/core-syntax/src/$file.rs", $text, [Text.UTF8Encoding]::new($false)) }
function Expect-Failure([string[]]$Filters) {
    foreach ($filter in $Filters) {
        & $runner -CargoArgs @('test', '-p', 'kwil-core-syntax', '-j', '4', '--locked', '--test', 'contracts', $filter, '--', '--exact') -TimeoutSeconds 60
        if ($LASTEXITCODE -eq 0) { throw "Mutation survived: $filter" }
        if ($LASTEXITCODE -ne 101) { throw "Unexpected cargo exit: $LASTEXITCODE for $filter" }
        Write-Output "EXPECTED RED: $filter"
    }
}
try {
    $empty = $original.engine.Replace('Ok(Highlights { revision, requested, queried, spans, mode, incremental })', 'Ok(Highlights { revision, requested, queried, spans: Vec::new(), mode, incremental })')
    if ($empty -eq $original.engine) { throw 'Empty-spans mutation did not apply' }
    Write-Source 'engine' $empty
    Expect-Failure @('rust_ranges', 'python_ranges', 'json_ranges', 'markdown_ranges', 'incremental_changed_region_equals_fresh', 'fragmented_text_and_range_clipping', 'lexical_tables', 'huge_source_is_window_bounded', 'worker_cancels_stale_work_and_keeps_previous')
    Write-Source 'engine' $original.engine
    $broken = $original.language.Replace('let name = path.as_ref()', 'return None; let name = path.as_ref()')
    if ($broken -eq $original.language) { throw 'Detection mutation did not apply' }
    Write-Source 'language' $broken
    Expect-Failure @('detection')
    Write-Source 'language' $original.language
    $broken = $original.language.Replace('highlights: std::borrow::Cow::Borrowed(&$module::HIGHLIGHTS_QUERY)', 'highlights: std::borrow::Cow::Borrowed("(")')
    if ($broken -eq $original.language) { throw 'Query mutation did not apply' }
    Write-Source 'language' $broken
    Expect-Failure @('every_bundled_query_compiles')
    Write-Source 'language' $original.language
    Write-Source 'engine' $original.engine.Replace('Error::Cancelled', 'Error::Closed')
    Expect-Failure @('cancel_before_source_read')
    Write-Source 'engine' $original.engine.Replace('if queried.len() > self.config.max_window_bytes {', 'if false {')
    Expect-Failure @('rejects_oversized_window_and_bad_source')
    Write-Source 'engine' $original.engine
    $broken = $original.worker.Replace('let cancelled = || worker.generation.load(Ordering::Acquire) != job.generation;', 'let cancelled = || false;')
    if ($broken -eq $original.worker) { throw 'Cancellation mutation did not apply' }
    Write-Source 'worker' $broken
    Expect-Failure @('worker_cancels_stale_work_and_keeps_previous')
} finally {
    foreach ($file in $files) { Write-Source $file $original[$file] }
}
Write-Output 'All 13 contracts failed on deliberate implementation mutations; sources restored.'
