# Launch only as a NEW hidden-console powershell.exe child, with a 45 s
# controller timeout. Pass that controller's PID. This helper refuses an
# inherited/visible console and never allocates or targets an owner console.
param([Parameter(Mandatory=$true)][string]$BuildDir, [Parameter(Mandatory=$true)][string]$ResultPath, [Parameter(Mandatory=$true)][uint32]$ControllerPid)
$ErrorActionPreference='Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class KwilConsoleProbe {
    [DllImport("kernel32.dll")] public static extern uint GetConsoleCP();
    [DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow();
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
    [DllImport("kernel32.dll")] public static extern uint GetConsoleProcessList(uint[] ids, uint count);
    [DllImport("kernel32.dll")] public static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);
    [DllImport("kernel32.dll")] public static extern bool GenerateConsoleCtrlEvent(uint kind, uint group);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr window, int attr, out int value, int size);
}
"@
$owned=@()
$result=@{helperPid=$PID; consoleCodePage=[KwilConsoleProbe]::GetConsoleCP()}
try {
    if ($result.consoleCodePage -eq 0) { throw 'Probe requires a dedicated hidden console; never allocate a visible one' }
    $parent=(Get-CimInstance Win32_Process -Filter "ProcessId = $PID").ParentProcessId
    $initialPids=New-Object 'System.UInt32[]' 32
    $initialCount=[KwilConsoleProbe]::GetConsoleProcessList($initialPids,32)
    $consoleWindow=[KwilConsoleProbe]::GetConsoleWindow()
    if ($parent -ne $ControllerPid -or $ControllerPid -eq $PID -or $initialCount -ne 1 -or $initialPids[0] -ne $PID -or [KwilConsoleProbe]::IsWindowVisible($consoleWindow)) {
        throw 'Refusing control event: console is not the controller-owned, dedicated hidden helper console'
    }
    $psi=New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName=Join-Path $BuildDir 'kwil.com'
    $psi.Arguments='--hidden'
    $psi.UseShellExecute=$false
    $psi.RedirectStandardOutput=$true
    $psi.RedirectStandardError=$true
    $psi.EnvironmentVariables['KWIL_NO_UPDATE']='1'
    $psi.EnvironmentVariables['KWIL_TRACE_LAUNCH']='1'
    $launcher=[System.Diagnostics.Process]::Start($psi)
    $owned += $launcher
    $result.launcherPid=$launcher.Id
    $lineTask=$launcher.StandardError.ReadLineAsync()
    if (-not $lineTask.Wait(10000)) { throw 'GUI PID acknowledgement timeout' }
    if ($lineTask.Result -notmatch 'GUI PID (\d+)') { throw ('Missing recorded GUI PID: ' + $lineTask.Result) }
    $gui=[System.Diagnostics.Process]::GetProcessById([int]$Matches[1])
    $owned += $gui
    $result.guiPid=$gui.Id
    if (-not $launcher.WaitForExit(20000)) { throw 'GUI launcher timeout' }
    $result.launcherExit=$launcher.ExitCode
    $stdoutTask=$launcher.StandardOutput.ReadToEndAsync()
    $stderrTask=$launcher.StandardError.ReadToEndAsync()
    $result.stdoutEof=$stdoutTask.Wait(1000)
    $result.stderrEof=$stderrTask.Wait(1000)
    $consolePids=New-Object 'System.UInt32[]' 32
    $count=[KwilConsoleProbe]::GetConsoleProcessList($consolePids,32)
    $result.guiStillAttached=($consolePids -contains [uint32]$gui.Id)
    $cloaked=0
    $result.cloakResult=[KwilConsoleProbe]::DwmGetWindowAttribute($gui.MainWindowHandle,14,[ref]$cloaked,4)
    $result.cloaked=$cloaked
    # Install ignore only after launching the editor, so it cannot inherit it.
    if (-not [KwilConsoleProbe]::SetConsoleCtrlHandler([IntPtr]::Zero,$true)) { throw 'Cannot protect probe from Ctrl+C' }
    $result.ctrlCGenerated=[KwilConsoleProbe]::GenerateConsoleCtrlEvent(0,0)
    $result.guiExitedAfterCtrlC=$gui.WaitForExit(800)
    if ($gui.HasExited) { $result.guiExitAfterCtrlC=$gui.ExitCode }
    # Direct .exe GUI launches must also release a shell's console and pipes.
    [KwilConsoleProbe]::SetConsoleCtrlHandler([IntPtr]::Zero,$false) | Out-Null
    $directPsi=New-Object System.Diagnostics.ProcessStartInfo
    $directPsi.FileName=Join-Path $BuildDir 'kwil.exe'
    $directPsi.Arguments='--hidden'
    $directPsi.UseShellExecute=$false
    $directPsi.RedirectStandardOutput=$true
    $directPsi.RedirectStandardError=$true
    $directPsi.EnvironmentVariables['KWIL_NO_UPDATE']='1'
    $direct=[System.Diagnostics.Process]::Start($directPsi)
    $owned += $direct
    $result.directGuiPid=$direct.Id
    $directOut=$direct.StandardOutput.ReadToEndAsync()
    $directErr=$direct.StandardError.ReadToEndAsync()
    $result.directStdoutEof=$directOut.Wait(10000)
    $result.directStderrEof=$directErr.Wait(1000)
    $directConsolePids=New-Object 'System.UInt32[]' 32
    [KwilConsoleProbe]::GetConsoleProcessList($directConsolePids,32) | Out-Null
    $result.directStillAttached=($directConsolePids -contains [uint32]$direct.Id)
    [KwilConsoleProbe]::SetConsoleCtrlHandler([IntPtr]::Zero,$true) | Out-Null
    if (-not [KwilConsoleProbe]::GenerateConsoleCtrlEvent(0,0)) { throw 'Cannot generate second probe Ctrl+C' }
    $result.directExitedAfterCtrlC=$direct.WaitForExit(800)

    # The terminal entry must keep its real console through its first frame.
    $tuiPsi=New-Object System.Diagnostics.ProcessStartInfo
    $tuiPsi.FileName=Join-Path $BuildDir 'kwil.com'
    $tuiPsi.Arguments='--tui'
    $tuiPsi.UseShellExecute=$false
    $tuiPsi.RedirectStandardError=$true
    $tuiPsi.EnvironmentVariables['KWIL_NO_UPDATE']='1'
    $tuiPsi.EnvironmentVariables['KWIL_TRACE_LAUNCH']='1'
    $tui=[System.Diagnostics.Process]::Start($tuiPsi)
    $owned += $tui
    $result.tuiPid=$tui.Id
    $tuiReady=$tui.StandardError.ReadLineAsync()
    if (-not $tuiReady.Wait(10000)) { throw 'TUI first-frame acknowledgement timeout' }
    $result.tuiReady=($tuiReady.Result -eq 'kwil: TUI ready')
    $tuiPids=New-Object 'System.UInt32[]' 32
    [KwilConsoleProbe]::GetConsoleProcessList($tuiPids,32) | Out-Null
    $result.tuiAttached=($tuiPids -contains [uint32]$tui.Id)

    # GUI startup errors still reach the shell via launcher-owned IPC.
    $badPsi=New-Object System.Diagnostics.ProcessStartInfo
    $badPsi.FileName=Join-Path $BuildDir 'kwil.com'
    $missing=Join-Path ([System.IO.Path]::GetDirectoryName($ResultPath)) ('kwil-missing-' + $PID + '-1-' + [guid]::NewGuid().ToString('N'))
    $badPsi.Arguments='--hidden "' + $missing + '"'
    $badPsi.UseShellExecute=$false
    $badPsi.RedirectStandardOutput=$true
    $badPsi.RedirectStandardError=$true
    $badPsi.EnvironmentVariables['KWIL_NO_UPDATE']='1'
    $bad=[System.Diagnostics.Process]::Start($badPsi)
    $owned += $bad
    $result.errorLauncherPid=$bad.Id
    $badErr=$bad.StandardError.ReadToEndAsync()
    if (-not $bad.WaitForExit(10000) -or -not $badErr.Wait(1000)) { throw 'Startup error diagnostics timeout' }
    $result.startupDiagnostics=($bad.ExitCode -eq 1 -and $badErr.Result.Contains('cannot open'))
    $result.passed=($result.launcherExit -eq 0 -and $result.stdoutEof -and $result.stderrEof -and -not $result.guiStillAttached -and -not $result.guiExitedAfterCtrlC -and $result.cloaked -ne 0 -and $result.directStdoutEof -and $result.directStderrEof -and -not $result.directStillAttached -and -not $result.directExitedAfterCtrlC -and $result.tuiReady -and $result.tuiAttached -and $result.startupDiagnostics)
} catch {
    $result.error=$_.Exception.Message
    $result.passed=$false
} finally {
    foreach ($process in $owned) {
        try { if (-not $process.HasExited) { $process.Kill(); if (-not $process.WaitForExit(5000)) { throw 'Owned child failed to exit' } } } catch { $result.cleanupError=$_.Exception.Message }
    }
    $result | ConvertTo-Json | Set-Content -LiteralPath $ResultPath
}
if (-not $result.passed) { exit 1 }
