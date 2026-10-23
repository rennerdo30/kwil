param([switch]$CheckAnchors)
# Run mutation execution only after the parent's Cargo lane grant. Each case starts
# from pristine owned source, changes one defect, and must fail its named assertion.
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$palettePath = Join-Path $root 'crates/core-commands/src/palette.rs'
$cataloguePath = Join-Path $root 'crates/core-workspace/src/catalogue.rs'
$palette = [IO.File]::ReadAllText($palettePath)
$catalogue = [IO.File]::ReadAllText($cataloguePath)
$commit = git -C $root rev-parse HEAD
$logs = Join-Path ([IO.Path]::GetTempPath()) ('kw24-isolated-mutants-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $logs | Out-Null
$summary = Join-Path $logs 'summary.txt'
"Commit under test: $commit; each mutant starts from pristine owned source" | Set-Content -LiteralPath $summary
function Mutate([string]$name, [string]$file, [string]$before, [string]$after, [string]$crate, [string]$target, [string]$test, [string]$panic) {
    $original = if ($file -eq 'palette') { $palette } else { $catalogue }
    $path = if ($file -eq 'palette') { $palettePath } else { $cataloguePath }
    $source = $original.Replace("`r`n", "`n")
    $before = $before.Replace("`r`n", "`n")
    $after = $after.Replace("`r`n", "`n")
    $anchorCount = [regex]::Matches($source, [regex]::Escape($before)).Count
    if ($anchorCount -ne 1) { throw "$name needs one unique mutation anchor, found $anchorCount" }
    if ($CheckAnchors) { Write-Output "$name anchor PASS"; return }
    try {
        # Both files are pristine before every independent mutation.
        [IO.File]::WriteAllText($palettePath, $palette)
        [IO.File]::WriteAllText($cataloguePath, $catalogue)
        [IO.File]::WriteAllText($path, $source.Replace($before, $after))
        $stdout = Join-Path $logs "$name.out"
        $stderr = Join-Path $logs "$name.err"
        $cargoArgs = @('test','--offline','--locked','-j','4','-p',$crate)
        $cargoArgs += if ($target -eq 'palette') { @('--test','palette') } else { @('--lib') }
        $cargoArgs += @($test,'--','--exact','--nocapture')
        $process = Start-Process cargo -ArgumentList $cargoArgs -WorkingDirectory $root -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        $line = "$name PID=$($process.Id) timeout=120s test=$test intended-panic=$panic"
        $line | Add-Content -LiteralPath $summary
        Write-Output $line
        if (!$process.WaitForExit(120000)) { $process.Kill($true); throw "$name timeout (own process tree closed)" }
        $outText = [IO.File]::ReadAllText($stdout)
        $errText = [IO.File]::ReadAllText($stderr)
        $header = "Commit under test: $commit; isolated mutant=$name; exact test=$test`n"
        [IO.File]::WriteAllText($stdout, $header + $outText)
        [IO.File]::WriteAllText($stderr, $header + $errText)
        if ($process.ExitCode -ne 101 -or $errText.Contains('could not compile') -or !$outText.Contains('running 1 test')) {
            throw "$name must compile and execute exactly one failing test`n$outText`n$errText"
        }
        if (!$outText.Contains("$test ... FAILED") -or !($outText + $errText).Contains($panic)) {
            throw "$name failed at the wrong assertion; required: $panic`n$outText`n$errText"
        }
        "PASS: $name rejected at intended assertion: $panic" | Add-Content -LiteralPath $summary
    } finally {
        [IO.File]::WriteAllText($palettePath, $palette)
        [IO.File]::WriteAllText($cataloguePath, $catalogue)
    }
}
$syncCommands = @"
if mode == PaletteMode::Commands {
    let commands: Vec<_> = COMMANDS.iter().map(|c| (format!("{} {} {}", c.id, c.title, c.category), PaletteRow { title: c.title.into(), category: c.category.into(), action: PaletteAction::Command(c.id) })).collect();
    let query = PaletteQuery { generation: self.generation, query: self.needle().into(), candidates: PaletteCandidates::Commands };
    self.rows = match_candidates(&commands, &query, self.limit, &matcher(), || false).unwrap().rows;
}
let candidates = match mode {
"@
$syncTabs = @"
if mode == PaletteMode::Tabs {
    let query = PaletteQuery { generation: self.generation, query: self.needle().into(), candidates: PaletteCandidates::Tabs(self.tabs.clone()) };
    self.rows = match_candidates(&[], &query, self.limit, &matcher(), || false).unwrap().rows;
}
let candidates = match mode {
"@
Mutate 'ui-commands' 'palette' 'let candidates = match mode {' $syncCommands 'kwil-core-commands' 'palette' 'commands_and_tabs_are_scheduled_without_ui_matching' 'cold command results must not be scored on UI'
Mutate 'ui-tabs' 'palette' 'let candidates = match mode {' $syncTabs 'kwil-core-commands' 'palette' 'commands_and_tabs_are_scheduled_without_ui_matching' 'cold tab results must not be scored on UI'
Mutate 'one-based-line' 'palette' 'let line = positive(parts.next()?)?;' 'let line = positive(parts.next()?)? + 1;' 'kwil-core-commands' 'palette' 'one_based_locations' 'one-based line/column action changed'
Mutate 'grapheme-backspace' 'palette' 'if let Some((start, _)) = self.query[..self.cursor].grapheme_indices(true).next_back() {' 'if let Some((start, _)) = self.query[..self.cursor].char_indices().next_back() {' 'kwil-core-commands' 'palette' 'grapheme_backspace_mode_transitions_wrap_and_caps' 'backspace must remove the whole joined emoji'
Mutate 'forward-delete' 'palette' 'self.query.replace_range(self.cursor..self.cursor + cluster.len(), "");' 'self.query.replace_range(self.cursor..self.cursor, "");' 'kwil-core-commands' 'palette' 'query_caret_edits_and_deletes_whole_graphemes' 'forward delete must remove the whole joined emoji'
Mutate 'query-cap' 'palette' 'if text.len() > MAX_QUERY_BYTES.saturating_sub(self.query.len()) {' 'if false {' 'kwil-core-commands' 'palette' 'query_limits_are_atomic_and_no_tabs_have_empty_selection' 'overlong insert must be rejected atomically'
Mutate 'fresh-action' 'palette' 'if !self.open || !self.fresh {' 'if !self.open {' 'kwil-core-commands' 'palette' 'stale_generations_cached_rows_and_cancel_cannot_accept_old_paths' 'cached stale row must not be accepted'
Mutate 'lifetime-generation' 'palette' '|| result.generation != self.generation' '|| false' 'kwil-core-commands' 'palette' 'reopening_identical_query_rejects_previous_lifetime_results' 'previous palette lifetime must not apply'
Mutate 'native-opening-path' 'palette' 'Self { path, label }' 'Self { path: PathBuf::from(path.to_string_lossy().as_ref()), label }' 'kwil-core-commands' 'palette' 'feed_bounds_paths_and_preserves_os_identity' 'native path identity changed in worker result'
Mutate 'latest-query-slot' 'palette' 'inbox.query = Some(query);' 'if inbox.query.is_none() { inbox.query = Some(query); }' 'kwil-core-commands' 'palette' 'newest_query_is_published_after_both_requests_are_submitted' 'newest query generation was not published after both requests were submitted'
Mutate 'changed-tab-ids' 'palette' 'self.cached[PaletteMode::Tabs as usize] = None;' '{}' 'kwil-core-commands' 'palette' 'changing_parent_tab_ids_invalidates_same_query_results' 'changed tab IDs retained cached selection'
Mutate 'tab-snapshot-copy' 'palette' 'PaletteMode::Tabs => PaletteCandidates::Tabs(self.tabs.clone()),' 'PaletteMode::Tabs => PaletteCandidates::Tabs(TabSnapshot(self.tabs.0.to_vec().into())),' 'kwil-core-commands' 'unit' 'palette::tests::tab_queries_share_validated_snapshot_without_copying_titles' 'editing copied the tab snapshot'
Mutate 'empty-cache' 'palette' 'self.cached[mode] = Some(result);' 'self.cached[mode] = None;' 'kwil-core-commands' 'palette' 'first_key_has_bounded_cached_files_and_cancel_stops_feed' 'first key has default cached rows'
Mutate 'feed-cancel' 'palette' 'if inbox.stopped || inbox.feed_cancelled {' 'if inbox.stopped {' 'kwil-core-commands' 'palette' 'first_key_has_bounded_cached_files_and_cancel_stops_feed' 'cancelled feed must reject new paths'
Mutate 'scoring-cancel' 'palette' "for path in paths {`n            if cancelled() {" "for path in paths {`n            if false {" 'kwil-core-commands' 'unit' 'palette::tests::cancellation_during_matching_discards_partial_snapshot' 'cancelled scoring must discard partial snapshot'
Mutate 'stale-publication' 'palette' 'if inbox.revision != revision || inbox.stopped {' 'if inbox.stopped {' 'kwil-core-commands' 'unit' 'palette::tests::superseded_worker_cannot_publish_even_if_cancellation_arrives_after_scoring' 'superseded worker must not publish'
Mutate 'result-cap' 'palette' 'keep_best(&mut rows, score, path.clone(), limit);' 'keep_best(&mut rows, score, path.clone(), usize::MAX);' 'kwil-core-commands' 'unit' 'palette::tests::capped_snapshot_counts_all_matches' 'worker snapshot exceeded result cap'
Mutate 'walk-cancel' 'catalogue' 'self.0.load(Ordering::Acquire)' 'false' 'kwil-core-workspace' 'unit' 'catalogue::tests::cancellation_stops_before_start_and_after_first_emit' 'catalogue continued after cancellation'
Mutate 'git-ignore' 'catalogue' '.require_git(false)' '.require_git(true)' 'kwil-core-workspace' 'unit' 'catalogue::tests::ignores_nested_paths_and_streams_exact_unicode_paths' 'root gitignore must hide matching files'
Mutate 'walk-capacity' 'catalogue' 'if report.files >= limits.max_files || bytes > limits.max_path_bytes.saturating_sub(report.path_bytes) {' 'if false {' 'kwil-core-workspace' 'unit' 'catalogue::tests::bounds_and_consumer_stop_are_explicit' 'catalogue file bound exceeded'
Mutate 'walk-native-path' 'catalogue' 'emit(entry.into_path())' 'emit(PathBuf::from(entry.path().to_string_lossy().as_ref()))' 'kwil-core-workspace' 'unit' 'catalogue::tests::depth_bound_and_os_path_identity_are_preserved' 'catalogue changed native path identity'
if ($CheckAnchors) { Write-Output 'Source-only anchor checks passed; no Cargo executed.' } else { Write-Output "All isolated defects rejected at intended assertions. Sources restored. Evidence: $summary" }
