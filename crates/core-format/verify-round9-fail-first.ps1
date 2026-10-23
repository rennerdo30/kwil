# Source-only until direct parent Cargo grant and exclusive package-reader hold.
# The admitted runner must route Cargo through pinned rustup and child-only RUSTC.
param([ValidateSet('Base', 'Sql', 'Php')][string]$Case = 'Base')
. (Join-Path $PSScriptRoot 'verify-support.ps1')
$evidence = 'E:/AgentData/scratch/kwil-v1-wave-formatter-r9-9dcc8e0/harness'
$historical = '9dcc8e0e5194a7f995efecb209cb03e7ae34d813'
$fault = switch ($Case) {
    'Base' { @{
        Paths = @('languages.rs', 'literals.rs'); Filter = ''; Failed = 4; Base = $historical
    } }
    'Sql' { @{
        Paths = @('languages.rs'); Filter = 'sql_'; Failed = 2; Base = 'independent universal dollar/bracket SQL quotation fault'
    } }
    'Php' { @{
        Paths = @('literals.rs'); Filter = 'php_'; Failed = 2; Base = 'independent PHP outer-context admission bypass'
    } }
}
function Replace-Once([string]$Source, [string]$From, [string]$To) {
    if ($Source.Split([string[]]@($From), [StringSplitOptions]::None).Count -ne 2) { throw "Fault anchor missing/ambiguous: $From" }
    return $Source.Replace($From, $To)
}
$before = @{}
foreach ($name in $fault.Paths) {
    $path = Join-Path $PSScriptRoot "src/$name"
    $before[$path] = (Get-FileHash -LiteralPath $path).Hash
}
try {
    $witnessOutput = Invoke-FormatterWitness -Name "round9-$Case" -TestTarget 'round9_regressions' -Filter $fault.Filter -Passed 0 -Failed $fault.Failed -Paths $fault.Paths -Base $fault.Base -EvidenceDirectory $evidence -Prepare {
        if ($Case -eq 'Base') {
            Set-FormatterBase -Commit $historical -Paths $fault.Paths
        } else {
            $path = Join-Path $PSScriptRoot "src/$($fault.Paths[0])"
            $source = [IO.File]::ReadAllText($path)
            # Normalize only this temporary owned mutant. Finally restores the
            # candidate's exact original bytes, line endings, SHA and timestamps.
            $source = $source.Replace("`r`n", "`n")
            if ($Case -eq 'Sql') {
                $source = Replace-Once $source "b'?' | b'`$' | b'['" "b'?'"
                $source = Replace-Once $source "if text[start..i].contains('`$') {" "if false && text[start..i].contains('`$') {"
                $old = @'
        let mut quoted = matches!(b[i], b'\'' | b'"' | b'`');
'@
                $new = @'
        let bracket = sql && b[i] == b'[';
        let dollar = if sql && b[i] == b'$' {
            text[i + 1..].find('$').map(|end| &text[i..i + end + 2])
        } else { None };
        let mut quoted = matches!(b[i], b'\'' | b'"' | b'`') || bracket || dollar.is_some();
'@
                $source = Replace-Once $source $old $new
                $old = @'
        } else if quoted {
            i = quoted_end(text, i, b[i], sql, kind)?;
'@
                $new = @'
        } else if let Some(delimiter) = dollar {
            let after = i + delimiter.len();
            i = text[after..].find(delimiter).map(|n| after + n + delimiter.len())
                .ok_or_else(|| FormatError::new(kind, "fault: unterminated universal quote", line_at(b, start)))?;
        } else if quoted {
            i = quoted_end(text, i, if bracket { b']' } else { b[i] }, sql, kind)?;
'@
                $source = Replace-Once $source $old $new
            } else {
                $source = Replace-Once $source 'if self.kind == Kind::Php && !nowdoc {' 'if self.kind == Kind::Php && !nowdoc && false {'
            }
            [IO.File]::WriteAllText($path, $source)
        }
    }
    $witnessOutput | Write-Output
    $combined = $witnessOutput -join "`n"
    if ($combined -notmatch 'Compiling kwil-core-format') { throw 'R9 fault must freshly compile the formatter' }
    if ($Case -in @('Base', 'Sql') -and [regex]::Matches($combined, 'literal bytes changed:').Count -ne 2) {
        throw 'R9 SQL witness must fail both significant literal-byte assertions'
    }
    if ($Case -in @('Base', 'Php') -and [regex]::Matches($combined, 'significant raw body whitespace changed').Count -ne 2) {
        throw 'R9 PHP witness must fail both significant raw-body byte assertions'
    }
} finally {
    foreach ($path in $before.Keys) {
        $sha = (Get-FileHash -LiteralPath $path).Hash
        if ($sha -ne $before[$path]) { throw "R9 source restoration failed: $path" }
        $restoredTime = [IO.File]::GetLastWriteTimeUtc($path)
        [IO.File]::SetLastWriteTimeUtc($path, [DateTime]::UtcNow)
        if (Test-Path $evidence) {
            Add-Content (Join-Path $evidence 'restored-freshness.log') "Case=$Case SHA256=$sha exact-restored-mtime=$($restoredTime.ToString('o')) next-compile-mtime=$([IO.File]::GetLastWriteTimeUtc($path).ToString('o')) path=$path"
        }
    }
}
