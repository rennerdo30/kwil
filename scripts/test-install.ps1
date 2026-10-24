param([Parameter(Mandatory=$true)][string]$FixtureRoot)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/install.ps1"
function Assert($condition, $message) { if (!$condition) { throw $message } }
function Must-Fail([scriptblock]$action, [string]$message) {
    try { & $action } catch { if ($_.Exception.Message -like "*$message*") { return }; throw }
    throw "Expected refusal: $message"
}
$fixture = [IO.Path]::GetFullPath($FixtureRoot)
if (Test-Path -LiteralPath $fixture) { throw 'Fixture root must be new' }
[void][IO.Directory]::CreateDirectory($fixture)
$oldLocal = $env:LOCALAPPDATA; $oldPath = $env:PATH
$oldArch = $env:PROCESSOR_ARCHITECTURE; $oldWow = $env:PROCESSOR_ARCHITEW6432
$script:badPayload = $false
$script:payload = [Text.Encoding]::UTF8.GetBytes('non-executable installer fixture')
$hash = [Security.Cryptography.SHA256]::Create()
try { $script:digest = -join ($hash.ComputeHash($script:payload) | ForEach-Object { $_.ToString('x2') }) } finally { $hash.Dispose() }
# No networking, registry, app launch or real profile access in these fixtures.
function Get-KwilFile([string]$Url, [string]$Destination, [long]$Limit) {
    if ($Url -like '*/latest') {
        [IO.File]::WriteAllText($Destination, '{"draft":false,"prerelease":false,"tag_name":"v1.2.3"}')
    } elseif ($Url -like '*/SHA256SUMS') {
        [IO.File]::WriteAllText($Destination, "$script:digest  kwil-x86_64-pc-windows-msvc.exe`n$script:digest  kwil-x86_64-pc-windows-msvc.com`n")
    } else {
        $bytes = $script:payload
        if ($script:badPayload -and $Url.EndsWith('.com')) { $bytes = [byte[]]@(0,1,2) }
        [IO.File]::WriteAllBytes($Destination, $bytes)
    }
}
try {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'; $env:PROCESSOR_ARCHITEW6432 = ''
    $env:LOCALAPPDATA = Join-Path $fixture 'success'
    Install-Kwil 'latest'
    $installed = Join-Path $env:LOCALAPPDATA 'Programs/Kwil/v1.2.3'
    Assert (Test-Path -LiteralPath "$installed/kwil.exe") 'GUI missing'
    Assert (Test-Path -LiteralPath "$installed/kwil.com") 'Console missing'
    Assert ((Get-FileHash -LiteralPath "$installed/kwil.exe").Hash -ieq $script:digest) 'Exact bytes changed'
    Must-Fail { Install-Kwil 'latest' } 'Version already exists'
    Assert ((Get-FileHash -LiteralPath "$installed/kwil.com").Hash -ieq $script:digest) 'Existing console changed'
    $env:LOCALAPPDATA = Join-Path $fixture 'corrupt'
    $script:badPayload = $true
    Must-Fail { Install-Kwil 'latest' } 'Checksum mismatch'
    Assert (!(Test-Path -LiteralPath "$env:LOCALAPPDATA/Programs/Kwil/v1.2.3")) 'Partial pair published'
    Assert (@(Get-ChildItem -LiteralPath "$env:LOCALAPPDATA/Programs/Kwil" -Force).Count -eq 0) 'Failed stage retained'
    Must-Fail { Get-KwilChecksum "$script:digest  a`n$script:digest  a" 'a' } 'duplicate checksum'
    Must-Fail { Get-KwilChecksum "$script:digest  other" 'a' } 'Missing'
    Must-Fail { Install-Kwil '../escape' } 'Expected latest'
    Write-Output 'PASS: verified complete pair, exact preservation, corrupt-pair refusal, duplicate/missing checksum and version refusal'
} finally {
    $env:LOCALAPPDATA = $oldLocal; $env:PATH = $oldPath
    $env:PROCESSOR_ARCHITECTURE = $oldArch; $env:PROCESSOR_ARCHITEW6432 = $oldWow
}
