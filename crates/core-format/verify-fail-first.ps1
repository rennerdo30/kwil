# Reproducible mutation proof: every contract must reject an identity formatter.
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-7-format'
$sourcePath = Join-Path $PSScriptRoot 'src/lib.rs'
$source = [IO.File]::ReadAllText($sourcePath)
$formatStart = $source.IndexOf('pub fn format(')
$formatEnd = $source.IndexOf('/// Admission probe', $formatStart)
$stub = @'
pub fn format(input: &[u8], _language: Language, _file_type: &str, _selection: Option<Range<usize>>, _settings: &Settings) -> Result<FormatResult, FormatError> {
    Ok(FormatResult { bytes: input.to_vec(), replacement: None, notices: Vec::new() })
}

'@
$mutated = $source.Substring(0, $formatStart) + $stub + $source.Substring($formatEnd)
$guardStart = $mutated.IndexOf('pub fn check_size(')
$guardEnd = $mutated.IndexOf('/// Format an already-admitted', $guardStart)
$mutated = $mutated.Substring(0, $guardStart) + "pub fn check_size(_size: usize, _settings: &Settings) -> Result<(), FormatError> { Ok(()) }`n`n" + $mutated.Substring($guardEnd)
$outLog = Join-Path $PSScriptRoot 'mutation.stdout.log'
$errLog = Join-Path $PSScriptRoot 'mutation.stderr.log'
try {
    [IO.File]::WriteAllText($sourcePath, $mutated)
    Write-Output ('commit_under_test=' + (git rev-parse HEAD))
    $cargoProcess = Start-Process cargo -ArgumentList @('test', '-p', 'kwil-core-format', '--test', 'contracts', '-j', '4') -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    Write-Output ('started_cargo_pid=' + $cargoProcess.Id)
    if (-not $cargoProcess.WaitForExit(60000)) { $cargoProcess.Kill(); throw 'mutation test timed out' }
    Get-Content $outLog
    Get-Content $errLog
    if ($cargoProcess.ExitCode -eq 0) { throw 'Broken formatter unexpectedly passed' }
    $output = [IO.File]::ReadAllText($outLog)
    if ($output -notmatch '0 passed; 20 failed') { throw 'Every one of the 20 contract tests must fail' }
} finally {
    [IO.File]::WriteAllText($sourcePath, $source)
}
