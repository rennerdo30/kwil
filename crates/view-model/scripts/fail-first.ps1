param([switch]$UnixOnly,[string[]]$Only)
# Each defect is compiled in a disposable source copy. No source mutation,
# worktree switching, full workspace tests, timing loops or cache cleanup.
$ErrorActionPreference = 'Stop'
$source = (Resolve-Path "$PSScriptRoot/../../..").Path
$cache = if ($IsWindows) { 'E:/Development/_cache/cargo-target/kwil-w2-editing-model-mutant' } else { Join-Path $source 'target/w2-a1-mutant' }
$scratch = Join-Path $cache ('proof-' + [guid]::NewGuid().ToString('N'))
$copy = Join-Path $scratch 'source'
New-Item -ItemType Directory -Path $copy -Force | Out-Null
foreach ($file in @('Cargo.toml','Cargo.lock','rust-toolchain.toml')) { Copy-Item -LiteralPath (Join-Path $source $file) -Destination $copy }
Copy-Item -LiteralPath (Join-Path $source 'crates') -Destination (Join-Path $copy 'crates') -Recurse
New-Item -ItemType Directory -Path (Join-Path $copy '.cargo') | Out-Null
Copy-Item -LiteralPath (Join-Path $source '.cargo/config.toml') -Destination (Join-Path $copy '.cargo/config.toml')
$env:CARGO_TARGET_DIR = Join-Path $cache 'target'
$env:CARGO_BUILD_JOBS = '4'
$summary = Join-Path $scratch 'summary.log'
$commit = & git -C $source rev-parse HEAD
"commit=$commit source_copy=$copy" | Set-Content -LiteralPath $summary
Get-ChildItem -LiteralPath (Join-Path $copy 'crates/core-text/src'),(Join-Path $copy 'crates/view-model/src'),(Join-Path $copy 'crates/core-workspace/src') -Recurse -File | Get-FileHash -Algorithm SHA256 | ForEach-Object { "$($_.Hash) $($_.Path.Substring($copy.Length))" } | Add-Content -LiteralPath $summary

function Mutate([string]$name,[string]$file,[string]$before,[string]$after,[string]$package,[string[]]$tests,[switch]$Ignored) {
    if ($Only -and $name -notin $Only) { return }
    $path = Join-Path $copy $file
    $original = [IO.File]::ReadAllText($path)
    $mutated = if ($original.Contains($before)) { $original.Replace($before,$after) } else {
        $pattern = (($before -split '\s+' | ForEach-Object { [regex]::Escape($_) }) -join '\s+')
        if (-not [regex]::IsMatch($original,$pattern)) { throw "Missing mutation anchor: $name" }
        [regex]::Replace($original,$pattern,[Text.RegularExpressions.MatchEvaluator]{ param($match) $after })
    }
    try {
        [IO.File]::WriteAllText($path,$mutated)
        $outPath = Join-Path $scratch "$name.out"
        $errPath = Join-Path $scratch "$name.err"
        "commit=$commit mutant=$name file=$file" | Set-Content -LiteralPath (Join-Path $scratch "$name.log")
        $args = @('test','--locked','--manifest-path',(Join-Path $copy 'Cargo.toml'),'-j','4')
        foreach ($pkg in $package.Split(',')) { $args += @('-p',$pkg) }
        if ($package.Contains(',')) { $args += '--no-fail-fast' }
        $args += @('--','--nocapture') + $tests
        if ($Ignored) { $args += '--ignored' }
        $startOptions = @{}
        if ($IsWindows) { $startOptions.WindowStyle = 'Hidden' }
        $process = Start-Process -FilePath (Get-Command cargo).Source -ArgumentList $args -WorkingDirectory $copy -RedirectStandardOutput $outPath -RedirectStandardError $errPath -PassThru @startOptions
        $receipt = "mutant=$name cargo_pid=$($process.Id) timeout_seconds=180"
        Write-Output $receipt; $receipt | Add-Content -LiteralPath $summary
        if (-not $process.WaitForExit(180000)) { $process.Kill($true); throw "Owned mutant cargo PID $($process.Id) timed out" }
        $process.Refresh()
        $stdout = [IO.File]::ReadAllText($outPath); $stderr = [IO.File]::ReadAllText($errPath)
        @($stdout,$stderr) | Add-Content -LiteralPath (Join-Path $scratch "$name.log")
        if ($process.ExitCode -eq 0 -or $stderr -match 'could not compile|error\[E') { throw "Mutant $name must compile and fail assertions: $stderr $stdout" }
        foreach ($test in $tests) { if ($stdout -notmatch ([regex]::Escape("test $test ... FAILED"))) { throw "Mutant $name did not fail $test : $stdout" } }
        $failed = ($stdout -split "`n" | Where-Object { $_ -match '^test .*FAILED|^test result:' }) -join "`n"
        Write-Output $failed; $failed | Add-Content -LiteralPath $summary
    } finally { [IO.File]::WriteAllText($path,$original) }
}

if ($UnixOnly) {
    if ($IsWindows) { throw 'Unix permission proof needs a Unix runner' }
    Mutate 'unix-permissions' 'crates/core-workspace/src/save.rs' 'fs::set_permissions(temp, original.permissions())' '{ use std::os::unix::fs::PermissionsExt; fs::set_permissions(temp, fs::Permissions::from_mode(0o777)) }' 'kwil-core-workspace' @('save::tests::unix_permissions_and_owner_survive_replace')
    Write-Output "Unix fail-first proof: $summary"
    exit 0
}

Mutate 'typing-payload' 'crates/view-model/src/editor.rs' 'let data: Arc<[u8]> = Arc::from(bytes);' 'let data: Arc<[u8]> = Arc::from(b"BROKEN".as_slice());' 'kwil-view-model' @(
    'tests::multicursor_typing_deletion_undo_redo_and_primary',
    'tests::typing_timeout_movement_and_history_cap_break_bursts',
    'tests::overlapping_selections_and_duplicate_cursors_edit_once',
    'tests::box_selection_replaces_short_lines_and_restores_selections',
    'tests::add_cursor_vertical_and_next_literal_occurrence',
    'tests::repeated_typing_at_touching_ranges_keeps_distinct_cursors',
    'tests::vim_insert_session_and_repeat_do_not_merge_unrelated_changes'
)
Mutate 'skip-delete' 'crates/view-model/src/editor.rs' 'self.document.buffer.delete(edit.range.clone())?;' 'self.document.buffer.delete(edit.range.start..edit.range.start)?;' 'kwil-view-model' @(
    'tests::character_word_delete_and_invalid_utf8_are_byte_exact',
    'tests::line_delete_duplicate_and_move_edges',
    'tests::multiple_line_moves_preserve_all_cursors_and_redo_positions',
    'tests::indent_outdent_and_mixed_comment_selection',
    'tests::trim_is_streamed_one_undo_and_markdown_is_preserved',
    'tests::vim_parser_dw_ciw_repeat_3dd_and_put_registers'
)
Mutate 'disable-autoindent' 'crates/view-model/src/editor.rs' 'if self.settings.editor.auto_indent {' 'if false && self.settings.editor.auto_indent {' 'kwil-view-model' @('tests::newline_autoindent_settings_and_crlf')
Mutate 'motion-count' 'crates/view-model/src/editor.rs' 'let count = u64::from(count);' 'let count = u64::from(count) * 0;' 'kwil-view-model' @('tests::unicode_motions_home_pages_columns_and_selection','tests::damage_single_row_cursor_old_new_selection_and_scroll')
Mutate 'drop-copy' 'crates/view-model/src/editor.rs' 'clipboard.set(self.selection_data()?)' '{ let _ = clipboard; Ok(()) }' 'kwil-view-model' @('tests::clipboard_cut_copy_paste_and_word_line_all_selection')
Mutate 'unknown-applied' 'crates/view-model/src/editor/commands.rs' 'CommandStatus::NotImplemented(format!("command not implemented: {id}"))' 'CommandStatus::Applied' 'kwil-view-model' @('tests::unknown_commands_status_and_command_motion_routing')
Mutate 'no-bracket-match' 'crates/view-model/src/editor.rs' 'if depth == 0 {' 'if depth == 999 {' 'kwil-view-model' @('tests::bracket_match_is_bounded_and_extends')
Mutate 'wrong-saved-revision' 'crates/view-model/src/editor/save.rs' 'self.document.saved_revision = saved.revision;' 'self.document.saved_revision = saved.revision.wrapping_add(1);' 'kwil-view-model' @('tests::saved_revision_and_concurrent_edits_keep_undo_and_dirty_state')
Mutate 'tab-columns' 'crates/view-model/src/grid.rs' 'tab_size.max(1) - column % tab_size.max(1)' '1' 'kwil-view-model' @('tests::frontend_positions_and_selection_ranges_use_unicode_and_tab_stops')
Mutate 'drop-pending-goto' 'crates/view-model/src/lib.rs' 'self.pending_goto = Some((line, args.column.unwrap_or(1), args.extend));' 'self.pending_goto = None;' 'kwil-view-model' @('tests::view_defers_goto_and_caches_bounded_long_line_windows')
Mutate 'full-undo-damage' 'crates/view-model/src/editor.rs' 'edited = entry.edited.clone();' 'edited = self.top..self.top + self.rows as u64;' 'kwil-view-model' @('tests::linewise_put_at_unterminated_eof_and_precise_undo_damage')
Mutate 'inverse-prefix-scan' 'crates/core-text/src/line_index.rs' 'if self.rank_checkpoints[j].1 > off {
            (cl, off) = self.rank_checkpoints[j];
        }
        let end = self.rank_checkpoints.get(j + 1).map_or(self.len, |&(_, at)| at);' 'let end = self.len;' 'kwil-core-text' @('line_index::tests::inverse_line_lookup_scans_one_byte_rank_block')
Mutate 'disable-coalescing' 'crates/core-text/src/buffer.rs' 'self.history.record(before, self.snapshot(), coalesce);' 'self.history.record(before, self.snapshot(), false);' 'kwil-core-text' @('buffer::editing_contract_tests::batch_cancel_coalescing_shared_ranges_and_checkpoint_keep_history')
Mutate 'foreign-budget-bypass' 'crates/core-text/src/piece_tree.rs' 'Storage::Memory { budget: source, .. } => !Arc::ptr_eq(source, budget),' 'Storage::Memory { budget: source, .. } => false,' 'kwil-core-text' @('buffer::editing_contract_tests::foreign_snapshot_paste_obeys_destination_resident_budget')
Mutate 'retain-foreign-original' 'crates/core-text/src/piece_tree.rs' '!piece.owner.as_ref().is_some_and(|source| Arc::ptr_eq(source, owner))' 'false' 'kwil-core-text' @('buffer::editing_contract_tests::tiny_foreign_original_pastes_release_resident_sources_under_zero_caps')
Mutate 'keep-deleted-originals' 'crates/core-text/src/buffer.rs' 'if !removed.is_empty() {' 'if false && !removed.is_empty() {' 'kwil-core-text' @('buffer::editing_contract_tests::deleting_last_original_byte_prunes_all_source_bookkeeping')
Mutate 'copy-own-original' 'crates/core-text/src/piece_tree.rs' '!piece.owner.as_ref().is_some_and(|source| Arc::ptr_eq(source, owner))' 'true' 'kwil-core-text,kwil-view-model' @('buffer::editing_contract_tests::immutable_slice_concat_keep_own_original_shared_and_counted_once','tests::whole_document_line_move_and_clipboard_reuse_keep_original_pieces_owned')
Mutate 'receipt-payload-range' 'crates/view-model/src/editor/receipts.rs' 'range: source }' 'range: 0..0 }' 'kwil-view-model' @('tests::receipts_multicursor_coalesced_undo_redo_are_exact','tests::receipts_bulk_lines_and_vim_operations_replay_atomically','tests::receipts_backpressure_retains_save_completion_and_stale_save_settles')
Mutate 'receipt-ignore-backpressure' 'crates/view-model/src/editor/receipts.rs' 'if self.has_pending_edit_receipt() {' 'if false && self.has_pending_edit_receipt() {' 'kwil-view-model' @('tests::receipts_backpressure_retains_save_completion_and_stale_save_settles')
Mutate 'receipt-stale-deferral' 'crates/view-model/src/editor/save.rs' 'if current && saved.trimmed_rows.is_some() && self.flush_edit_receipt().is_err() {' 'if saved.trimmed_rows.is_some() && self.flush_edit_receipt().is_err() {' 'kwil-view-model' @('tests::receipts_backpressure_retains_save_completion_and_stale_save_settles')
Mutate 'compound-publish-early' 'crates/view-model/src/editor.rs' 'if compound {
            let delta = self.document.buffer.line_count() as i128 - snapshot.line_count() as i128;' 'if false && compound {
            let delta = self.document.buffer.line_count() as i128 - snapshot.line_count() as i128;' 'kwil-view-model' @('tests::vim_compound_repeat_is_one_receipt_when_observer_is_full')
Mutate 'compound-drop-prior-damage' 'crates/view-model/src/editor/compound.rs' 'for row in &damage.rows {' 'for row in &Vec::<usize>::new() {' 'kwil-view-model' @('tests::vim_counted_open_repeat_preserves_structural_damage_and_one_undo')
Mutate 'compound-empty-recipe' 'crates/view-model/src/editor/compound.rs' 'recipe: Recipe::Explicit(Arc::new(compound.shapes)),' 'recipe: Recipe::Explicit(Arc::new(Vec::new())),' 'kwil-view-model' @('editor::compound::tests::composition_oracle_replays_mixed_intermediate_replacements','editor::compound::tests::contiguous_repeat_accepts_4096_count_and_refuses_larger_without_truncation')
Mutate 'compound-skip-rollback' 'crates/view-model/src/editor/compound.rs' 'self.document.buffer.cancel();' 'let _ = &self.document.buffer;' 'kwil-view-model' @('editor::compound::tests::failed_compound_restores_document_modes_cursors_registers_and_history')
Mutate 'compound-unbounded-spans' 'crates/view-model/src/editor/compound.rs' 'if self.shapes.len() >= MAX_CURSORS {' 'if false && self.shapes.len() >= MAX_CURSORS {' 'kwil-view-model' @('editor::compound::tests::disjoint_span_overflow_refuses_and_rolls_back_entire_command')
Mutate 'spool-leak' 'crates/core-workspace/src/save.rs' 'let _ = fs::remove_file(&self.path);' 'let _ = &self.path;' 'kwil-core-workspace' @('save::trim_spool::tests::trim_spool_offsets_are_lazy_reversible_and_removed_on_last_drop')
Mutate 'model-metadata-cap-bypass' 'crates/core-text/src/buffer.rs' 'self.history.charge_latest_model(bytes);' 'let _ = bytes;' 'kwil-core-text' @('buffer::editing_contract_tests::model_metadata_charge_is_idempotent_and_obeys_history_cap')
Mutate 'line-selection-boundary' 'crates/view-model/src/editor.rs' 'let owner = old.selection().start;' 'let owner = old.head;' 'kwil-view-model' @('tests::line_move_selection_end_boundary_owns_selected_line_and_restores_orientation')
Mutate 'unsafe-saved-baseline' 'crates/core-text/src/piece_tree.rs' 'pub(crate) fn saved_original(tree: &Tree) -> bool {' 'pub(crate) fn saved_original(tree: &Tree) -> bool { if tree.is_some() { return true; }' 'kwil-core-text' @('buffer::editing_contract_tests::saved_baseline_shortcuts_reject_foreign_resident_slices')
Mutate 'keep-invalid-cache' 'crates/core-text/src/file/read_batch.rs' 'admission.invalid |= admission.generation != file.shared.read_generation.load(Ordering::Relaxed);' 'admission.invalid |= false;' 'kwil-core-text' @('file::read_batch::tests::live_check_invalidates_outer_nested_and_fresh_commands','file::read_batch::tests::pin_generation_and_unwind_never_revive_nested_admission')
Mutate 'ignore-write-failure' 'crates/core-workspace/src/save.rs' 'let trimmed_rows = write(&request.snapshot, temp.file.as_mut().unwrap()).map_err(|e| save_error(&path, e))?;' 'let trimmed_rows = write(&request.snapshot, temp.file.as_mut().unwrap()).unwrap_or(None);' 'kwil-core-workspace' @('save::tests::failed_write_leaves_original_and_no_temp')
Mutate 'ignore-readonly' 'crates/core-workspace/src/save.rs' 'if m.permissions().readonly() {' 'if false && m.permissions().readonly() {' 'kwil-core-workspace' @('save::tests::read_only_error_is_clear')
Mutate 'resident-save-versions' 'crates/core-workspace/src/save.rs' 'let file = TextFile::open_saved_version(&path).map_err(io::Error::from)?;' 'let file = TextFile::open(&path).map_err(io::Error::from)?;' 'kwil-core-workspace' @('save::tests::repeated_small_saves_keep_versions_mapped_and_release_backings')
Mutate 'ignore-backing-cost' 'crates/core-text/src/file.rs' 'self.cost' '0' 'kwil-core-text' @('buffer::editing_contract_tests::history_charges_sparse_backings_once_per_snapshot_not_per_piece')
Mutate 'ui-thread-trim' 'crates/view-model/src/editor/save.rs' 'self.end_burst();' 'if self.settings.editor.trim_trailing_whitespace_on_save { self.trim_trailing_whitespace()?; } self.end_burst();' 'kwil-view-model' @('tests::trim_save_request_does_no_reads_and_worker_completion_is_undoable')
Mutate 'alias-untrimmed-save-revision' 'crates/view-model/src/editor/save.rs' 'self.document.saved_revision = self.document.next_revision;' 'self.document.saved_revision = saved.revision;' 'kwil-view-model' @('tests::concurrent_trim_save_never_marks_untrimmed_undo_version_clean')
if ($IsWindows) {
    Mutate 'leak-save-backing' 'crates/core-text/src/file/original_save.rs' 'let _ = fs::remove_file(path);' 'let _ = path;' 'kwil-core-workspace' @('save::tests::atomic_content_crlf_worker_snapshot_and_undo','save::tests::mapped_original_saves_and_snapshot_outlives_the_editor')
    Mutate 'drop-rebase' 'crates/view-model/src/editor/save.rs' 'self.document.buffer.checkpoint_current(saved.snapshot)?;' 'let _ = saved.snapshot;' 'kwil-view-model' @('tests::save_rebases_current_root_and_keeps_pre_save_worker_alive')
    Mutate 'cleanup-live-owner' 'crates/core-text/src/file/original_save.rs' '!process_gone(pid)' 'false' 'kwil-core-text' @('file::original_save::tests::stale_cleanup_checks_pid_identity_marker_and_recovery_path')
    Mutate 'rename-away-gap' 'crates/core-text/src/file/original_save.rs' '*guard = Some(backing);' '#[cfg(windows)] if let Some(path) = &backing.backup { fs::rename(&self.shared.path,path)?; } *guard = Some(backing);' 'kwil-core-workspace' @('save::tests::failed_replace_keeps_destination_present_and_intact')
}
# A single deliberately buffered save proves the allocation guard catches the
# exact principle-7 failure; all actual production saves above stream.
Mutate 'buffer-whole-save' 'crates/core-text/src/buffer.rs' 'for chunk in Chunks::new(&self.root, 0..self.len()) {' 'let mut whole = Vec::with_capacity(self.len() as usize); for chunk in Chunks::new(&self.root, 0..self.len()) { whole.extend_from_slice(chunk.map_err(io::Error::other)?); } writer.write_all(&whole)?; return Ok(()); for chunk in Chunks::new(&self.root, 0..self.len()) {' 'kwil-core-workspace' @('save::tests::streaming_100mb_mapped_save') -Ignored
Write-Output "All selected tests rejected compiling defects. Proof: $summary"
