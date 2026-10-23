# Per-user, versioned Windows installation. Never edits persistent PATH or associations.
[CmdletBinding()]
param([string]$Version = 'latest')

$ErrorActionPreference = 'Stop'

function Get-KwilFile([string]$Url, [string]$Destination, [long]$Limit) {
    Add-Type -AssemblyName System.Net.Http
    $handler = [Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd('Kwil-Installer/1')
    $cancel = [Threading.CancellationTokenSource]::new(60000)
    $response = $null; $source = $null; $output = $null
    try {
        $uri = [uri]$Url
        for ($redirect = 0; ; $redirect++) {
            if ($uri.Scheme -ne 'https' -or $redirect -gt 10) { throw 'Unsafe or excessive download redirects' }
            $response = $client.GetAsync($uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancel.Token).GetAwaiter().GetResult()
            if ([int]$response.StatusCode -notin @(301,302,303,307,308)) { break }
            $next = $response.Headers.Location
            if (!$next) { throw 'Missing download redirect location' }
            $uri = [uri]::new($uri, $next)
            $response.Dispose(); $response = $null
        }
        $response.EnsureSuccessStatusCode() | Out-Null
        if ($response.Content.Headers.ContentLength -gt $Limit) { throw 'Download exceeds installer limit' }
        $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $output = [IO.File]::Open($Destination, [IO.FileMode]::CreateNew)
        $buffer = [byte[]]::new(65536); [long]$total = 0
        while (($count = $source.ReadAsync($buffer, 0, $buffer.Length, $cancel.Token).GetAwaiter().GetResult()) -gt 0) {
            $total += $count
            if ($total -gt $Limit) { throw 'Download exceeds installer limit' }
            $output.Write($buffer, 0, $count)
        }
        $output.Flush($true)
    } finally {
        if ($output) { $output.Dispose() }; if ($source) { $source.Dispose() }
        if ($response) { $response.Dispose() }; $cancel.Dispose(); $client.Dispose()
    }
}

function Get-KwilChecksum([string]$Manifest, [string]$Name) {
    $matches = @($Manifest -split "`n" | Where-Object { $_.TrimEnd("`r") -cmatch ('^[0-9a-fA-F]{64} [ *]' + [regex]::Escape($Name) + '$') })
    if ($matches.Count -ne 1) { throw "Missing or duplicate checksum: $Name" }
    return $matches[0].Substring(0, 64)
}

function Install-Kwil([string]$RequestedVersion) {
    if ($env:OS -ne 'Windows_NT') { throw 'Use install.sh on Linux or macOS' }
    # PROCESSOR_ARCHITEW6432 describes the native OS when PowerShell is emulated.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (!$arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    $target = switch ($arch) {
        'AMD64' { 'x86_64-pc-windows-msvc' }
        'ARM64' { 'aarch64-pc-windows-msvc' }
        default { throw "Unsupported Windows architecture: $arch" }
    }
    if ($RequestedVersion -ne 'latest' -and $RequestedVersion -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Expected latest or vX.Y.Z' }
    if (!$env:LOCALAPPDATA -or ![IO.Path]::IsPathRooted($env:LOCALAPPDATA)) { throw 'LOCALAPPDATA must be an absolute path' }
    $root = Join-Path $env:LOCALAPPDATA 'Programs/Kwil'
    # Refuse redirected installation ancestors rather than writing through a junction.
    $ancestor = $root
    while ($ancestor) {
        if (Test-Path -LiteralPath $ancestor) {
            $item = Get-Item -LiteralPath $ancestor -Force
            if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Unsafe installation directory: $ancestor" }
        }
        $ancestor = Split-Path -Parent $ancestor
    }
    [void][IO.Directory]::CreateDirectory($root)
    $stage = Join-Path $root ('.install-' + [guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    try {
        $endpoint = 'https://api.github.com/repos/rennerdo30/kwil/releases/'
        if ($RequestedVersion -eq 'latest') { $endpoint += 'latest' } else { $endpoint += 'tags/' + $RequestedVersion }
        $metadata = Join-Path $stage 'release.json'
        Get-KwilFile $endpoint $metadata 1048576
        $release = Get-Content -Raw -LiteralPath $metadata | ConvertFrom-Json
        if ($release.draft -or $release.prerelease -or $release.tag_name -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Expected a published stable release' }
        $tag = [string]$release.tag_name
        if ($RequestedVersion -ne 'latest' -and $tag -cne $RequestedVersion) { throw 'Release version mismatch' }
        $destination = Join-Path $root $tag
        if (Test-Path -LiteralPath $destination) { throw "Version already exists; retained unchanged: $destination" }
        $base = "https://github.com/rennerdo30/kwil/releases/download/$tag"
        $sums = Join-Path $stage 'SHA256SUMS'
        Get-KwilFile "$base/SHA256SUMS" $sums 1048576
        $manifest = Get-Content -Raw -LiteralPath $sums
        foreach ($ext in @('exe', 'com')) {
            $name = "kwil-$target.$ext"
            $expected = Get-KwilChecksum $manifest $name
            $path = Join-Path $stage "kwil.$ext"
            Get-KwilFile "$base/$name" $path 268435456
            if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ine $expected) { throw "Checksum mismatch: $name" }
        }
        Remove-Item -LiteralPath $metadata, $sums
        # Publish only the complete verified pair, into an absent version directory.
        [IO.Directory]::Move($stage, $destination)
        $env:PATH = $destination + [IO.Path]::PathSeparator + $env:PATH
        Write-Host "Installed Kwil $tag. Run kwil in this PowerShell session."
        Write-Host "For future sessions: & '$destination/kwil.exe'"
        Write-Host 'No persistent PATH or file associations were changed.'
    } finally {
        if (Test-Path -LiteralPath $stage) {
            # Only installer-owned known files; never recursively remove a path.
            foreach ($name in @('release.json', 'SHA256SUMS', 'kwil.exe', 'kwil.com')) {
                $path = Join-Path $stage $name
                if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
            }
            [IO.Directory]::Delete($stage)
        }
    }
}

# Dot-sourcing loads the functions for offline tests without installing anything.
if ($MyInvocation.InvocationName -ne '.') { Install-Kwil $Version }
