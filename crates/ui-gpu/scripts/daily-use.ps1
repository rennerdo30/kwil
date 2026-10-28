param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [ValidateSet('resize','daily','handback')][string]$Probe='resize',
    [ValidateSet('cpu','auto')][string]$Renderer='cpu',
    [switch]$RequireCoalescing
)
$ErrorActionPreference='Stop'
Set-Location (Resolve-Path "$PSScriptRoot/../../..")
$commit=git rev-parse HEAD
$dir="E:/AgentData/scratch/kwil/gui-daily-$Probe-$PID-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path "$dir/profile/kwil","$dir/workspace" -Force | Out-Null
@'
[files]
restore_session = false
[editor]
word_wrap = false
'@ | Set-Content "$dir/profile/kwil/settings.toml"
$fixture=0..199 | ForEach-Object { "line $_ " + ('wide text ' * 35) + " END$_" }
[IO.File]::WriteAllText("$dir/example.txt",($fixture -join "`n"),[Text.UTF8Encoding]::new($false))
Copy-Item -LiteralPath "$dir/example.txt" -Destination "$dir/original.txt"
$env:APPDATA="$dir/profile"
$env:LOCALAPPDATA="$dir/local"
$env:KWIL_WORKSPACE_ROOT="$dir/workspace"
$env:KWIL_INSTANCE_NAMESPACE="gui-daily-$PID-$([guid]::NewGuid().ToString('N'))"
$env:KWIL_NO_UPDATE='1'
$env:KWIL_FRAME_SCRIPT='1'
$env:KWIL_CAPTURE_DIR=$dir
if($Probe -eq 'resize') { $env:KWIL_RESIZE_PROBE='1' }
elseif($Probe -eq 'handback') { $env:KWIL_HANDBACK_PROBE='1' }
else { $env:KWIL_DAILY_USE_PROBE='1' }
if($RequireCoalescing) { $env:KWIL_RESIZE_REQUIRE_COALESCING='1' }
$exe=(Resolve-Path -LiteralPath $Executable).Path
$hash=(Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
[IO.File]::WriteAllText("$dir/artifact.txt","commit $commit`nexecutable $exe`nsha256 $hash`n")
$process=Start-Process -FilePath $exe -ArgumentList '--hidden','--renderer',$Renderer,"$dir/example.txt" -WindowStyle Hidden -PassThru -RedirectStandardOutput "$dir/stdout.txt" -RedirectStandardError "$dir/stderr.txt"
$processHandle=$process.Handle
Write-Output "Native PID $($process.Id), evidence $dir"
try {
    if(-not $process.WaitForExit(20000)) { Stop-Process -Id $process.Id; throw 'Hidden GUI probe timed out' }
    $exit=$process.ExitCode
} finally { if(-not $process.HasExited) { Stop-Process -Id $process.Id } }
foreach($name in @('stdout.txt','stderr.txt')) {
    $path=Join-Path $dir $name
    $content=[IO.File]::ReadAllText($path)
    [IO.File]::WriteAllText($path,"commit $commit`n$content")
}
Add-Type -AssemblyName System.Drawing
Get-ChildItem -LiteralPath $dir -Filter '*.bmp' | ForEach-Object {
    $bitmap=[Drawing.Bitmap]::FromFile($_.FullName)
    try { $bitmap.Save([IO.Path]::ChangeExtension($_.FullName,'.png'),[Drawing.Imaging.ImageFormat]::Png) }
    finally { $bitmap.Dispose() }
}
Get-Content "$dir/stdout.txt","$dir/stderr.txt"
foreach($name in @('harness-failure.txt','native-window.txt','daily-use.txt','handback-result.txt','resize-timing.txt','resize-idle.txt','frame-counts.txt')) {
    $path=Join-Path $dir $name
    if(Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path }
}
Write-Output "Exit $exit, evidence $dir"
if($exit -ne 0) { throw "Hidden GUI probe failed ($exit)" }
