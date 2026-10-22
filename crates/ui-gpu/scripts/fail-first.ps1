param(
    [string]$Evidence = ('E:/Development/_cache/kwil-kw11-fail-first-' + [Guid]::NewGuid()),
    [string]$Only,
    [int]$TimeoutSeconds = 120
)
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path "$PSScriptRoot/../../..")
# Use the repository-configured shared Cargo cache. Never clean or bypass it.
New-Item -ItemType Directory -Path $Evidence -Force | Out-Null
$commit = git rev-parse HEAD
$cases = @(
    @{ Package='kwil-ui-gpu'; File='crates/ui-gpu/src/shaping.rs'; Filter='shaping::tests::cursive_positions_preserve_joins_and_mark_offsets'; Anchor='total - before - advance'; Mutant='before'; Count=1 },
    @{ Package='kwil-ui-gpu'; File='crates/ui-gpu/src/lib.rs'; Filter='font::tests::glyph_arrival_between_queued_scroll_and_paint_damages_moved_row'; Anchor='            font.scroll_rows(d.scroll, rows);'; Mutant='            let _ = (d.scroll, rows);'; Count=1 },
    @{ Package='kwil-ui-gpu'; File='crates/ui-gpu/src/font.rs'; Filter='font::tests::queued_scroll_preserves_fixed_status_glyph_damage'; Anchor='        if *y >= visible * ch {'; Mutant='        if false && *y >= visible * ch {'; Count=1 },
    @{ Package='kwil-ui-gpu'; File='crates/ui-gpu/src/shaping.rs'; Filter='shaping::tests::hebrew_arabic_and_marks_are_in_visual_cluster_order'; Anchor='if levels[range.start].is_rtl()'; Mutant='if false'; Count=1 },
    @{ Package='kwil-ui-gpu'; File='crates/ui-gpu/src/shaping.rs'; Filter='shaping::tests::mixed_latin_rtl_and_digits_keep_directional_runs'; Anchor='if levels[range.start].is_rtl()'; Mutant='if false'; Count=1 },
    @{ Package='kwil-ui-gpu'; File='crates/ui-gpu/src/shaping.rs'; Filter='shaping::tests::rtl_ligature_anchors_use_full_cluster_end'; Anchor='self.columns - self.before(end)'; Mutant='self.before(start)'; Count=1 },
    @{ File='crates/ui-gpu/src/font.rs'; Package='kwil-ui-gpu'; Filter='font::tests::deferred_glyph_damage_follows_scroll_copy'; Anchor='    let shift ='; Mutant="    return;`n    let shift ="; Count=1 },
    @{ File='crates/view-model/src/grid.rs'; Package='kwil-view-model'; Filter='grid::tests::cjk_and_combining_and_emoji_use_grid_columns'; Anchor='    let limit ='; Mutant="    return Vec::new();`n    let limit ="; Count=1 },
    @{ File='crates/view-model/src/grid.rs'; Package='kwil-view-model'; Filter='grid::tests::tabs_follow_configured_stops'; Anchor='    let limit ='; Mutant="    return Vec::new();`n    let limit ="; Count=1 },
    @{ File='crates/view-model/src/grid.rs'; Package='kwil-view-model'; Filter='grid::tests::invalid_bytes_are_visible_individually'; Anchor='    let limit ='; Mutant="    return Vec::new();`n    let limit ="; Count=1 },
    @{ File='crates/view-model/src/grid.rs'; Package='kwil-view-model'; Filter='grid::tests::viewport_clips_wide_clusters_and_unbounded_marks'; Anchor='    let limit ='; Mutant="    return Vec::new();`n    let limit ="; Count=1 },
    @{ File='crates/ui-gpu/src/remote.rs'; Package='kwil-ui-gpu'; Filter='remote::tests::overrides_and_live_session_change'; Anchor='    pub fn remote(&self) -> bool {'; Mutant="    pub fn remote(&self) -> bool {`n        return false;"; Count=1 },
    @{ File='crates/ui-gpu/src/remote.rs'; Package='kwil-ui-gpu'; Filter='remote::tests::remote_disables_motion_blink_and_translucency'; Anchor='    pub fn remote(&self) -> bool {'; Mutant="    pub fn remote(&self) -> bool {`n        return false;"; Count=1 },
    @{ File='crates/ui-gpu/src/remote.rs'; Package='kwil-ui-gpu'; Filter='remote::tests::linux_hints_do_not_mistake_local_wayland_for_remote'; Anchor='pub fn linux_remote_hints(get: impl Fn(&str) -> Option<String>) -> bool {'; Mutant="pub fn linux_remote_hints(get: impl Fn(&str) -> Option<String>) -> bool {`n    return false;"; Count=1 },
    @{ File='crates/ui-gpu/src/shaping.rs'; Package='kwil-ui-gpu'; Filter='shaping::tests::fallback_cache_remembers_hits_and_misses'; Anchor='        if self.0.len() >= 4096 {'; Mutant="        self.0.clear();`n        if self.0.len() >= 4096 {"; Count=1 }
)
function Invoke-Focused($package, $filter, $label, [switch]$AllFeatures) {
    $args = @('test', '-p', $package, '--lib', '--offline', '--locked', '-j', '4')
    if ($AllFeatures) { $args += '--all-features' } else { $args += '--no-default-features' }
    $args += $filter
    $name = ($label + '-' + $filter).Replace(':', '_')
    $stdout = Join-Path $Evidence ($name + '.stdout')
    $stderr = Join-Path $Evidence ($name + '.stderr')
    $process = Start-Process cargo -ArgumentList $args -NoNewWindow -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    [IO.File]::AppendAllText((Join-Path $Evidence 'pids.txt'), "commit $commit $label PID $($process.Id) timeout ${TimeoutSeconds}s`n")
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        $process.Kill($true)
        $process.WaitForExit()
        throw "Focused cargo timeout: owned PID $($process.Id), $filter"
    }
    $process.Refresh()
    $output = [IO.File]::ReadAllText($stdout) + [IO.File]::ReadAllText($stderr)
    [IO.File]::WriteAllText((Join-Path $Evidence ($name + '.txt')), "commit $commit ($label $package $filter)`n$output")
    return @{ Exit=$process.ExitCode; Output=$output }
}
# Each original contract must pass before its deliberately compiling mutant.
foreach ($case in $cases | Where-Object {-not $Only -or $_.Filter -eq $Only}) {
    $path = Join-Path $PWD $case.File
    $source = [IO.File]::ReadAllText($path)
    if (([regex]::Matches($source, [regex]::Escape($case.Anchor))).Count -ne 1) {
        throw "Mutation anchor missing or ambiguous: $($case.File) $($case.Filter)"
    }
    $baseline = Invoke-Focused $case.Package $case.Filter 'baseline'
    $passed = ([regex]::Matches($baseline.Output, 'test [^\r\n]+ \.\.\. ok')).Count
    if ($baseline.Exit -ne 0 -or $passed -ne $case.Count) {
        throw "Expected $($case.Count) baseline passes for $($case.Filter), got $passed (exit $($baseline.Exit)): $($baseline.Output)"
    }
    try {
        [IO.File]::WriteAllText($path, $source.Replace($case.Anchor, $case.Mutant))
        $result = Invoke-Focused $case.Package $case.Filter 'mutant'
        $failed = ([regex]::Matches($result.Output, 'test [^\r\n]+ \.\.\. FAILED')).Count
        # A build failure or an empty test selection cannot count as fail-first proof.
        if ($result.Exit -eq 0 -or $failed -ne $case.Count -or $result.Output -notmatch 'test result: FAILED') {
            throw "Expected $($case.Count) failing assertions, got $failed (exit $($result.Exit)): $($result.Output)"
        }
        Write-Output "RED $($case.Package) $($case.Filter): $failed assertion failures"
    } finally { [IO.File]::WriteAllText($path, $source) }
}
# Restore and verify the affected contracts with all features, without a full local suite.
foreach ($selection in @(
    @{ Package='kwil-view-model'; Filter='grid::tests::' },
    @{ Package='kwil-ui-gpu'; Filter='font::tests::' },
    @{ Package='kwil-ui-gpu'; Filter='remote::tests::' },
    @{ Package='kwil-ui-gpu'; Filter='shaping::tests::' }
)) {
    $result = Invoke-Focused $selection.Package $selection.Filter 'restored' -AllFeatures
    if ($result.Exit -ne 0 -or $result.Output -notmatch 'test [^\r\n]+ \.\.\. ok') { throw $result.Output }
    Write-Output "GREEN $($selection.Package) $($selection.Filter)"
}
Write-Output "Evidence: $Evidence"
