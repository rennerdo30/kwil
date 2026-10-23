# Every retained contract must reject identity formatting with disabled admission.
# Requires direct ROOT Cargo grant and exclusive source-reader hold.
. (Join-Path $PSScriptRoot 'verify-support.ps1')
Invoke-FormatterWitness -Name 'contracts' -TestTarget 'contracts' -Passed 0 -Failed 20 -Paths @('lib.rs') -Prepare {
    $path = Join-Path $PSScriptRoot 'src/lib.rs'
    $source = [IO.File]::ReadAllText($path)
    $start = $source.IndexOf('pub fn format(')
    $end = $source.IndexOf('/// Admission probe', $start)
    if ($start -lt 0 -or $end -lt $start) { throw 'Format mutation anchor missing' }
    $stub = 'pub fn format(input: &[u8], _language: Language, _file_type: &str, _selection: Option<Range<usize>>, _settings: &Settings) -> Result<FormatResult, FormatError> { Ok(FormatResult { bytes: input.to_vec(), replacement: None, notices: Vec::new() }) }' + "`n`n"
    $source = $source.Substring(0, $start) + $stub + $source.Substring($end)
    $start = $source.IndexOf('pub fn check_size(')
    $end = $source.IndexOf('/// Format an already-admitted', $start)
    if ($start -lt 0 -or $end -lt $start) { throw 'Admission mutation anchor missing' }
    $source = $source.Substring(0, $start) + "pub fn check_size(_size: usize, _settings: &Settings) -> Result<(), FormatError> { Ok(()) }`n`n" + $source.Substring($end)
    [IO.File]::WriteAllText($path, $source)
}
