$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-10-encodings'
$sourcePath = Join-Path (Get-Location) 'crates/core-text/src/encoding.rs'
$originalSource = [IO.File]::ReadAllText($sourcePath)
$baseCommit = (git rev-parse HEAD).Trim()
$evidenceRoot = 'E:/AgentData/scratch/kwil-kw10/review-1'
New-Item -ItemType Directory -Path $evidenceRoot -Force | Out-Null
$cargoExe = (Get-Command cargo).Source
$cases = @(
    @{ name='corrupt-original-copy'; old='output.writer.write_all(bytes)?;'; new='output.writer.write_all(&bytes[..bytes.len().saturating_sub(1)])?;' },
    @{ name='hide-invalid-byte-markers'; old='emit(DisplaySegment::Invalid(&bytes[valid..valid + invalid]));'; new='emit(DisplaySegment::Invalid(&[]));' },
    @{ name='skip-multibyte-alignment'; old='let aligned = self.aligned(bytes, start..end)?;'; new='let aligned = start..end;' },
    @{ name='count-utf16-bytes'; old='let unit = |at: usize| if encoding.utf16() { encoding.unit(bytes, at) } else { bytes.get(at).map(|&b| b as u16) };'; new='let unit = |at: usize| bytes.get(at).map(|&b| b as u16);' },
    @{ name='drop-partial-added-utf8'; old='self.pending.extend_from_slice(&bytes[e.valid_up_to()..]);'; new='self.pending.clear();' },
    @{ name='skip-text-ending-normalization'; old='let Some(ending) = self.plan.line_ending else {'; new='let Some(ending) = None::<LineEnding> else {' },
    @{ name='silently-drop-unmappable'; old='return Err(EncodingError::Unmappable { character, encoding: self.plan.encoding.name() });'; new='return Ok(());' },
    @{ name='ignore-reinterpretation'; old='detect_bytes(self.file.bytes()?, Some(encoding))?'; new='detect_bytes(self.file.bytes()?, None)?' },
    @{ name='disable-range-end-guard'; old='range.start > range.end || range.end > self.file.len()'; new='range.start > range.end' },
    @{ name='remove-viewport-cap'; old=@('range.len_u64() > MAX_VIEWPORT_BYTES as u64', 'aligned.len() > MAX_VIEWPORT_BYTES'); new=@('false && range.len_u64() > MAX_VIEWPORT_BYTES as u64', 'false && aligned.len() > MAX_VIEWPORT_BYTES') },
    @{ name='invert-ending-edit-plan'; old='if self.target != current {'; new='if self.target == current {' },
    @{ name='allow-split-character-edit'; old='if aligned.start != at as usize {'; new='if false && aligned.start != at as usize {' },
    @{ name='decode-malformed-during-normalization'; old='} else if plan.encoding == self.detection.encoding {'; new='} else if false && plan.encoding == self.detection.encoding {' },
    @{ name='allow-stateful-iso2022-jp'; old='encoding == encoding_rs::ISO_2022_JP || encoding == encoding_rs::REPLACEMENT'; new='encoding == encoding_rs::REPLACEMENT' },
    @{ name='override-loses-to-utf32-sniff'; old='if chosen.is_none() && (bytes.starts_with(b"\xff\xfe\0\0") || bytes.starts_with(b"\0\0\xfe\xff")) {'; new='if bytes.starts_with(b"\xff\xfe\0\0") || bytes.starts_with(b"\0\0\xfe\xff") {' },
    @{ name='finalize-each-original-piece'; old='transcode(decoder, bytes, false, &mut output)?;'; new='transcode(decoder, bytes, true, &mut output)?; source_decoder = None;' }
)
$proved = [Collections.Generic.HashSet[string]]::new()
$summaries = [Collections.Generic.List[string]]::new()
try {
    foreach ($case in $cases) {
        $broken = $originalSource
        $oldValues = @($case.old)
        $newValues = @($case.new)
        for ($i = 0; $i -lt $oldValues.Count; $i++) {
            if (-not $broken.Contains($oldValues[$i])) { throw "Mutation pattern absent: $($case.name)" }
            $broken = $broken.Replace($oldValues[$i], $newValues[$i])
        }
        [IO.File]::WriteAllText($sourcePath, $broken)
        $stdoutPath = Join-Path $evidenceRoot "$($case.name).stdout"
        $stderrPath = Join-Path $evidenceRoot "$($case.name).stderr"
        $process = Start-Process -FilePath $cargoExe -ArgumentList @('test','-p','kwil-core-text','encoding::tests','-j','4','--locked','--','--test-threads=4') -WorkingDirectory (Get-Location) -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath -PassThru
        Write-Output "mutation=$($case.name) cargo_pid=$($process.Id)"
        if (-not $process.WaitForExit(120000)) { $process.Kill($true); throw "Owned cargo PID $($process.Id) exceeded 120 seconds" }
        $stdout = [IO.File]::ReadAllText($stdoutPath)
        $stderr = [IO.File]::ReadAllText($stderrPath)
        $failed = [regex]::Matches($stdout, '(?m)^test (encoding::tests::\w+) \.\.\. FAILED')
        $logPath = Join-Path $evidenceRoot "$($case.name).log"
        [IO.File]::WriteAllText($logPath, "commit=$baseCommit mutation=$($case.name) cargo_pid=$($process.Id)`n$stdout`n$stderr")
        if ($process.ExitCode -eq 0 -or $failed.Count -eq 0) { throw "Mutation did not produce a behavioral test failure: $($case.name), exit=$($process.ExitCode); see $logPath" }
        foreach ($test in $failed) { [void]$proved.Add($test.Groups[1].Value) }
        $summary = "$($case.name): $($failed.Count) test failures, exit=$($process.ExitCode)"
        $summaries.Add($summary)
        Write-Output $summary
    }
} finally {
    [IO.File]::WriteAllText($sourcePath, $originalSource)
}
$summaries.Add("Unique tests proven to fail: $($proved.Count)")
$summaries.AddRange([string[]]($proved | Sort-Object))
[IO.File]::WriteAllLines((Join-Path $evidenceRoot 'failfirst-summary.txt'), @("commit=$baseCommit") + $summaries)
Write-Output "Unique tests proven to fail: $($proved.Count)"
if ($proved.Count -ne 26) { throw 'Not every new test has fail-first evidence' }
