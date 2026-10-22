param([string]$Case = '', [int]$TimeoutSeconds = 180, [switch]$VerifyRecipes)
$ErrorActionPreference = 'Stop'
# Run only under the coordinator's shared Cargo lane. Mutations are sequential,
# assertion-checked and restored byte-for-byte, including on failure. No Git
# reset/stash, lockfile update, cache clean, native run or network fetch occurs.
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$evidence = Join-Path $taskRoot 'fixtures/kw37-evidence'
[IO.Directory]::CreateDirectory($evidence) | Out-Null
$head = (& git -C $taskRoot rev-parse HEAD).Trim()
function Invoke-Focused([string]$Package, [string]$Test, [string]$Label, [bool]$ExpectFailure) {
    $taskInfo = [Diagnostics.ProcessStartInfo]::new('cargo')
    $taskInfo.WorkingDirectory = $taskRoot
    $taskInfo.UseShellExecute = $false
    $taskInfo.CreateNoWindow = $true
    $taskInfo.RedirectStandardOutput = $true
    $taskInfo.RedirectStandardError = $true
    foreach ($argument in @('test','--locked','--offline','-j','4','-p',$Package,'--lib',$Test,'--','--exact','--test-threads=1')) { $taskInfo.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $taskInfo
    if (-not $process.Start()) { throw 'Cargo launch failed' }
    $ownPid = $process.Id
    Write-Output "Started own cargo PID $ownPid, timeout ${TimeoutSeconds}s: $Label"
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        # Only the exact process tree launched above belongs to this check.
        $process.Kill($true)
        $process.WaitForExit()
        throw "Own cargo PID $ownPid timed out; stopped its own child process tree"
    }
    $exitCode = $process.ExitCode
    $log = "head=$head`nlabel=$Label`nownCargoPid=$ownPid`ncommand=cargo $($taskInfo.ArgumentList -join ' ')`n" + $stdout.Result + $stderr.Result
    [IO.File]::WriteAllText((Join-Path $evidence "$Label.log"),$log,[Text.UTF8Encoding]::new($false))
    $process.Dispose()
    if ($ExpectFailure) {
        if ($exitCode -eq 0 -or $log -notmatch 'running 1 test' -or $log -notmatch 'test result: FAILED' -or $log -match 'could not compile|error\[E\d+\]') {
            Write-Output $log
            throw "Mutant must compile independently and assertion-fail its named test: $Label"
        }
        Write-Output "ASSERTION-FAILED as required: $Label"
    } elseif ($exitCode -ne 0 -or $log -notmatch 'test result: ok') { Write-Output $log; throw "Focused check failed: $Label" }
}
function Mutant([string]$Label,[string]$File,[string]$Package,[string]$Test,[string]$Before,[string]$After) {
    if ($Case -and $Case -ne $Label) { return }
    $path = Join-Path $taskRoot $File
    $original = [IO.File]::ReadAllBytes($path)
    $source = [Text.Encoding]::UTF8.GetString($original)
    $index = $source.IndexOf($Before,[StringComparison]::Ordinal)
    if ($index -lt 0 -or $source.IndexOf($Before,$index + $Before.Length,[StringComparison]::Ordinal) -ge 0) { throw "Mutation must match exactly once: $Label" }
    if ($VerifyRecipes) { Write-Output "Recipe matched exactly once: $Label"; return }
    try {
        [IO.File]::WriteAllText($path,$source.Replace($Before,$After),[Text.UTF8Encoding]::new($false))
        Invoke-Focused $Package $Test $Label $true
    } finally { [IO.File]::WriteAllBytes($path,$original) }
}
$model = 'crates/core-commands/src/theme.rs'
$parse = 'crates/core-commands/src/theme_parse.rs'
$cache = 'crates/core-commands/src/theme_cache.rs'
$worker = 'crates/core-commands/src/theme_worker.rs'
$commands = 'kwil-core-commands'
Mutant 'rgba-alpha' $model $commands 'theme::tests::rgba_alpha_and_remote_background_contract' 'if hex.len() == 8 { byte(&hex[6..8])? } else { 255 }' 'if false { byte(&hex[6..8])? } else { 255 }'
Mutant 'remote-opacity' $model $commands 'theme::tests::rgba_alpha_and_remote_background_contract' 'if role.is_background() {' 'if false && role.is_background() {'
Mutant 'builtin-bg' $model $commands 'theme::tests::builtins_and_native_round_trip_contract' '0x0d1424, 0xd6e4f0, 0x0a101d, 0x5b6b85' '0x010203, 0xd6e4f0, 0x0a101d, 0x5b6b85'
Mutant 'source-byte-bound' $parse $commands 'theme::tests::native_parser_bounds_and_strictness_contract' 'if bytes.len() > MAX_THEME_BYTES {' 'if false && bytes.len() > MAX_THEME_BYTES {'
Mutant 'native-unknown-field' $parse $commands 'theme::tests::native_parser_bounds_and_strictness_contract' "#[serde(deny_unknown_fields)]`nstruct Native {" 'struct Native {'
Mutant 'scope-bound' $model $commands 'theme::tests::native_parser_bounds_and_strictness_contract' 'if self.syntax.len() > MAX_SCOPES {' 'if false && self.syntax.len() > MAX_SCOPES {'
Mutant 'scope-reset' $model $commands 'theme::tests::scope_cascade_and_explicit_reset_contract' 'if other.font_style.is_some() {' 'if other.font_style.is_some_and(|f| f != 0) {'
Mutant 'import-unsupported' $parse $commands 'theme::tests::vscode_import_rejects_unsupported_instead_of_discarding_contract' 'finish(&root)?;' 'let _ = &root;'
Mutant 'json-duplicate' $parse $commands 'theme::tests::vscode_import_rejects_unsupported_instead_of_discarding_contract' 'if out.insert(k, v.0).is_some() {' 'if out.insert(k, v.0).is_some() && false {'
Mutant 'xml-dtd' $parse $commands 'theme::tests::textmate_import_structure_and_entity_contract' 'Event::DocType(_) => return Err(error("tmTheme DTDs/entities are unsupported; remove the DOCTYPE")),' 'Event::DocType(_) => continue,'
Mutant 'cache-version' $cache $commands 'theme::tests::binary_cache_validated_round_trip_contract' 'u16::from_le_bytes(bytes[8..10].try_into().unwrap()) != VERSION' 'u16::from_le_bytes(bytes[8..10].try_into().unwrap()) == 65535'
Mutant 'cache-checksum' $cache $commands 'theme::tests::binary_cache_validated_round_trip_contract' 'if hash(body) != u64::from_le_bytes(bytes[14..22].try_into().unwrap()) {' 'if false && hash(body) != u64::from_le_bytes(bytes[14..22].try_into().unwrap()) {'
Mutant 'preview-cancel' $model $commands 'theme::tests::preview_cancel_apply_and_os_follow_contract' "pub fn cancel(&mut self) {`n        self.preview = None;" "pub fn cancel(&mut self) {`n        let _ = &self.preview;"
Mutant 'preview-apply' $model $commands 'theme::tests::preview_cancel_apply_and_os_follow_contract' 'self.selection = ThemeSelection::Named(id);' 'let _ = id;'
Mutant 'os-follow' $model $commands 'theme::tests::preview_cancel_apply_and_os_follow_contract' 'self.scheme = scheme;' 'let _ = scheme;'
Mutant 'catalogue-cap' $worker $commands 'theme_worker::tests::theme_worker_catalogue_bound_is_transactional_contract' 'if paths.len() + 2 > MAX_THEMES {' 'if paths.len() > MAX_THEMES {'
Mutant 'cache-admission' $worker $commands 'theme_worker::tests::theme_worker_obsolete_cache_write_is_skipped_contract' 'if shared.state.lock().unwrap_or_else(|p| p.into_inner()).desired.revision != wanted.revision {' 'if false && shared.state.lock().unwrap_or_else(|p| p.into_inner()).desired.revision != wanted.revision {'
Mutant 'snapshot-admission' $worker $commands 'theme_worker::tests::theme_worker_obsolete_completion_is_not_published_contract' 'if s.desired.revision != wanted.revision {' 'if false && s.desired.revision != wanted.revision {'
Mutant 'folder-children' 'crates/core-workspace/src/watcher.rs' 'kwil-core-workspace' 'watcher::tests::watcher_directory_child_contract_preserves_exact_file_filter' '|| directories && p.parent().is_some_and(|parent| same_path(parent, path))' '|| false && directories && p.parent().is_some_and(|parent| same_path(parent, path))'
Mutant 'import-no-clobber' $cache $commands 'theme_worker::tests::theme_worker_import_is_bounded_no_clobber_and_round_trips_contract' 'fs::hard_link(temp, path)?;' 'replace(temp, path)?;'
