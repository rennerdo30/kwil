# Run only after the parent's Cargo lane grant. Mutations touch this branch's
# two new, owned files only and are restored in finally. No cache override/clean.
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$palettePath = Join-Path $root 'crates/core-commands/src/palette.rs'
$cataloguePath = Join-Path $root 'crates/core-workspace/src/catalogue.rs'
$palette = [IO.File]::ReadAllText($palettePath)
$catalogue = [IO.File]::ReadAllText($cataloguePath)
$commit = git -C $root rev-parse HEAD
$logs = Join-Path ([IO.Path]::GetTempPath()) ('kw24-fail-first-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $logs | Out-Null
$summary = Join-Path $logs 'summary.txt'
"Commit under test: $commit plus owned KW24 source mutations" | Set-Content -LiteralPath $summary
function Replace-Checked([string]$source, [string]$before, [string]$after) {
    if (!$source.Contains($before)) { throw "Mutation anchor missing: $before" }
    return $source.Replace($before, $after)
}
function Reject-Defect([string]$name, [string[]]$cargoArgs, [string[]]$expected) {
    $stdout = Join-Path $logs "$name.out"
    $stderr = Join-Path $logs "$name.err"
    $process = Start-Process cargo -ArgumentList $cargoArgs -WorkingDirectory $root -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $line = "$name PID=$($process.Id) timeout=120s"
    $line | Add-Content -LiteralPath $summary
    Write-Output $line
    if (!$process.WaitForExit(120000)) { Stop-Process -Id $process.Id; throw "$name timeout" }
    $outText = [IO.File]::ReadAllText($stdout)
    $errText = [IO.File]::ReadAllText($stderr)
    $header = "Commit under test: $commit plus owned KW24 source mutations`n"
    [IO.File]::WriteAllText($stdout, $header + $outText)
    [IO.File]::WriteAllText($stderr, $header + $errText)
    if ($process.ExitCode -eq 0 -or $errText.Contains('could not compile')) {
        throw "$name must compile and fail assertions`n$outText`n$errText"
    }
    foreach ($test in $expected) {
        if (!$outText.Contains("$test ... FAILED")) { throw "Expected failing contract missing: $test`n$outText`n$errText" }
    }
    ($outText -split "`n" | Where-Object { $_ -match '^test .*FAILED|^test result:' }) | Add-Content -LiteralPath $summary
}
try {
    $broken = Replace-Checked $palette "Some(b':') => PaletteMode::Line," "Some(b':') => PaletteMode::Files,"
    $broken = Replace-Checked $broken 'self.query[..self.cursor].grapheme_indices(true).next_back()' 'self.query[..self.cursor].char_indices().next_back()'
    $broken = Replace-Checked $broken '|| result.generation != self.generation' '|| false'
    $broken = Replace-Checked $broken 'if cancelled() {' 'if false {'
    $broken = Replace-Checked $broken 'if inbox.revision != revision || inbox.stopped {' 'if inbox.stopped {'
    $broken = Replace-Checked $broken 'Self { path, label }' 'Self { path: PathBuf::from(path.to_string_lossy().as_ref()), label }'
    $broken = Replace-Checked $broken 'if text.len() > MAX_QUERY_BYTES.saturating_sub(self.query.len()) {' 'if false {'
    $broken = Replace-Checked $broken 'inbox.query = Some(query);' 'if inbox.query.is_none() { inbox.query = Some(query); }'
    $broken = Replace-Checked $broken 'if at < limit {' 'if true {'
    $broken = Replace-Checked $broken 'if rows.len() > limit {' 'if false {'
    $broken = Replace-Checked $broken 'if !self.open || !self.fresh {' 'if !self.open {'
    [IO.File]::WriteAllText($palettePath, $broken)
    Reject-Defect 'palette-input' @('test','--offline','--locked','-j','4','-p','kwil-core-commands','--test','palette') @('modes_commands_and_one_based_locations','grapheme_backspace_mode_transitions_wrap_and_caps','reopening_identical_query_rejects_previous_lifetime_results','feed_bounds_paths_and_preserves_os_identity','query_limits_are_atomic_and_no_tabs_have_empty_selection','query_caret_edits_and_deletes_whole_graphemes','worker_matches_bounded_results_and_newest_query_wins','stale_generations_cached_rows_and_cancel_cannot_accept_old_paths')
    Reject-Defect 'palette-worker' @('test','--offline','--locked','-j','4','-p','kwil-core-commands','--lib','palette::tests') @('cancellation_during_matching_discards_partial_snapshot','superseded_worker_cannot_publish_even_if_cancellation_arrives_after_scoring','capped_snapshot_counts_all_matches_without_copying_catalogue')
    $brokenCatalogue = Replace-Checked $catalogue 'if cancel.is_cancelled() {' 'if false {'
    $brokenCatalogue = Replace-Checked $brokenCatalogue '.require_git(false)' '.require_git(true)'
    $brokenCatalogue = Replace-Checked $brokenCatalogue 'if report.files >= limits.max_files || bytes > limits.max_path_bytes.saturating_sub(report.path_bytes) {' 'if false {'
    $brokenCatalogue = Replace-Checked $brokenCatalogue 'emit(entry.into_path())' 'emit(PathBuf::from(entry.path().to_string_lossy().as_ref()))'
    [IO.File]::WriteAllText($cataloguePath, $brokenCatalogue)
    Reject-Defect 'catalogue' @('test','--offline','--locked','-j','4','-p','kwil-core-workspace','--lib','catalogue::tests') @('cancellation_stops_before_start_and_after_first_emit','ignores_nested_paths_and_streams_exact_unicode_paths','bounds_and_consumer_stop_are_explicit','depth_bound_and_os_path_identity_are_preserved')
} finally {
    [IO.File]::WriteAllText($palettePath, $palette)
    [IO.File]::WriteAllText($cataloguePath, $catalogue)
}
Write-Output "Sources restored. Fail-first evidence: $summary"
