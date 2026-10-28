$ErrorActionPreference='Stop'
function Check-Mutant([string]$Path,[string]$Package,[string]$Test,[scriptblock]$Mutate) {
    $absolute=Join-Path $PSScriptRoot "../$Path"
    $original=[IO.File]::ReadAllText($absolute)
    try {
        $changed=& $Mutate $original
        if($changed -eq $original){throw "Mutation did not match $Path"}
        [IO.File]::WriteAllText($absolute,$changed)
        $output=& cargo test -p $Package --lib -j 4 $Test -- --test-threads=1 2>&1
        if($LASTEXITCODE -eq 0 -or ($output -join "`n") -notmatch 'test result: FAILED') { $output | Write-Output; throw "Mutant did not assertion-fail: $Test" }
        Write-Output "Assertion-failed as required: $Test"
    } finally { [IO.File]::WriteAllText($absolute,$original) }
}
Check-Mutant 'crates/view-model/src/grid.rs' 'kwil-view-model' 'horizontal_windows_have_per_row_utf8_boundaries' {
    param($s) $s.Replace('let position = position.min(bytes.len());','let position = position.min(bytes.len()); if position > 0 { return position; }')
}
Check-Mutant 'crates/ui-gpu/src/input.rs' 'kwil-ui-gpu' 'ime_commit_targets_the_save_prompt' {
    param($s) $s.Replace('if let Some(path) = path {','if path.is_some() { return Some(Action::Text(text)); } if let Some(path) = path {')
}
Check-Mutant 'crates/core-commands/src/keymap.rs' 'kwil-core-commands' 'exact_shift_shortcuts_win' {
    param($s) $s.Replace('let result = self.feed(key);','if key.modifiers.shift { let mut plain=key; plain.modifiers.shift=false; return self.feed(plain); } let result = self.feed(key);')
}
Check-Mutant 'crates/ui-tui/src/lib.rs' 'kwil-ui-tui' 'rendered_tabs_wide_characters_selection' {
    param($s) [regex]::Replace($s,'let column = cells\s*\.iter\(\).*?;', 'let column=editor.position_at(editor.primary_cursor().head).unwrap().1 as usize;', [System.Text.RegularExpressions.RegexOptions]::Singleline)
}
Check-Mutant 'crates/ui-tui/src/lib.rs' 'kwil-ui-tui' 'untitled_save_prompt_keeps' {
    param($s) $s.Replace('return KeyResult::Save(prompt.take().map(Into::into));','return KeyResult::Changed(false);')
}
