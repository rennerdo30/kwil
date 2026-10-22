param()
$ErrorActionPreference='Stop'
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-28-diff'
$path=Join-Path $PSScriptRoot '../src/lib.rs'
$original=[IO.File]::ReadAllText($path)
$mutants=@(
    @('insert_delete_modify_alignment','model.push(a.start, b.start, paired, Kind::Modify);','model.push(a.start, b.start, paired, Kind::Equal);'),
    @('whitespace_modes_and_line_endings_are_independent','let eol = if bytes.ends_with','let options = &Options::default(); let eol = if bytes.ends_with'),
    @('word_ranges_address_original_bytes','ranges.left.push(l..l + len)','ranges.left.push(l + 1..l + len)'),
    @('refinement_has_line_and_hunk_bounds','> MAX_WORD_LINE_BYTES','> usize::MAX'),
    @('collapse_and_navigation_use_alignment_rows','let mut out = Vec::new();','let mut out = vec![DisplaySegment::Rows(0..self.row_count)];'),
    @('size_guards_precede_reads_and_bound_bytes','line.len() > MAX_LINE_BYTES || bytes > MAX_SCANNED_BYTES','bytes > MAX_SCANNED_BYTES'),
    @('cancellation_before_and_during_source_reads','self.0.store(true, Ordering::Relaxed)','self.0.store(false, Ordering::Relaxed)')
)
Write-Output ('commit under test: ' + (git rev-parse HEAD) + ' plus working core-diff changes')
try {
    foreach($m in $mutants) {
        if(-not $original.Contains($m[1])) { throw "Missing mutation anchor: $($m[0])" }
        [IO.File]::WriteAllText($path,$original.Replace($m[1],$m[2]))
        $prefix=Join-Path $env:CARGO_TARGET_DIR ('mutant-' + [guid]::NewGuid().ToString('N'))
        $run=Start-Process -FilePath 'cargo' -ArgumentList @('test','-p','kwil-core-diff','-j','4','--locked','--test','engine',$m[0],'--','--exact') -WindowStyle Hidden -PassThru -RedirectStandardOutput "$prefix.out" -RedirectStandardError "$prefix.err"
        Write-Output "Started mutation PID $($run.Id), timeout 60s"
        if(-not $run.WaitForExit(60000)) { Stop-Process -Id $run.Id; throw "Mutation timeout: $($m[0])" }
        $exit=$run.ExitCode
        $log=@(Get-Content "$prefix.out") + @(Get-Content "$prefix.err")
        if($exit -eq 0 -or -not ($log -match 'test result: FAILED')) { $log; throw "Mutation not detected: $($m[0])" }
        Write-Output "DETECTED: $($m[0])"
    }
} finally { [IO.File]::WriteAllText($path,$original) }
