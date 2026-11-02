param([Parameter(Mandatory=$true)][string]$Executable, [string]$OutputDirectory, [string]$Revision, [switch]$WordWrap)
$ErrorActionPreference = 'Stop'
if (!$Revision) { $Revision = git rev-parse HEAD }
Write-Output "Commit $Revision; executable $Executable"
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class KwilInspect {
    public delegate bool Callback(IntPtr hwnd, IntPtr data);
    [DllImport("user32.dll")] public static extern bool EnumWindows(Callback callback, IntPtr data);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, System.Text.StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr PostMessage(IntPtr hwnd, uint msg, IntPtr wparam, IntPtr lparam);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd, int attr, out int value, int size);
    public struct Rect { public int left, top, right, bottom; }
    public static IntPtr Find(int processId) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            uint pid; GetWindowThreadProcessId(hwnd, out pid);
            var title = new System.Text.StringBuilder(512); GetWindowText(hwnd, title, 512);
            if (pid == processId && title.ToString().Contains("Kwil") && !title.ToString().StartsWith("Welcome") && !title.ToString().StartsWith("Settings")) { result = hwnd; return false; }
            return true;
        }, IntPtr.Zero);
        return result;
    }
    public static IntPtr FindPrefix(int processId, string prefix) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            uint pid; GetWindowThreadProcessId(hwnd, out pid);
            var title = new System.Text.StringBuilder(512); GetWindowText(hwnd, title, 512);
            if (pid == processId && title.ToString().StartsWith(prefix)) { result = hwnd; return false; }
            return true;
        }, IntPtr.Zero);
        return result;
    }
}
'@
Add-Type -ReferencedAssemblies @([Windows.Automation.AutomationElement].Assembly.Location, [Windows.Automation.AutomationEventArgs].Assembly.Location) @'
using System.Threading;
using System.Windows.Automation;
public static class KwilEvents {
    public static int Selections;
    public static int Focuses;
    public static int ProcessId;
    static readonly AutomationEventHandler selected = (sender, args) => Interlocked.Increment(ref Selections);
    static readonly AutomationFocusChangedEventHandler focused = (sender, args) => {
        try { if (((AutomationElement)sender).Current.ProcessId == ProcessId) Interlocked.Increment(ref Focuses); }
        catch (ElementNotAvailableException) { }
    };
    public static void Watch(AutomationElement editor, int processId) {
        ProcessId = processId;
        Automation.AddAutomationEventHandler(TextPattern.TextSelectionChangedEvent, editor, TreeScope.Element, selected);
        Automation.AddAutomationFocusChangedEventHandler(focused);
    }
}
'@
if (!$OutputDirectory) { $OutputDirectory = "E:/AgentData/reports/kwil-accessibility-$PID-$([guid]::NewGuid().ToString('N'))" }
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $OutputDirectory '.kwil') -Force | Out-Null
[IO.File]::WriteAllText((Join-Path $OutputDirectory '.kwil/welcome-v1.seen'), "kwil-welcome-v1`n")
$env:KWIL_WORKSPACE_ROOT = $OutputDirectory
$env:KWIL_NO_UPDATE = '1'
$env:APPDATA = Join-Path $OutputDirectory 'appdata'
New-Item -ItemType Directory -Path (Join-Path $env:APPDATA 'kwil') -Force | Out-Null
$wrapSetting = $WordWrap.IsPresent.ToString().ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $env:APPDATA 'kwil/settings.toml'), "[update]`ncheck = false`n[editor]`nword_wrap = $wrapSetting`n")
$fixture = Join-Path $OutputDirectory 'viewport.txt'
$japanese = -join @([char]0x65e5, [char]0x672c, [char]0x8a9e)
[IO.File]::WriteAllText($fixture, "Accessible first line`n$japanese second line`n" + ("outside viewport`n" * 100))
$errorLog = Join-Path $OutputDirectory 'stderr.log'
$process = Start-Process -FilePath $Executable -ArgumentList @('--hidden','--gui','--renderer=cpu', $fixture) -WindowStyle Hidden -RedirectStandardError $errorLog -PassThru
Write-Output "Started Kwil PID $($process.Id); timeout 30 seconds"
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    $window = $null
    $editor = $null
    $pattern = $null
    while ([DateTime]::UtcNow -lt $deadline -and !$process.HasExited) {
        $process.Refresh()
        $hwnd = [KwilInspect]::Find($process.Id)
        if ($hwnd -ne [IntPtr]::Zero) {
            $window = [Windows.Automation.AutomationElement]::FromHandle($hwnd)
            $editor = $window.FindFirst([Windows.Automation.TreeScope]::Descendants,
                [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Editor'))
            if ($editor) {
                try {
                    $pattern = [Windows.Automation.TextPattern]$editor.GetCurrentPattern([Windows.Automation.TextPattern]::Pattern)
                    if ($pattern.DocumentRange.GetText(-1).Contains('Accessible first line')) { break }
                } catch [InvalidOperationException] { }
            }
        }
        Start-Sleep -Milliseconds 150
    }
    if (!$editor) { throw 'No accessible Editor node in the hidden Kwil window' }
    if (!$pattern) { throw 'Editor did not expose TextPattern' }
    # Debug builds deliberately show Welcome even with a seen marker. Close
    # that owned, cloaked modal before testing actions on the editor behind it.
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $welcomeHwnd = [KwilInspect]::FindPrefix($process.Id, 'Welcome')
        if ($welcomeHwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($welcomeHwnd -ne [IntPtr]::Zero) {
        [KwilInspect]::PostMessage($welcomeHwnd, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 200
        if ([KwilInspect]::FindPrefix($process.Id, 'Welcome') -ne [IntPtr]::Zero) { throw 'Owned welcome modal did not close' }
    }
    # Prepared wrapping and cold fonts can replace the initial viewport page.
    # Reacquire the text provider after the owned modal has finished closing.
    Start-Sleep -Milliseconds 500
    $pattern = [Windows.Automation.TextPattern]$editor.GetCurrentPattern([Windows.Automation.TextPattern]::Pattern)
    $text = $pattern.DocumentRange.GetText(-1)
    if (!$text.Contains('Accessible first line') -or !$text.Contains("$japanese second line")) { throw 'Visible Unicode text missing' }
    if ($text.Length -ge [IO.File]::ReadAllText($fixture).Length) { throw 'Whole document exposed instead of viewport' }
    $selection = $pattern.GetSelection()
    if ($selection.Length -ne 1) { throw 'Caret/selection is missing' }
    [KwilEvents]::Watch($editor, $process.Id)
    # Exercise the focus notification seam on our own cloaked HWND. This posts
    # WM_SETFOCUS without calling SetFocus or changing the owner's foreground.
    [KwilInspect]::PostMessage($hwnd, 7, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
    # Select through UIA's TextPattern; no native SetFocus or global keyboard input.
    $range = $pattern.DocumentRange.Clone()
    $range.MoveEndpointByUnit([Windows.Automation.Text.TextPatternRangeEndpoint]::End, [Windows.Automation.Text.TextUnit]::Character, -($text.Length - 4)) | Out-Null
    $range.Select()
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    $selectedText = ''
    do {
        Start-Sleep -Milliseconds 100
        try {
            $pattern = [Windows.Automation.TextPattern]$editor.GetCurrentPattern([Windows.Automation.TextPattern]::Pattern)
            $selectedText = $pattern.GetSelection()[0].GetText(-1)
        } catch [InvalidOperationException] { }
        if ($selectedText -eq 'Acce') { break }
    } while ([DateTime]::UtcNow -lt $deadline -and !$process.HasExited)
    if ($selectedText -ne 'Acce') { throw "UIA selection action mismatch: requested '$($range.GetText(-1))', actual '$selectedText', events $([KwilEvents]::Selections)" }
    if ([KwilEvents]::Selections -eq 0) { throw 'UIA did not raise TextSelectionChanged' }
    $nodes = $window.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
    $names = @($nodes | ForEach-Object { "$($_.Current.ControlType.ProgrammaticName): $($_.Current.Name)" })
    @("Commit $Revision") + $names | Set-Content (Join-Path $OutputDirectory 'uia-tree.txt') -Encoding UTF8
    if (!($names -match 'MenuBar') -or !($names -match 'TabItem') -or !($names -match 'StatusBar')) { throw 'GUI chrome missing' }
    $cloaked = 0
    [KwilInspect]::DwmGetWindowAttribute($hwnd, 14, [ref]$cloaked, 4) | Out-Null
    if (!$cloaked) { throw 'Inspection window must remain DWM-cloaked' }
    $rect = New-Object KwilInspect+Rect
    [KwilInspect]::GetClientRect($hwnd, [ref]$rect) | Out-Null
    $bitmap = New-Object Drawing.Bitmap($rect.right, $rect.bottom)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $dc = $graphics.GetHdc()
    try { [KwilInspect]::PrintWindow($hwnd, $dc, 1) | Out-Null } finally { $graphics.ReleaseHdc($dc) }
    $bitmap.Save((Join-Path $OutputDirectory 'hidden-window.png'), [Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    function Invoke-Item($root, $name) {
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            $node = $root.FindFirst([Windows.Automation.TreeScope]::Descendants,
                [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, $name))
            if ($node -and $node.Current.IsEnabled) { break }
            Start-Sleep -Milliseconds 100
        } while ([DateTime]::UtcNow -lt $deadline)
        if (!$node) { throw "Missing accessible menu item: $name" }
        ([Windows.Automation.InvokePattern]$node.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)).Invoke()
        Start-Sleep -Milliseconds 150
    }
    Invoke-Item $window 'View'
    Invoke-Item $window ('Command Palette' + [char]0x2026)
    $palette = $window.FindFirst([Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::List))
    if (!$palette) { throw 'Command palette did not expose a list' }
    [KwilInspect]::PostMessage($hwnd, 0x100, [IntPtr]27, [IntPtr]::Zero) | Out-Null
    [KwilInspect]::PostMessage($hwnd, 0x101, [IntPtr]27, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 200
    Invoke-Item $window 'Search'
    Invoke-Item $window ('Go to Line' + [char]0x2026)
    $prompt = $window.FindFirst([Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, 'Go to Line'))
    if (!$prompt) { throw 'Go to Line did not expose its labeled prompt' }
    [KwilInspect]::PostMessage($hwnd, 0x100, [IntPtr]27, [IntPtr]::Zero) | Out-Null
    [KwilInspect]::PostMessage($hwnd, 0x101, [IntPtr]27, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 200
    Invoke-Item $window 'File'
    Invoke-Item $window 'Settings'
    Start-Sleep -Milliseconds 200
    $settingsHwnd = [KwilInspect]::FindPrefix($process.Id, 'Settings')
    if (!$settingsHwnd) { throw 'Settings window did not open through UIA' }
    $settings = [Windows.Automation.AutomationElement]::FromHandle($settingsHwnd)
    # Activation can initially return AccessKit's placeholder until the GUI handles its wake.
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $list = $settings.FindFirst([Windows.Automation.TreeScope]::Descendants,
            [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::List))
        if ($list) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if (!$list) { throw 'Settings dialog did not expose a labeled list' }
    [KwilInspect]::DwmGetWindowAttribute($settingsHwnd, 14, [ref]$cloaked, 4) | Out-Null
    if (!$cloaked) { throw 'Settings window must remain DWM-cloaked' }
    if ([KwilEvents]::Focuses -eq 0) { throw 'UIA did not raise focus notifications' }
    Write-Output "PASS: hidden UIA tree, Unicode viewport text ($($text.Length) chars), selection action and events ($([KwilEvents]::Selections)), focus notifications ($([KwilEvents]::Focuses)), menu, tabs, status"
} finally {
    [Windows.Automation.Automation]::RemoveAllEventHandlers()
    if (!$process.HasExited) { Stop-Process -Id $process.Id }
    $process.WaitForExit(5000) | Out-Null
    if (Test-Path $errorLog) {
        $errors = [IO.File]::ReadAllText($errorLog)
        [IO.File]::WriteAllText($errorLog, "Commit $Revision`n$errors")
        if ($errors) { Write-Output $errors }
    }
    Write-Output "Closed owned PID $($process.Id); evidence $OutputDirectory"
}
