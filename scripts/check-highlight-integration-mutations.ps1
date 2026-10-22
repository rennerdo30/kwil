param([string[]]$Only = @())
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$proof = Join-Path 'E:/Development/_cache' ('kwil-highlight-integration-proof-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $proof | Out-Null
$sha = (git -C $root rev-parse HEAD).Trim()
$cases = @(
    @{Name='bounded-windows';File='crates/view-model/src/highlight_session.rs';Old='end.saturating_sub(last.start) <= WINDOW';New='(true || end.saturating_sub(last.start) <= WINDOW)';Package='kwil-view-model';Test='highlight_session::tests::giant_line_gaps_and_long_windows_remain_bounded'},
    @{Name='highlight-row-damage';File='crates/view-model/src/highlight_session.rs';Old='if *previous != styles {';New='if true || *previous != styles {';Package='kwil-view-model';Test='highlight_session::tests::worker_results_style_visible_rows_and_only_repaint_changed_styles'},
    @{Name='source-retirement';File='crates/view-model/src/highlighting.rs';Old='*slot = Some(source);';New='drop(source);';Package='kwil-view-model';Test='highlighting::tests::idle_source_retirement_runs_on_the_existing_worker_and_wakes_host'},
    @{Name='terminal-color';File='crates/ui-tui/src/lib.rs';Old='style = style.fg(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));';New='let _ = rgb;';Package='kwil-ui-tui';Test='tests::syntax_foreground_preserves_selection_background_and_horizontal_bytes'}
)
$executed = 0
foreach($case in $cases) {
    if($Only.Count -and $case.Name -notin $Only) {continue}
    $executed += 1
    $file = Join-Path $root $case.File
    $original = [IO.File]::ReadAllBytes($file)
    $source = [IO.File]::ReadAllText($file)
    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
    if(([regex]::Matches($source,[regex]::Escape($case.Old))).Count -ne 1) {throw "Mutation anchor missing/ambiguous: $($case.Name)"}
    try {
        [IO.File]::WriteAllText($file,$source.Replace($case.Old,$case.New))
        [IO.File]::SetLastWriteTimeUtc($file,[DateTime]::UtcNow)
        $arguments = @('test','--offline','--locked','-j4','-p',$case.Package,'--lib',$case.Test,'--','--exact','--test-threads=1')
        $stdout=Join-Path $proof ($case.Name+'.stdout')
        $stderr=Join-Path $proof ($case.Name+'.stderr')
        $proc=Start-Process -FilePath (Get-Command cargo).Source -WorkingDirectory $root -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        "base $sha case $($case.Name) source $hash PID $($proc.Id)" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
        if(-not $proc.WaitForExit(180000)) {$proc.Kill();$proc.WaitForExit();throw "Owned mutant $($case.Name) timed out"}
        $out=[IO.File]::ReadAllText($stdout)
        if($proc.ExitCode -ne 101 -or $out -notmatch 'running 1 test' -or $out -notmatch ([regex]::Escape('test '+$case.Test+' ... FAILED')) -or $out -notmatch 'assertion') {throw "Expected compiling assertion failure missing: $($case.Name), exit $($proc.ExitCode), logs $proof"}
        "PASS witness $($case.Name), exit101 assertion failed" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
    } finally {
        [IO.File]::WriteAllBytes($file,$original)
        [IO.File]::SetLastWriteTimeUtc($file,[DateTime]::UtcNow)
        if((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $hash) {throw "Source restore hash failed: $file"}
    }
}
if($executed -eq 0) {throw 'No mutation cases selected; pass -Only as a PowerShell string array'}
Write-Output "Proof $proof"
