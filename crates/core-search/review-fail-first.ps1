# Review regressions must fail assertions on deliberately broken implementations.
# Only this worktree's owned source is mutated, then restored byte-for-byte.
$ErrorActionPreference = 'Stop'
$cases = @(
    @{ Test='whole_word_alternatives_captures_and_literal_boundaries'; File='search.rs'; From='RegexBuilder::new(&constrained)'; To='RegexBuilder::new(expression)' },
    @{ Test='piece_snapshot_chunks_search_and_replace_stay_immutable'; File='search.rs'; From='at += bytes.len() as u64;'; To='at += 0;' },
    @{ Test='haystack_read_errors_reach_search_filter_and_replace'; File='search.rs'; From='copy_range(source, lo..hi, &mut bytes)?;'; To='if copy_range(source, lo..hi, &mut bytes).is_err() { return Ok(()); }' },
    @{ Test='changed_file_adapter_reports_errors_for_file_and_snapshot'; File='search.rs'; From='let bytes = self.bytes().map_err(io::Error::other)?;'; To='let bytes: &[u8] = b"xxxxxxxxxxxxxxxxx";' },
    @{ Test='files_utf16_boms_keep_original_offsets_and_skip_binary'; File='files.rs'; From='offsets.at(m.start(), false)..offsets.at(m.end(), true)'; To='m.start()..m.end()' },
    @{ Test='files_crlf_ranges_include_matching_terminator'; File='files.rs'; From='.find_iter(bytes, |m| {'; To='.find_iter(preview, |m| {' }
)
foreach ($case in $cases) {
    $path = Join-Path $PSScriptRoot "src/$($case.File)"
    $original = [IO.File]::ReadAllBytes($path)
    $source = [Text.Encoding]::UTF8.GetString($original)
    if (-not $source.Contains($case.From)) { throw "Mutation point missing for $($case.Test)" }
    try {
        [IO.File]::WriteAllText($path, $source.Replace($case.From, $case.To), [Text.UTF8Encoding]::new($false))
        $verify = Join-Path $PSScriptRoot 'verify.ps1'
        $command = "& '$verify' -CargoArgs @('test','-p','kwil-core-search','-j','4','--locked','$($case.Test)','--','--exact','tests::$($case.Test)') -TimeoutSeconds 60"
        $output = & pwsh -NoProfile -Command $command 2>&1
        $code = $LASTEXITCODE
        $output | Write-Output
        if ($code -eq 0 -or -not (($output -join "`n").Contains('test result: FAILED'))) {
            throw "Mutation survived or failed without a test assertion: $($case.Test) (exit $code)"
        }
        Write-Output "FAIL-FIRST CONFIRMED: $($case.Test) (exit $code)"
    } finally {
        [IO.File]::WriteAllBytes($path, $original)
    }
}
