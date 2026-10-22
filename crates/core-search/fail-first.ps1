# Mutate only this package's owned files, prove each acceptance test detects a
# broken behavior, and restore the exact originals even if a check fails.
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$cases = @(
    @{ Test='tiny_chunks_and_work_windows_match_reference'; File='search.rs'; From='buf.copy_within(discard.., 0);'; To='buf.fill(0);' },
    @{ Test='case_word_selection_and_invalid_bytes'; File='search.rs'; From='.case_insensitive(true)'; To='.case_insensitive(false)' },
    @{ Test='count_progress_and_cancellation_stop_early'; File='search.rs'; From='result.count += 1;'; To='result.count += 0;' },
    @{ Test='capture_replace_plans_and_literal_dollars'; File='search.rs'; From='append(&mut out, capture.as_bytes())?;'; To='append(&mut out, b"BROKEN")?;' },
    @{ Test='filter_boolean_runs_and_incremental_partial_line'; File='filter.rs'; From='if self.expression.eval(&values) {'; To='if !self.expression.eval(&values) {' },
    @{ Test='filter_long_lines_are_bounded_and_runs_enforce_capacity'; File='filter.rs'; From='if self.expression.eval(&values) {'; To='if !self.expression.eval(&values) {' },
    @{ Test='files_gitignore_globs_previews_ranges_and_mapped_adapter'; File='files.rs'; From='walk.require_git(false).threads(workers).follow_links(false);'; To='walk.require_git(false).threads(workers).follow_links(false).git_ignore(false);' },
    @{ Test='files_cancellation_backpressure_and_disconnect'; File='files.rs'; From='self.hits.fetch_add(1, Ordering::Relaxed);'; To='self.hits.fetch_add(500, Ordering::Relaxed);' },
    @{ Test='files_oversize_lines_report_error_and_range_caps'; File='files.rs'; From='ranges_truncated = true;'; To='ranges_truncated = false;' },
    @{ Test='invalid_ranges_chunks_and_match_bounds_are_errors'; File='search.rs'; From='if m.len() > pattern.max_len {'; To='if false && m.len() > pattern.max_len {' },
    @{ Test='parallel_default_matches_serial_and_preserves_order'; File='parallel.rs'; From='s.count -= duplicates.get();'; To='s.count += 1;' }
)
foreach ($case in $cases) {
    $path = Join-Path $root "src/$($case.File)"
    $original = [IO.File]::ReadAllBytes($path)
    $source = [Text.Encoding]::UTF8.GetString($original)
    if (-not $source.Contains($case.From)) { throw "Mutation point missing for $($case.Test)" }
    try {
        [IO.File]::WriteAllText($path, $source.Replace($case.From, $case.To), [Text.UTF8Encoding]::new($false))
        & "$root/verify.ps1" -CargoArgs @('test','-p','kwil-core-search','-j','4','--locked',$case.Test,'--','--exact',"tests::$($case.Test)") -TimeoutSeconds 60
        if ($LASTEXITCODE -eq 0) { throw "Mutation survived: $($case.Test)" }
        Write-Output "FAIL-FIRST CONFIRMED: $($case.Test) (exit $LASTEXITCODE)"
    } finally {
        [IO.File]::WriteAllBytes($path, $original)
    }
}
