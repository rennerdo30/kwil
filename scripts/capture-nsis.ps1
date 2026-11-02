# Native welcome-page evidence on disposable hosted Windows only.
param([Parameter(Mandatory)][string]$Setup, [Parameter(Mandatory)][string]$Output)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'Installer capture runs only on GitHub-hosted Windows.'
}
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SetupCapture {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr dc, uint flags);
    private delegate bool Callback(IntPtr hwnd, IntPtr unused);
    [DllImport("user32.dll")] private static extern bool EnumWindows(Callback callback, IntPtr unused);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    public static IntPtr Window(uint pid) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, unused) => { uint owner; GetWindowThreadProcessId(hwnd, out owner);
            if (owner != pid) return true; found = hwnd; return false; }, IntPtr.Zero);
        return found;
    }
}
'@
$process = Start-Process -FilePath $Setup -PassThru -WindowStyle Hidden
Write-Host "Capture PID $($process.Id) (timeout 30 s)"
try {
    if (!$process.WaitForInputIdle(30000)) { throw 'Setup did not become idle within 30 s' }
    $process.Refresh()
    $window = [SetupCapture]::Window($process.Id)
    if (!$window) { throw 'Setup has no window' }
    $rect = [SetupCapture+Rect]::new()
    if (![SetupCapture]::GetWindowRect($window, [ref]$rect)) { throw 'GetWindowRect failed' }
    $bitmap = [Drawing.Bitmap]::new($rect.Right - $rect.Left, $rect.Bottom - $rect.Top)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $dc = $graphics.GetHdc()
    try {
        if (![SetupCapture]::PrintWindow($window, $dc, 2)) { throw 'PrintWindow failed' }
    } finally { $graphics.ReleaseHdc($dc); $graphics.Dispose() }
    try { $bitmap.Save($Output, [Drawing.Imaging.ImageFormat]::Png) } finally { $bitmap.Dispose() }
} finally {
    # Close only the process started above. Cancellation can show a prompt, so
    # terminate this unmodified welcome-page process after the bounded capture.
    if (!$process.HasExited) { $process.Kill(); $process.WaitForExit(5000) | Out-Null }
    $process.Dispose()
}
