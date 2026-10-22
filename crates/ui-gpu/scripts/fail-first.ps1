param([string]$Evidence = ('E:/Development/_cache/kwil-kw11-fail-first-' + [Guid]::NewGuid()),[string]$Only)
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path "$PSScriptRoot/../../..")
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-11-renderer'
New-Item -ItemType Directory -Path $Evidence -Force | Out-Null
$commit = git rev-parse HEAD
$cases = @(
    @{ File='shaping.rs'; Filter='shaping::tests::cursive_positions_preserve_joins_and_mark_offsets'; Anchor='total - before - advance'; Mutant='before'; Count=1 },
    @{ File='lib.rs'; Filter='font::tests::glyph_arrival_between_queued_scroll_and_paint_damages_moved_row'; Anchor='            font.scroll_rows(d.scroll, rows);'; Mutant='            let _ = (d.scroll, rows);'; Count=1 },
    @{ File='font.rs'; Filter='font::tests::queued_scroll_preserves_fixed_status_glyph_damage'; Anchor='        if *y >= visible * ch {'; Mutant='        if false && *y >= visible * ch {'; Count=1 },
    @{ File='shaping.rs'; Filter='shaping::tests::hebrew_arabic_and_marks_are_in_visual_cluster_order'; Anchor='if levels[range.start].is_rtl()'; Mutant='if false'; Count=1 },
    @{ File='shaping.rs'; Filter='shaping::tests::mixed_latin_rtl_and_digits_keep_directional_runs'; Anchor='if levels[range.start].is_rtl()'; Mutant='if false'; Count=1 },
    @{ File='shaping.rs'; Filter='shaping::tests::rtl_ligature_anchors_use_full_cluster_end'; Anchor='self.columns - self.before(end)'; Mutant='self.before(start)'; Count=1 },
    @{ File='font.rs'; Filter='font::tests::deferred_glyph_damage_follows_scroll_copy'; Anchor='    let shift ='; Mutant="    return;`n    let shift ="; Count=1 },
    @{ File='text.rs'; Filter='text::tests::'; Anchor='    let limit ='; Mutant="    return Vec::new();`n    let limit ="; Count=4 },
    @{ File='remote.rs'; Filter='remote::tests::overrides_and_live_session_change'; Anchor='    pub fn remote(&self) -> bool {'; Mutant="    pub fn remote(&self) -> bool {`n        return false;"; Count=1 },
    @{ File='remote.rs'; Filter='remote::tests::remote_disables_motion_blink_and_translucency'; Anchor='    pub fn remote(&self) -> bool {'; Mutant="    pub fn remote(&self) -> bool {`n        return false;"; Count=1 },
    @{ File='remote.rs'; Filter='remote::tests::linux_hints_do_not_mistake_local_wayland_for_remote'; Anchor='pub fn linux_remote_hints(get: impl Fn(&str) -> Option<String>) -> bool {'; Mutant="pub fn linux_remote_hints(get: impl Fn(&str) -> Option<String>) -> bool {`n    return false;"; Count=1 },
    @{ File='shaping.rs'; Filter='shaping::tests::fallback_cache_remembers_hits_and_misses'; Anchor='        if self.0.len() >= 4096 {'; Mutant="        self.0.clear();`n        if self.0.len() >= 4096 {"; Count=1 }
)
foreach ($case in $cases | Where-Object {-not $Only -or $_.Filter -eq $Only}) {
    $path = Join-Path $PWD ('crates/ui-gpu/src/' + $case.File)
    $source = [IO.File]::ReadAllText($path)
    if (-not $source.Contains($case.Anchor)) { throw "Mutation anchor missing: $($case.Filter)" }
    try {
        [IO.File]::WriteAllText($path, $source.Replace($case.Anchor, $case.Mutant))
        $output = & cargo test -p kwil-ui-gpu --lib --no-default-features --locked -j 4 $case.Filter 2>&1 | Out-String
        $exit = $LASTEXITCODE
        $log = "commit $commit (deliberate mutant $($case.Filter))`n$output"
        [IO.File]::WriteAllText((Join-Path $Evidence ($case.Filter.Replace(':','_') + '.txt')), $log)
        $failed = ([regex]::Matches($output, 'test [^\r\n]+ \.\.\. FAILED')).Count
        if ($exit -eq 0 -or $failed -ne $case.Count) { throw "Expected $($case.Count) failing assertions, got $failed (exit $exit): $output" }
        Write-Output "RED $($case.Filter): $failed assertion failures"
    } finally { [IO.File]::WriteAllText($path,$source) }
}
$output = & cargo test -p kwil-ui-gpu --lib --all-features --locked -j 4 2>&1 | Out-String
$exit = $LASTEXITCODE
[IO.File]::WriteAllText((Join-Path $Evidence 'restored.txt'), "commit $commit (restored)`n$output")
if ($exit -ne 0) { throw $output }
Write-Output $output
Write-Output "Evidence: $Evidence"
