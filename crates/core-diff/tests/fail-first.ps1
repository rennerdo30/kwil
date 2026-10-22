param()
$ErrorActionPreference = 'Stop'
$env:CARGO_TARGET_DIR = 'E:/Development/_cache/cargo-target/kwil-kw-28-diff'
$crate = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
# Unique standalone copy: product source is never a mutation target or restored.
$copy = Join-Path 'E:/Development/_cache' ('kwil-diff-fail-first-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $copy | Out-Null
Copy-Item -LiteralPath (Join-Path $crate 'src'),(Join-Path $crate 'tests') -Destination $copy -Recurse
$manifest = [IO.File]::ReadAllText((Join-Path $crate 'Cargo.toml'))
$manifest = $manifest.Replace('version.workspace = true','version = "0.1.0"').Replace('edition.workspace = true','edition = "2024"').Replace('license.workspace = true','license = "MIT"')
# Give each copy its own Cargo package identity in the shared build cache.
[IO.File]::WriteAllText((Join-Path $copy 'Cargo.toml'), ($manifest.Replace('name = "kwil-core-diff"', 'name = "kwil-core-diff-' + [guid]::NewGuid().ToString('N') + '"') + "`n[lib]`nname = ""kwil_core_diff""`n[workspace]`n"))
Copy-Item -LiteralPath (Join-Path $crate '../../Cargo.lock') -Destination $copy
$path = Join-Path $copy 'src/lib.rs'
$original = [IO.File]::ReadAllText($path)
$mutants = @(
    @('whitespace_preserves_content_terminator_boundary','Normalized { content, ending, whitespace: options.whitespace }','{ let mut content = content.into_owned(); content.extend_from_slice(ending); Normalized { content: Cow::Owned(content), ending: b"", whitespace: options.whitespace } }'),
    @('tests::normalized_equality_and_hash_agree','self.whitespace == other.whitespace && self.ending == other.ending && self.bytes().eq(other.bytes())','self.bytes().chain(self.ending.iter().copied()).eq(other.bytes().chain(other.ending.iter().copied()))'),
    @('fragmented_sources_match_contiguous_and_propagate_errors','Line::Scratch => Cow::Owned(scratch)','Line::Scratch => Cow::Borrowed(&b""[..])'),
    @('fragmented_sources_match_contiguous_and_propagate_errors','source.line(index, &mut scratch)?','source.line(index, &mut scratch).unwrap_or(Line::Borrowed(b""))'),
    @('insert_delete_modify_alignment','model.push(a.start, b.start, paired, Kind::Modify);','model.push(a.start, b.start, paired, Kind::Equal);'),
    @('whitespace_modes_and_line_endings_are_independent','let eol = if bytes.ends_with','let options = &Options::default(); let eol = if bytes.ends_with'),
    @('word_ranges_address_original_bytes','ranges.left.push(l..l + len)','ranges.left.push(l + 1..l + len)'),
    @('refinement_has_line_and_hunk_bounds','> MAX_WORD_LINE_BYTES','> usize::MAX'),
    @('collapse_and_navigation_use_alignment_rows','let mut out = Vec::new();','let mut out = vec![DisplaySegment::Rows(0..self.row_count)];'),
    @('size_guards_precede_reads_and_bound_bytes','line.len() > MAX_LINE_BYTES','line.len() > usize::MAX'),
    @('cancellation_before_and_during_source_reads','self.0.store(true, Ordering::Relaxed)','self.0.store(false, Ordering::Relaxed)')
)
Write-Output ('commit under test: ' + (git -C $crate rev-parse HEAD) + ' plus working core-diff changes')
Write-Output "Isolated mutation copy and logs retained at $copy"
$n = 0
foreach ($m in $mutants) {
    if (-not $original.Contains($m[1])) { throw "Missing mutation anchor: $($m[0])" }
    [IO.File]::WriteAllText($path, $original.Replace($m[1], $m[2]))
    $n++
    $prefix = Join-Path $copy "mutant-$n"
    $info = [Diagnostics.ProcessStartInfo]::new((Get-Command cargo).Source)
    $info.WorkingDirectory = $copy
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in @('test','--manifest-path',(Join-Path $copy 'Cargo.toml'),'-j','4','--offline',$m[0],'--','--exact')) { $info.ArgumentList.Add($arg) }
    $run = [Diagnostics.Process]::Start($info)
    Write-Output "Started mutation PID $($run.Id), timeout 60s"
    $stdout = $run.StandardOutput.ReadToEndAsync()
    $stderr = $run.StandardError.ReadToEndAsync()
    if (-not $run.WaitForExit(60000)) { $run.Kill($true); throw "Mutation timeout: $($m[0])" }
    $stdout.Result | Set-Content "$prefix.out"
    $stderr.Result | Set-Content "$prefix.err"
    $log = @(Get-Content "$prefix.out") + @(Get-Content "$prefix.err")
    if ($run.ExitCode -eq 0 -or -not ($log -match 'test result: FAILED') -or -not ($log -match 'assertion|unwrap')) {
        $log
        throw "Mutation not detected by assertion: $($m[0])"
    }
    Write-Output "DETECTED: $($m[0])"
}
# Leave only the isolated copy restored for inspection; no product writes occurred.
[IO.File]::WriteAllText($path, $original)
