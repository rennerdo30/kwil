# Run only the focused contract tests, in a disposable standalone crate copy.
# Each deliberate defect must compile and fail its named tests. The source
# worktree is never mutated. Output/logs stay in the branch's external cache.
$ErrorActionPreference = 'Stop'
$source = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cache = 'E:/Development/_cache/cargo-target/kwil-kw-25-commands-keymaps'
$scratch = Join-Path $cache ('fail-first-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$copy = Join-Path $scratch 'crate'
Copy-Item -LiteralPath $source -Destination $copy -Recurse
$manifest = Get-Content -LiteralPath (Join-Path $copy 'Cargo.toml') -Raw
$manifest = $manifest.Replace('version.workspace = true', 'version = "0.1.0"').Replace('edition.workspace = true', 'edition = "2024"').Replace('license.workspace = true', 'license = "MIT"')
[IO.File]::WriteAllText((Join-Path $copy 'Cargo.toml'), $manifest)
$base = git -C $source rev-parse HEAD
$summary = Join-Path $scratch 'summary.txt'
"Base under test: $base; disposable copy: $copy" | Set-Content -LiteralPath $summary
$env:CARGO_TARGET_DIR = $cache
$env:CARGO_BUILD_JOBS = '4'
function Run-Tests([string]$name, [string[]]$filters) {
    $out = Join-Path $scratch "$name.out"
    $err = Join-Path $scratch "$name.err"
    $arguments = @('test', '--offline', '--manifest-path', (Join-Path $copy 'Cargo.toml'), '-j', '4', '--test', 'contracts', '--') + $filters
    $process = Start-Process -FilePath (Get-Command cargo).Source -ArgumentList $arguments -PassThru -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError $err
    $line = "Run=$name PID=$($process.Id) timeout=120s"
    $line | Add-Content -LiteralPath $summary
    Write-Output $line
    if (!$process.WaitForExit(120000)) { $process.Kill($true); throw "$name timed out" }
    $stdout = Get-Content -LiteralPath $out -Raw
    $stderr = Get-Content -LiteralPath $err -Raw
    if ($process.ExitCode -eq 0 -or $stderr.Contains('could not compile')) {
        throw "$name did not produce the required assertion failure`n$stdout`n$stderr"
    }
    foreach ($filter in $filters) {
        if (!$stdout.Contains("test $filter ... FAILED")) { throw "Missing failure for $filter`n$stdout`n$stderr" }
    }
    $failed = ($stdout -split "`n" | Where-Object { $_ -match '^test .*FAILED|^test result:' }) -join "`n"
    $failed | Add-Content -LiteralPath $summary
    Write-Output $failed
}
function Mutate([string]$name, [string]$file, [string]$before, [string]$after, [string[]]$tests) {
    $path = Join-Path $copy $file
    $original = [IO.File]::ReadAllText($path)
    if (!$original.Contains($before)) { throw "Mutation anchor missing: $name" }
    try {
        [IO.File]::WriteAllText($path, $original.Replace($before, $after))
        "Defect=$name File=$file" | Add-Content -LiteralPath $summary
        Run-Tests $name $tests
    } finally { [IO.File]::WriteAllText($path, $original) }
}
Mutate 'registry-id' 'src/registry.rs' '"file.save" => "Save"' '"file.missing" => "Save"' @('registry_covers_v1_and_has_unique_palette_metadata', 'presets_load_and_reference_registered_commands')
Mutate 'shortcut-case' 'src/key.rs' 'parts.len() > 1' 'parts.len() > 99' @('chords_parse_valid_aliases_and_reject_invalid_input')
Mutate 'punctuation-shift' 'src/key.rs' 'modifiers.shift = false;' 'modifiers.shift = true;' @('adapters_normalize_logical_keys_without_conflating_ctrl_and_cmd')
Mutate 'drop-overrides' 'src/keymap.rs' 'for binding in bindings {' 'for binding in bindings.iter().take(0) {' @('overrides_add_rebind_unbind_and_validate_atomically', 'resolution_handles_sequences_contexts_modes_and_prefix_conflicts', 'settings_validate_values_and_apply_binding_overrides')
Mutate 'override-precedence' 'src/keymap.rs' 'e.layer,' '0,' @('overrides_add_rebind_unbind_and_validate_atomically')
Mutate 'motion-direction' 'src/vim.rs' "'h' => Some(Motion::Left)" "'h' => Some(Motion::Right)" @('vim_motions_and_counts_table')
Mutate 'operator-count' 'src/vim.rs' 'checked_mul(op.map_or(1, |o| o.count))' 'checked_add(op.map_or(1, |o| o.count))' @('vim_operators_counts_and_text_objects_table')
Mutate 'insert-mode' 'src/vim.rs' 'self.mode = Mode::Insert;' 'self.mode = Mode::Normal;' @('vim_modes_insert_visual_and_cancel')
Mutate 'repeat-command' 'src/vim.rs' "'.' => `"edit.repeat`"" "'.' => `"edit.undo`"" @('vim_repeat_search_and_ex_commands')
Mutate 'register-selection' 'src/vim.rs' 'self.register = Some(c);' "self.register = Some('z');" @('vim_registers_and_overflow_reset_pending_state')
Mutate 'ignore-vim-unbind' 'src/keymap.rs' 'let blocked = self.keymap.explicitly_unbound' 'let blocked = false && self.keymap.explicitly_unbound' @('engine_dispatches_overrides_before_vim_and_cancels_context_sequences')
Mutate 'unknown-diagnostics' 'src/settings.rs' 'unknown(&value, "", &mut unknown_keys);' 'unknown(&value, "", &mut unknown_keys); unknown_keys.clear();' @('settings_defaults_unknown_keys_and_typed_values')
Mutate 'reload-callback' 'src/settings.rs' '(self.callback)(&report);' 'if false { (self.callback)(&report); }' @('settings_reload_hook_keeps_last_valid_and_delivers_changes')
Mutate 'portable-path' 'src/settings.rs' 'folder.join("settings.toml")' 'folder.join("wrong.toml")' @('settings_paths_cover_portable_and_all_platforms_without_env_mutation')
Write-Output "All 16 contracts rejected their deliberately broken implementation. Sources restored in copy. Evidence: $summary"
