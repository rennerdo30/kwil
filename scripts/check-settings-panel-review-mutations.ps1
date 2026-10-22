param([string[]]$Only = @(), [string]$EvidenceRoot = 'E:/Development/_cache')
$ErrorActionPreference = 'Stop'
$proof = Join-Path $EvidenceRoot ('kwil-settings-panel-review-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $proof | Out-Null
$head = git rev-parse HEAD
$oldSelection = @'
let window = panel.window(doc, 0, 0).map_err(|e| e.to_string())?;
            let target = index.min(window.total.saturating_sub(1));
            for _ in 0..window.selection.abs_diff(target) {
                let input = if target > window.selection { PanelInput::Next } else { PanelInput::Previous };
                panel.input(doc, input).map_err(|e| e.to_string())?;
            }
'@
$cases = @(
    @{Name='terminal-repeat'; File='crates/ui-tui/src/settings_ui.rs'; Old='event.kind == KeyEventKind::Repeat && (host.panel_is_capturing() || host.panel_is_confirming())'; New='false'; Package='kwil-ui-tui'; Test='terminal_compound_capture_keeps_modified_escape_and_cancels_in_stages'},
    @{Name='terminal-detail-page'; File='crates/ui-tui/src/settings_ui.rs'; Old='(view.page + 1).min(view.pages - 1)'; New='view.page'; Package='kwil-ui-tui'; Test='terminal_compound_capture_keeps_modified_escape_and_cancels_in_stages'},
    @{Name='review-page-content'; File='crates/view-model/src/settings_panels.rs'; Old='lines.into_iter().skip(page * rows).take(rows).collect()'; New='lines.into_iter().take(rows).collect()'; Package='kwil-view-model'; Test='settings_confirmation_pages_preserve_long_fields_in_compact_cards'},
    @{Name='review-wrap'; File='crates/view-model/src/settings_panels.rs'; Old='columns + cell.width > width && !line.is_empty()'; New='false'; Package='kwil-view-model'; Test='settings_confirmation_pages_preserve_long_fields_in_compact_cards'},
    @{Name='terminal-zero-conflicts'; File='crates/view-model/src/settings_panels.rs'; Old='let proposal = self.proposal.as_ref()?;'; New="if self.confirmation.is_empty() { return None; }`n        let proposal = self.proposal.as_ref()?;"; Package='kwil-ui-tui'; Test='terminal_compound_capture_keeps_modified_escape_and_cancels_in_stages'},
    @{Name='panel-capacity'; File='crates/view-model/src/settings_panels.rs'; Old="Ok(()) => {`n                self.rows(&candidate)?;"; New="Ok(()) => {"; Package='kwil-view-model'; Test='settings_panel_capacity_rejects_add_before_save_and_never_traps_cancel'},
    @{Name='cancel-over-capacity'; File='crates/view-model/src/settings_panels.rs'; Old="pub fn input(&mut self, doc: &SettingsDocument, input: PanelInput<'_>) -> Result<Option<PanelEdit>, SettingsError> {"; New="pub fn input(&mut self, doc: &SettingsDocument, input: PanelInput<'_>) -> Result<Option<PanelEdit>, SettingsError> {`n        let _ = self.rows(doc)?;"; Package='kwil-view-model'; Test='settings_panel_capacity_rejects_add_before_save_and_never_traps_cancel'},
    @{Name='clear-query-over-capacity'; File='crates/view-model/src/settings_panels.rs'; Old='if !query.is_empty() {'; New='if true {'; Package='kwil-view-model'; Test='settings_panel_capacity_rejects_add_before_save_and_never_traps_cancel'},
    @{Name='absolute-selection'; File='crates/view-model/src/settings_session.rs'; Old='panel.select(doc, index).map_err(|e| e.to_string())?;'; New=$oldSelection; Package='kwil-view-model'; Test='settings_panel_absolute_selection_builds_rows_once'},
    @{Name='conflict-navigation'; File='crates/view-model/src/settings_panels.rs'; Old='(self.confirmation_index + 1).min(approved.len().saturating_sub(1))'; New='self.confirmation_index'; Package='kwil-view-model'; Test='settings_confirmation_exposes_proposal_and_navigates_each_conflict'},
    @{Name='conflict-proposal'; File='crates/view-model/src/settings_panels.rs'; Old='"{} <- {} [{:?}/{:?}]"'; New='"hidden {} {} [{:?}/{:?}]"'; Package='kwil-view-model'; Test='settings_confirmation_exposes_proposal_and_navigates_each_conflict'},
    @{Name='terminal-conflicts'; File='crates/view-model/src/settings_panels.rs'; Old='format!("Command: {}", conflict.command)'; New='String::from("Command: hidden")'; Package='kwil-ui-tui'; Test='terminal_compound_capture_keeps_modified_escape_and_cancels_in_stages'},
    @{Name='terminal-proposal'; File='crates/view-model/src/settings_panels.rs'; Old='vec![format!("Proposed: {proposal}")]'; New='vec![String::from("Proposed: hidden")]'; Package='kwil-ui-tui'; Test='terminal_compound_capture_keeps_modified_escape_and_cancels_in_stages'}
)
if ($Only.Count) { $cases = @($cases | Where-Object { $_.Name -in $Only }) }
if (-not $cases.Count) { throw 'No selected mutation cases' }
"Commit $head with pending review correction; source fingerprints follow" | Set-Content (Join-Path $proof 'summary.log')
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
        if ($job.ExitCode -ne 101 -or $text -match 'error\[E\d+\]|could not compile' -or $text -notmatch 'running [1-9][0-9]* test' -or $text -notmatch 'assertion|must |confirmation|held Enter') {
            throw "Expected compiling targeted assertion failure: $($case.Name), exit=$($job.ExitCode)"
        }
        "PASS compiling assertion failure: $($case.Name)" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
    } finally {
        [IO.File]::WriteAllBytes($path, $bytes)
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $hash) { throw 'Source restoration mismatch' }
    }
}
Write-Output "Evidence $proof"
