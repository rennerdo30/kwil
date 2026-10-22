param([switch]$BrokenFontRecovery)
$ErrorActionPreference='Stop'
Set-Location (Resolve-Path "$PSScriptRoot/../../..")
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-11-renderer'
$commit=git rev-parse HEAD
$sourcePath=Join-Path $PWD 'crates/ui-gpu/src/lib.rs'
$source=[IO.File]::ReadAllText($sourcePath)
$case=if($BrokenFontRecovery){'broken-font'}else{'restored-font-rtl'}
$dir="E:/Development/_cache/kwil-kw11-evidence/review-$case-$PID-1"
New-Item -ItemType Directory -Path "$dir/settings/kwil" -Force | Out-Null
@'
[font]
family = "KwilMissingFontReviewFixture-8fc1"
'@ | Set-Content "$dir/settings/kwil/settings.toml"
$line=[Text.Encoding]::UTF8.GetBytes("ASCII normal`nCJK 日本語 中文`nEmoji 😀 👩‍💻`nCombining Cafe$([char]0x301)`nאבג`nمرحبا`nabc אבג 12`nא$([char]0x05b7)ב`nInvalid ")
$bytes=[Collections.Generic.List[byte]]::new()
for($i=0;$i -lt 16;$i++){$bytes.AddRange($line);$bytes.AddRange([byte[]]@(255,192,128,10))}
[IO.File]::WriteAllBytes("$dir/mixed.bin",$bytes.ToArray())
try {
    if($BrokenFontRecovery) {
        $anchor='                self.activate_unicode(true);'
        if(-not $source.Contains($anchor)){throw 'Font-recovery mutation anchor missing'}
        [IO.File]::WriteAllText($sourcePath,$source.Replace($anchor,'                let _ = &self.settings;'))
    }
    $build=& cargo build -p kwil --release --features kwil-ui-gpu/frame-harness --locked -j 4 --timings 2>&1 | Out-String
    [IO.File]::WriteAllText("$dir/build.txt","commit $commit ($case)`n$build")
    if($LASTEXITCODE -ne 0){throw $build}
    $env:KWIL_NO_UPDATE='1'
    $env:KWIL_FRAME_SCRIPT='1'
    $env:KWIL_REQUIRE_UNICODE='1'
    $env:KWIL_CAPTURE_DIR=$dir
    $env:APPDATA="$dir/settings"
    Remove-Item Env:KWIL_TRACE_STARTUP -ErrorAction SilentlyContinue
    $process=Start-Process -FilePath "$env:CARGO_TARGET_DIR/release/kwil.exe" -ArgumentList '--hidden',"$dir/mixed.bin" -WindowStyle Hidden -PassThru -RedirectStandardOutput "$dir/stdout.txt" -RedirectStandardError "$dir/stderr.txt"
    Write-Output "Native PID $($process.Id) ($case)"
    try {
        if(-not $process.WaitForExit(15000)){Stop-Process -Id $process.Id;throw 'Hidden renderer timeout'}
        $exit=$process.ExitCode
    } finally {if(-not $process.HasExited){Stop-Process -Id $process.Id}}
    $out=[IO.File]::ReadAllText("$dir/stdout.txt")
    $err=[IO.File]::ReadAllText("$dir/stderr.txt")
    [IO.File]::WriteAllText("$dir/stdout.txt","commit $commit ($case)`n$out")
    [IO.File]::WriteAllText("$dir/stderr.txt","commit $commit ($case)`n$err")
    if($BrokenFontRecovery) {
        if($exit -eq 0 -or -not $err.Contains('Unicode recovery unavailable')){throw "Expected Unicode-recovery assertion failure, exit=$exit`n$out`n$err"}
        Write-Output 'RED native configured-font recovery assertion'
    } else {
        if($exit -ne 0 -or -not $out.Contains('unicode_enabled=true') -or -not $out.Contains('glyphs_pending=0') -or -not $out.Contains('scroll_copy_matches_repaint=true') -or -not $out.Contains('backing_pixels_equal_cpu=Ok(true)')){throw "Native renderer checks failed, exit=$exit`n$out`n$err"}
        Add-Type -AssemblyName System.Drawing
        $image=[Drawing.Bitmap]::FromFile("$dir/first.bmp")
        try {$image.Save("$dir/first.png",[Drawing.Imaging.ImageFormat]::Png)} finally {$image.Dispose()}
        Write-Output $out
    }
    Write-Output "Evidence: $dir"
} finally {[IO.File]::WriteAllText($sourcePath,$source)}
