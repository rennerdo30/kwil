# Read-only audit of cached crate tarballs and pinned upstream licence text.
# Network calls have a timeout; no credentials or persistent environment changes.
param(
    [string]$Output = 'E:/Development/_cache/kwil-syntax-license-audit.json',
    [string]$Registry = (Join-Path $env:USERPROFILE '.cargo/registry')
)
$ErrorActionPreference = 'Stop'
$registry = $Registry
$index = 'index.crates.io-1949cf8c6b5b557f'
$names = @(Select-String crates/core-syntax/Cargo.toml -Pattern '^(arborium-[a-z-]+) = ' |
    ForEach-Object { $_.Matches[0].Groups[1].Value }) + @('arborium-jsdoc', 'tree-sitter-rust')
$lock = [IO.File]::ReadAllText((Join-Path (Get-Location) 'Cargo.lock'))
$rows = @()
foreach ($name in $names) {
    $version = if ($name -eq 'tree-sitter-rust') { '0.24.2' } else { '2.18.2' }
    $source = "$registry/src/$index/$name-$version"
    $readme = [IO.File]::ReadAllText("$source/README.md")
    $manifest = [IO.File]::ReadAllText("$source/Cargo.toml.orig")
    $licence = [regex]::Match($manifest, '(?m)^license = "([^"]+)"').Groups[1].Value
    $repo = [regex]::Match($readme, '\*\*Repository\*\*.*?\((https://github.com/[^)]+)\)').Groups[1].Value
    $commit = [regex]::Match($readme, '\*\*Commit\*\*.*?`([^`]*)`').Groups[1].Value
    $note = 'README upstream pin; crate manifest; upstream licence text'
    if ($name -eq 'tree-sitter-rust') {
        $repo = 'https://github.com/tree-sitter/tree-sitter-rust'
        $commit = '77a3747266f4d621d0757825e6b11edcbf991ca5'
        $note = 'v0.24.2 release pin; packaged LICENSE, grammar, query, parser and scanner byte-match upstream'
        foreach ($file in @('LICENSE', 'grammar.js', 'queries/highlights.scm', 'src/parser.c', 'src/scanner.c')) {
            $url = $repo.Replace('https://github.com/', 'https://raw.githubusercontent.com/') + "/$commit/$file"
            $content = [string](Invoke-WebRequest $url -TimeoutSec 30).Content
            $actual = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($content)))
            if ((Get-FileHash "$source/$file" -Algorithm SHA256).Hash -ne $actual) { throw "Rust upstream mismatch: $file" }
        }
    }
    if ($name -eq 'arborium-ruby') {
        # This released crate has a blank upstream commit. Pin the licence audit,
        # while identifying the exact shipped grammar by its crate checksum.
        $commit = 'ad907a69da0c8a4f7a943a7fe012712208da6dee'
        $note = 'README upstream pin blank; licence audited at this commit, not a claimed parser pin'
    }
    $crateHash = (Get-FileHash "$registry/cache/$index/$name-$version.crate" -Algorithm SHA256).Hash.ToLowerInvariant()
    $package = [regex]::Match($lock, '(?ms)\[\[package\]\]\s+name = "' + [regex]::Escape($name) + '"\s+version = "' + [regex]::Escape($version) + '".*?(?=\[\[package\]\]|\z)').Value
    if (-not $package.Contains('checksum = "' + $crateHash + '"')) { throw "Lock checksum mismatch: $name" }
    $text = $null
    $url = $null
    foreach ($path in @('LICENSE', 'LICENSE.md', 'LICENSE.txt', 'LICENCE', 'COPYING', 'LICENSE-MIT')) {
        $candidate = $repo.Replace('https://github.com/', 'https://raw.githubusercontent.com/') + "/$commit/$path"
        try {
            $response = Invoke-WebRequest $candidate -TimeoutSec 20
            $text = [string]$response.Content
            $url = $candidate
            break
        } catch {
            if ([int]$_.Exception.Response.StatusCode -ne 404) { throw }
        }
    }
    if (-not $text) { throw "No upstream licence found for $name at $commit" }
    if ($licence -eq 'MIT' -and $text -notmatch 'Permission is hereby granted') { throw "Not MIT: $name" }
    if ($licence -eq 'Apache-2.0' -and $text -notmatch 'Apache License') { throw "Not Apache-2.0: $name" }
    $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($text))).ToLowerInvariant()
    if ($name -eq 'tree-sitter-rust' -and (Get-FileHash "$source/LICENSE" -Algorithm SHA256).Hash.ToLowerInvariant() -ne $hash) {
        throw 'Rust packaged and pinned upstream licences differ'
    }
    $rows += [pscustomobject]@{ name=$name; version=$version; repository=$repo; commit=$commit;
        licence=$licence; crate_sha256=$crateHash; licence_url=$url; licence_sha256=$hash; verification=$note; text=$text }
    Write-Output "$name $version $licence verified"
}
# Kotlin's query has a separate Apache-2.0 notice in the packaged source.
$queryLicenceUrl = 'https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/f8ab59861eed4a1c168505e3433462ed800f2bae/LICENSE'
$text = [string](Invoke-WebRequest $queryLicenceUrl -TimeoutSec 20).Content
if ($text -notmatch 'Apache License') { throw 'Kotlin query licence changed' }
$hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($text))).ToLowerInvariant()
$kotlin = $rows | Where-Object name -eq 'arborium-kotlin'
$rows += [pscustomobject]@{ name='kotlin-highlight-query'; version='in arborium-kotlin 2.18.2';
    repository='https://github.com/nvim-treesitter/nvim-treesitter'; commit='f8ab59861eed4a1c168505e3433462ed800f2bae';
    licence='Apache-2.0'; crate_sha256=$kotlin.crate_sha256; licence_url=$queryLicenceUrl; licence_sha256=$hash;
    verification='packaged query header names upstream source and Apache licence; upstream licence text'; text=$text }
$rows | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $Output -Encoding utf8NoBOM
Write-Output "Audited 33 grammars and Kotlin query notice: $Output"
