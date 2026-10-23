param([string]$OutputDirectory = 'E:/Development/_cache/kwil-app-mutants', [string[]]$Only)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$commit = (& git -C $root rev-parse HEAD).Trim()
$proof = Join-Path $OutputDirectory ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $proof | Out-Null
$summary = Join-Path $proof 'summary.log'
"commit=$commit independent_mutants=true target=configured_shared_cache" | Set-Content $summary
$cases = @(
    @{Name='positions';File='file_argument.rs';Test='file_argument::tests::cli_positions_keep_native_paths_and_positive_coordinates';Before='line = Some(number);';After='line = Some(number + 1);'},
    @{Name='literal-path';File='file_argument.rs';Test='file_argument::tests::cli_positions_keep_native_paths_and_positive_coordinates';Before='if !literal && let Some((prefix, number)) = numeric_suffix(bytes)? {';After='if let Some((prefix, number)) = numeric_suffix(bytes)? {'},
    @{Name='native-name';File='file_argument.rs';Test= $(if($IsWindows){'file_argument::tests::cli_positions_preserve_unpaired_windows_surrogates'}else{'file_argument::tests::cli_positions_preserve_non_unicode_unix_names'});Before='let path = unsafe { OsStr::from_encoded_bytes_unchecked(&bytes[..end]) };';After='let lossy = String::from_utf8_lossy(&bytes[..end]); let path = OsStr::new(lossy.as_ref());'},
    @{Name='relative-directory';File='file_argument.rs';Test='file_argument::tests::cli_positions_resolve_without_file_system_access';Before='self.path = resolve_relative(&self.path, directory)?;';After='self.path = directory.join("wrong-file");'},
    @{Name='owner-error';File='instance.rs';Test='instance::tests::instance_errors_never_treat_busy_or_foreign_owners_as_absent';Before='io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused';After='io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut'},
    @{Name='full-request';File='instance.rs';Test='instance::tests::instance_delivery_retries_the_exact_bounded_request_and_stops_on_close';Before='request = retained;';After='if true { return true; } request = retained;'},
    @{Name='shutdown';File='instance.rs';Test='instance::tests::instance_delivery_retries_the_exact_bounded_request_and_stops_on_close';Before='if stopped.load(Ordering::Acquire) {';After='if false {'},
    @{Name='runtime-root';File='instance.rs';Test='instance::tests::instance_runtime_override_does_not_fall_back_when_malformed';Before='let root = runtime.or(home).ok_or_else';After='let root = runtime.filter(|p| p.is_absolute()).or(home).ok_or_else'},
    @{Name='claim-race';File='instance.rs';Test='instance::tests::instance_claim_race_waits_only_for_unadmitted_absence';Before='Err(error) if absent_owner(&error) => wait(),';After='Err(error) if absent_owner(&error) => return Err(error),'},
    @{Name='fallback-route';File='instance.rs';Test='instance::tests::instance_disconnected_gui_retains_request_until_tui_route_arrives';Before='Err(TrySendError::Disconnected(retained)) => {';After='Err(TrySendError::Disconnected(retained)) => { if true { return false; }'},
    @{Name='hidden-root';File='lib.rs';Test='isolation_tests::hidden_terminal_missing_empty_and_relative_roots_fail_closed';Before='if hidden && root.is_none_or(|root| !root.is_absolute()) {';After='if false {'}
)
if($IsWindows) {
    $cases += @{Name='drive-relative';File='file_argument.rs';Test='file_argument::tests::cli_drive_relative_paths_resolve_with_positions_without_opening_files';Before='self.path = resolve_relative(&self.path, directory)?;';After='self.path = directory.join(&self.path);'}
}
foreach($case in $cases) {
    if($Only -and $case.Name -notin $Only){continue}
    $path=Join-Path $root ('crates/app/src/'+$case.File)
    $original=[IO.File]::ReadAllBytes($path)
    $text=[IO.File]::ReadAllText($path)
    if(!$text.Contains($case.Before)){throw "Mutation anchor missing: $($case.Name)"}
    $hash=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    $base=Join-Path $proof $case.Name
    "commit=$commit mutant=$($case.Name) original_sha256=$hash exact_test=$($case.Test)" | Set-Content ($base+'.log')
    $process=$null
    try {
        [IO.File]::WriteAllText($path,$text.Replace($case.Before,$case.After))
        $arguments=@('test','--offline','--locked','-j4','-p','kwil','--lib','--features','windows-console',$case.Test,'--','--exact','--test-threads=1')
        $process=Start-Process cargo -ArgumentList $arguments -WorkingDirectory $root -NoNewWindow -PassThru -RedirectStandardOutput ($base+'.out') -RedirectStandardError ($base+'.err')
        "pid=$($process.Id)" | Tee-Object -FilePath ($base+'.log') -Append
        if(!$process.WaitForExit(60000)){Stop-Process -Id $process.Id; throw "Mutant timed out: $($case.Name)"}
        $output=[IO.File]::ReadAllText($base+'.out')+[IO.File]::ReadAllText($base+'.err')
        if($process.ExitCode -ne 101 -or $output -notmatch 'test result: FAILED' -or $output -notmatch 'assertion|winner paused before bind|fallback must not permanently|same-drive relative path must resolve') {
            throw "Mutant did not compile and assertion-fail: $($case.Name), exit=$($process.ExitCode)"
        }
        "assertion_failed=$($case.Name) exit=$($process.ExitCode)" | Tee-Object -FilePath $summary -Append
    } finally {
        if($process -and !$process.HasExited){Stop-Process -Id $process.Id}
        [IO.File]::WriteAllBytes($path,$original)
        if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $hash){throw 'Byte restoration failed'}
    }
}
"Restored all source bytes. Evidence: $summary"
