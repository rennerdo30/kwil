param([string[]]$Only = @())
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$proof = Join-Path 'E:/Development/_cache' ('kwil-integrated-ui-proof-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $proof | Out-Null
$sha = (git -C $root rev-parse HEAD).Trim()
$cases = @(
    @{Name='query-caret';File='crates/view-model/src/palette_session.rs';Old='while cursor >= columns && first < caret {';New='while false && cursor >= columns && first < caret {';Package='kwil-view-model';Test='palette_session::display_tests::query_window_keeps_unicode_caret_visible_and_escapes_controls'},
    @{Name='horizontal-roundtrip';File='crates/core-workspace/src/session.rs';Old='first_byte_column: if horizontal { r.u64()? } else { 0 },';New='first_byte_column: if horizontal { let _ = r.u64()?; 0 } else { 0 },';Package='kwil-core-workspace';Test='session::horizontal_codec_tests::horizontal_origin_round_trips_for_open_and_closed_tabs'},
    @{Name='zoom-dpi';File='crates/ui-gpu/src/zoom.rs';Old='base as f64 * scale *';New='base as f64 * 1.0 *';Package='kwil-ui-gpu';Test='zoom::tests::zoom_preserves_dpi_and_reset_uses_configured_size'}
)
foreach($case in $cases) {
    if($Only.Count -and $case.Name -notin $Only) {continue}
    $file = Join-Path $root $case.File
    $original = [IO.File]::ReadAllBytes($file)
    $source = [IO.File]::ReadAllText($file)
    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
    if(([regex]::Matches($source,[regex]::Escape($case.Old))).Count -ne 1) {throw "Mutation anchor missing/ambiguous: $($case.Name)"}
    try {
        [IO.File]::WriteAllText($file,$source.Replace($case.Old,$case.New))
        [IO.File]::SetLastWriteTimeUtc($file,[DateTime]::UtcNow)
        $args = @('test','--offline','--locked','-j4','-p',$case.Package,'--lib',$case.Test,'--','--exact','--test-threads=1')
        $stdout=Join-Path $proof ($case.Name+'.stdout')
        $stderr=Join-Path $proof ($case.Name+'.stderr')
        $proc=Start-Process -FilePath (Get-Command cargo).Source -WorkingDirectory $root -ArgumentList $args -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        "tested $sha case $($case.Name) source $hash PID $($proc.Id)" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
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
Write-Output "Proof $proof"
