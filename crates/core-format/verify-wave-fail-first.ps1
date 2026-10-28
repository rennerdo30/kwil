# Run exactly one independent fault per direct parent Cargo grant selection.
param([Parameter(Mandatory)][ValidateSet('sql', 'separator', 'xml', 'java', 'crlf', 'physical-eol')][string]$Case)
. (Join-Path $PSScriptRoot 'verify-support.ps1')
$fault = switch ($Case) {
    'sql' { @{
        Path = 'languages.rs'; Target = 'round6_regressions'; Filter = 'sql_backslash_quote_ambiguity'
        From = 'if kind == "SQL" {'; To = 'if false && kind == "SQL" {'
    } }
    'separator' { @{
        Path = 'literals.rs'; Target = 'round6_regressions'; Filter = 'generic_selection_does_not_trim'
        From = '(false, partial_final_line, partial_crlf)'; To = '(false, false, partial_crlf)'
    } }
    'xml' { @{
        Path = 'lib.rs'; Target = 'round6_regressions'; Filter = 'xml_selection_does_not_lose'
        From = 'Language::Xml if selected && range.start != 0'; To = 'Language::Xml if false && selected && range.start != 0'
        From2 = 'language == Language::Xml && (start_byte != 0 || start_line > 1)'
        To2 = 'false && language == Language::Xml && (start_byte != 0 || start_line > 1)'
    } }
    'java' { @{
        Path = 'literals.rs'; Target = 'round6_regressions'; Filter = 'java_unicode_preprocessing'
        From = 'if self.pos == 0 && self.kind == Kind::Java'; To = 'if false && self.pos == 0 && self.kind == Kind::Java'
    } }
    'crlf' { @{
        Path = 'literals.rs'; Target = 'selection_boundaries'; Filter = 'selected_crlf_prefix'
        From = '(false, partial_final_line, partial_crlf)'; To = '(false, partial_final_line, false)'
    } }
    'physical-eol' { @{
        Path = 'literals.rs'; Target = 'selection_boundaries'; Filter = 'selection_uses_actual_following_bytes'
        From = "b.get(end).is_some_and(|&c| c != b'\n' && (kind == Kind::Shell || c != b'\r'))"
        To = 'end < text.len()'
    } }
}
Invoke-FormatterWitness -Name "wave-$Case" -TestTarget $fault.Target -Filter $fault.Filter -Passed 0 -Failed 1 -Paths @($fault.Path) -Prepare {
    $path = Join-Path $PSScriptRoot "src/$($fault.Path)"
    $source = [IO.File]::ReadAllText($path)
    if (-not $source.Contains($fault.From)) { throw "Missing $Case mutation anchor" }
    $source = $source.Replace($fault.From, $fault.To)
    if ($fault.From2) {
        if (-not $source.Contains($fault.From2)) { throw 'Missing second XML entrypoint anchor' }
        $source = $source.Replace($fault.From2, $fault.To2)
    }
    [IO.File]::WriteAllText($path, $source)
}
