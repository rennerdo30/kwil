param([string]$R1Case='', [int]$TimeoutSeconds=180, [switch]$VerifyRecipes)
$ErrorActionPreference='Stop'
# Import the existing runner without executing any of its preserved 20 cases.
. (Join-Path $PSScriptRoot 'check-theme-mutations.ps1') -Case '__r1_definitions_only__' -TimeoutSeconds $TimeoutSeconds -VerifyRecipes:$VerifyRecipes
$Case=$R1Case
$commands='kwil-core-commands'
Mutant 'r1-native-inheritance' 'crates/core-commands/src/theme_parse.rs' $commands 'theme::tests::native_partial_style_inherits_builtin_properties_contract' 'theme.syntax.entry(scope).or_default().overlay(&supplied);' 'theme.syntax.insert(scope, supplied);'
Mutant 'r1-committed-fallback' 'crates/core-commands/src/theme_worker.rs' $commands 'theme_worker::tests::theme_worker_cancel_after_committed_source_deletion_restores_committed_contract' 'let active = active.unwrap_or_else(|| last_committed.clone());' 'let active = active.unwrap_or_else(|| s.snapshot.active.clone());'
Mutant 'r1-cancelled-session' 'crates/core-commands/src/theme_worker.rs' $commands 'theme_worker::tests::theme_worker_cancel_after_committed_source_deletion_restores_committed_contract' 'session: wanted.session,' 'session: if themes.iter().any(|t| &t.id == wanted.session.active_id()) { wanted.session } else { s.snapshot.session.clone() },'
if ($VerifyRecipes -or -not $IsWindows) {
    Mutant 'r1-cache-blocking-open' 'crates/core-commands/src/theme_cache.rs' $commands 'theme_cache::tests::cache_startup_refuses_fifo_and_symlinks_without_blocking_contract' 'options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);' 'options.custom_flags(libc::O_NOFOLLOW);'
    Mutant 'r1-cache-follow-symlink' 'crates/core-commands/src/theme_cache.rs' $commands 'theme_cache::tests::cache_startup_refuses_fifo_and_symlinks_without_blocking_contract' 'options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);' 'options.custom_flags(libc::O_NONBLOCK);'
} else {
    if ($R1Case -like 'r1-cache-*') { throw 'Unix FIFO/symlink subprocess witnesses cannot execute on Windows' }
    Write-Output 'NOT RUN on Windows: two Unix cache-open mutants and bounded FIFO/symlink subprocess contract'
}
