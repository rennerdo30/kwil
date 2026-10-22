param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$Fixture,
    [Parameter(Mandatory)][string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Drawing.Common,System.Private.Windows.GdiPlus,System.Private.Windows.Core,System.Drawing.Primitives -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class KwilGuiProbe {
    public delegate bool EnumCallback(IntPtr window, IntPtr state);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumCallback callback, IntPtr state);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr window, uint message, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr window);
    [DllImport("user32.dll")] static extern int ReleaseDC(IntPtr window, IntPtr dc);
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr context);
    [DllImport("gdi32.dll")] static extern IntPtr CreateCompatibleDC(IntPtr dc);
    [DllImport("gdi32.dll")] static extern IntPtr CreateCompatibleBitmap(IntPtr dc, int width, int height);
    [DllImport("gdi32.dll")] static extern IntPtr SelectObject(IntPtr dc, IntPtr value);
    [DllImport("gdi32.dll")] static extern bool BitBlt(IntPtr target, int x, int y, int width, int height, IntPtr source, int sx, int sy, uint operation);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr value);
    [DllImport("gdi32.dll")] static extern bool DeleteDC(IntPtr value);
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    public static void Dpi() { SetProcessDpiAwarenessContext(new IntPtr(-4)); }
    public static IntPtr Find(uint process) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((window,state) => { uint owner; GetWindowThreadProcessId(window,out owner);
            if(owner == process) { Rect r; GetClientRect(window,out r); if(r.Right > 100 && r.Bottom > 100) { found=window; return false; }} return true;
        },IntPtr.Zero);
        return found;
    }
    public static void Mouse(IntPtr window, uint message, int x, int y, int buttons) {
        SendMessageW(window,message,new IntPtr(buttons),new IntPtr((y << 16) | (x & 65535)));
    }
    public static int Width(IntPtr window) { Rect r; GetClientRect(window,out r); return r.Right; }
    public static int Height(IntPtr window) { Rect r; GetClientRect(window,out r); return r.Bottom; }
    public static void Capture(IntPtr window,string path) {
        Rect r; GetClientRect(window,out r);
        var dc=GetDC(window); var target=CreateCompatibleDC(dc); var bitmap=CreateCompatibleBitmap(dc,r.Right,r.Bottom);
        var old=SelectObject(target,bitmap);
        try {
            if(!BitBlt(target,0,0,r.Right,r.Bottom,dc,0,0,0x00CC0020)) throw new Exception("capture failed");
            using(var image=Image.FromHbitmap(bitmap)) image.Save(path,ImageFormat.Png);
        } finally { SelectObject(target,old); DeleteObject(bitmap); DeleteDC(target); ReleaseDC(window,dc); }
    }
}
'@
[KwilGuiProbe]::Dpi()
New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
$taskProcess = Start-Process -FilePath $Executable -ArgumentList @('--hidden',('"{0}"' -f $Fixture)) -WindowStyle Hidden -PassThru
Write-Output "Started owned PID $($taskProcess.Id)"
try {
    $taskDeadline = [DateTime]::UtcNow.AddSeconds(10)
    $taskWindow = [IntPtr]::Zero
    while ($taskWindow -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $taskDeadline) {
        if ($taskProcess.HasExited) { throw 'GUI exited before creating a window' }
        Start-Sleep -Milliseconds 100
        $taskWindow = [KwilGuiProbe]::Find($taskProcess.Id)
    }
    if ($taskWindow -eq [IntPtr]::Zero) { throw 'Owned window not found within 10 seconds' }
    Start-Sleep -Milliseconds 500
    [KwilGuiProbe]::Capture($taskWindow,(Join-Path $OutputDirectory 'before.png'))
    $taskWidth=[KwilGuiProbe]::Width($taskWindow)
    $taskHeight=[KwilGuiProbe]::Height($taskWindow)
    # Track click below the vertical thumb; no physical desktop input or focus.
    [KwilGuiProbe]::Mouse($taskWindow,0x200,($taskWidth-7),($taskHeight/2),0)
    [KwilGuiProbe]::Mouse($taskWindow,0x201,($taskWidth-7),($taskHeight/2),1)
    [KwilGuiProbe]::Mouse($taskWindow,0x202,($taskWidth-7),($taskHeight/2),0)
    Start-Sleep -Milliseconds 200
    [KwilGuiProbe]::Capture($taskWindow,(Join-Path $OutputDirectory 'vertical-page.png'))
    # Horizontal track lies immediately above the status row (font height 20).
    [KwilGuiProbe]::Mouse($taskWindow,0x200,($taskWidth-40),($taskHeight-27),0)
    [KwilGuiProbe]::Mouse($taskWindow,0x201,($taskWidth-40),($taskHeight-27),1)
    [KwilGuiProbe]::Mouse($taskWindow,0x202,($taskWidth-40),($taskHeight-27),0)
    Start-Sleep -Milliseconds 200
    [KwilGuiProbe]::Capture($taskWindow,(Join-Path $OutputDirectory 'horizontal-page.png'))
    Write-Output "Captured owned HWND $taskWindow (${taskWidth}x${taskHeight})"
} finally {
    if (!$taskProcess.HasExited) { $taskProcess.Kill(); $taskProcess.WaitForExit(5000) | Out-Null }
    Write-Output "Closed owned PID $($taskProcess.Id)"
}
