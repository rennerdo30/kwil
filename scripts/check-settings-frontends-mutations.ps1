param([string]$EvidenceRoot = 'E:/Development/_cache')
$ErrorActionPreference = 'Stop'
$proof = Join-Path $EvidenceRoot ('kwil-settings-frontends-proof-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $proof | Out-Null
$cases = @(
    @{ Name='missing-file'; File='crates/view-model/src/settings_session.rs'; Old='if !self.is_dirty() && self.source_exists() {'; New='if !self.is_dirty() {'; Package='kwil-view-model'; Test='settings_file_creation_waits_for_ack_and_navigation_keeps_search_separate' },
    @{ Name='terminal-accept'; File='crates/ui-tui/src/settings_ui.rs'; Old='KeyCode::Enter => Some(SettingsInput::Accept),'; New='KeyCode::Enter => None,'; Package='kwil-ui-tui'; Test='terminal_settings_keyboard_edits_persist_and_render_without_replacing_search' },
    @{ Name='terminal-caret'; File='crates/ui-tui/src/settings_ui.rs'; Old='frame.set_cursor_position(cursor);'; New='let _ = cursor;'; Package='kwil-ui-tui'; Test='terminal_settings_keyboard_edits_persist_and_render_without_replacing_search' }
)
foreach ($case in $cases) {
    $path = Join-Path $PWD $case.File
    $bytes = [IO.File]::ReadAllBytes($path)
    $source = [Text.Encoding]::UTF8.GetString($bytes)
    if (($source.Split($case.Old).Count - 1) -ne 1) { throw "Mutation anchor missing or ambiguous: $($case.Name)" }
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    try {
        [IO.File]::WriteAllText($path, $source.Replace($case.Old, $case.New), [Text.UTF8Encoding]::new($false))
        $out = Join-Path $proof ($case.Name + '.stdout')
        $err = Join-Path $proof ($case.Name + '.stderr')
        $job = Start-Process cargo -ArgumentList @('test','--offline','--locked','-j','4','-p',$case.Package,'--lib',$case.Test,'--','--nocapture') -WindowStyle Hidden -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
        "PID $($job.Id) $($case.Name) source=$hash" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
        if (-not $job.WaitForExit(180000)) { Stop-Process -Id $job.Id; throw 'Cargo timeout' }
        $text = [IO.File]::ReadAllText($out) + [IO.File]::ReadAllText($err)
        if ($job.ExitCode -ne 101 -or $text -match 'error\[E\d+\]|could not compile' -or $text -notmatch 'assertion|assert!') { throw "Expected compiling assertion failure: $($case.Name), exit=$($job.ExitCode)" }
        "PASS compiling assertion failure: $($case.Name)" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
    } finally {
        [IO.File]::WriteAllBytes($path, $bytes)
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $hash) { throw 'Source restoration mismatch' }
    }
}
Write-Output "Evidence $proof"
