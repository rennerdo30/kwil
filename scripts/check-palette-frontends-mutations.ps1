param([string[]]$Only = @())
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$proof = Join-Path 'E:/Development/_cache' ('kwil-palette-frontends-proof-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $proof | Out-Null
$sha = (git -C $root rev-parse HEAD).Trim()
$cases = @(
    @{Name='mouse-context';File='crates/ui-tui/src/palette_ui.rs';Old='let _ = input(view, host, keys, value, Duration::ZERO, Preset::VsCode);';New='let _ = host.input(value);';Package='kwil-ui-tui';Test='palette_ui::tests::palette_text_and_paste_cannot_mutate_document_in_vim_normal_mode'},
    @{Name='overlay-copy';File='crates/ui-gpu/src/palette_ui.rs';Old='open || displayed';New='open || false && displayed';Package='kwil-ui-gpu';Test='palette_ui::tests::dismissed_overlay_blocks_scroll_copy_until_restored'},
    @{Name='palette-paste';File='crates/core-commands/presets/vscode.toml';Old="keys = `"ctrl+v`"`ncommand = `"edit.paste`"`ncontext = `"palette`"";New="keys = `"ctrl+v`"`ncommand = `"edit.copy`"`ncontext = `"palette`"";Package='kwil-ui-gpu';Test='palette_ui::tests::palette_paste_binding_respects_unbind_and_pending_chords'},
    @{Name='tab-publication';File='crates/view-model/src/palette_session.rs';Old='let request = self.model.set_tabs(tabs.clone())?;';New='let request = self.model.set_tabs(self.tabs.clone())?;';Package='kwil-view-model';Test='palette_session::tests::tab_snapshot_replacement_invalidates_acceptance_and_publishes_new_tabs'}
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
Write-Output "Proof $proof"
