param([string[]]$Only = @())
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$proof = Join-Path 'E:/Development/_cache' ('kwil-recovery-marker-proof-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $proof | Out-Null
$sha = (git -C $root rev-parse HEAD).Trim()
$cases = @(
    @{Name='reserved-marker';File='crates/view-model/src/document.rs';Old='self.next_revision = next;';New='let _ = next;';Package='kwil-view-model';Test='workspace::tests::cached_reopen_without_original_keeps_bytes_and_requires_save_as'},
    @{Name='restored-marker';File='crates/view-model/src/document_worker.rs';Old='.max(document.saved_revision)';New='';Package='kwil-view-model';Test='document_worker::tests::restored_unsaved_marker_stays_outside_future_edit_and_undo_revisions'}
)
$executed = 0
foreach($case in $cases) {
    if($Only.Count -and $case.Name -notin $Only) {continue}
    $executed += 1
    $file = Join-Path $root $case.File
    $original = [IO.File]::ReadAllBytes($file)
    $source = [IO.File]::ReadAllText($file)
    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
    if(([regex]::Matches($source,[regex]::Escape($case.Old))).Count -ne 1) {throw "Mutation anchor missing/ambiguous: $($case.Name)"}
    try {
        [IO.File]::WriteAllText($file,$source.Replace($case.Old,$case.New))
        [IO.File]::SetLastWriteTimeUtc($file,[DateTime]::UtcNow)
        $arguments = @('test','--offline','--locked','-j4','-p',$case.Package,'--lib',$case.Test,'--','--exact','--test-threads=1')
        $stdout=Join-Path $proof ($case.Name+'.stdout')
        $stderr=Join-Path $proof ($case.Name+'.stderr')
        $proc=Start-Process -FilePath (Get-Command cargo).Source -WorkingDirectory $root -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        "base $sha case $($case.Name) source $hash PID $($proc.Id)" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
        if(-not $proc.WaitForExit(180000)) {$proc.Kill();$proc.WaitForExit();throw "Owned mutant $($case.Name) timed out"}
        $out=[IO.File]::ReadAllText($stdout)
        if($proc.ExitCode -ne 101 -or $out -notmatch 'running 1 test' -or $out -notmatch ([regex]::Escape('test '+$case.Test+' ... FAILED')) -or $out -notmatch 'assertion') {throw "Expected compiling assertion failure missing: $($case.Name), exit $($proc.ExitCode), logs $proof"}
        "PASS witness $($case.Name), exit101 assertion failed" | Tee-Object -FilePath (Join-Path $proof 'summary.log') -Append
    } finally {
        [IO.File]::WriteAllBytes($file,$original)
        [IO.File]::SetLastWriteTimeUtc($file,[DateTime]::UtcNow)
        if((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $hash) {throw "Source restore hash failed: $file"}
    }
}
if($executed -eq 0) {throw 'No mutation cases selected; pass -Only as a PowerShell string array'}
Write-Output "Proof $proof"
