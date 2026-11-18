# Native window screenshot (CopyFromScreen) of the visible top-level window whose title is
# exactly -Title, to a PNG. WebView2 content is only captured by the screen, not by PrintWindow,
# so this is the way. Adapted from Vera's scripts\screenshot.ps1.
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\screenshot.ps1 -Title Tugboat -Out shot.png
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][string]$Out,
    [int]$Timeout = 10
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class Win {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern void SwitchToThisWindow(IntPtr hWnd, bool altTab);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr hWnd, int flags);
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd, int attr, out RECT rect, int size);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public static IntPtr Find(string title) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, l) => {
            if (!IsWindowVisible(h)) return true;
            var sb = new StringBuilder(256);
            GetWindowText(h, sb, 256);
            if (sb.ToString() == title) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@
Add-Type -AssemblyName System.Drawing

# Per-monitor v2 DPI awareness, so the rectangle and the pixels agree on a scaled display.
[Win]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null

$deadline = (Get-Date).AddSeconds($Timeout)
$h = [IntPtr]::Zero
while ($h -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline) {
    $h = [Win]::Find($Title)
    if ($h -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 300 }
}
if ($h -eq [IntPtr]::Zero) { Write-Error "No visible window titled '$Title'"; exit 1 }

if ([Win]::IsIconic($h)) { [Win]::ShowWindow($h, 9) | Out-Null }   # SW_RESTORE

function Get-Rect {
    $r = New-Object Win+RECT
    # DWMWA_EXTENDED_FRAME_BOUNDS = 9: the visible frame without the invisible resize borders.
    if ([Win]::DwmGetWindowAttribute($h, 9, [ref]$r, [System.Runtime.InteropServices.Marshal]::SizeOf($r)) -ne 0) {
        Write-Error "Could not read the window rectangle"; exit 1
    }
    return $r
}

# CopyFromScreen captures whatever is on top, so the window must really be on top at its centre.
# If another window covers it, ask again a few times; if it still is not on top, stop rather
# than photograph someone else's windows.
$onTop = $false
for ($try = 0; $try -lt 5 -and -not $onTop; $try++) {
    [Win]::SetForegroundWindow($h) | Out-Null
    # SwitchToThisWindow is not held back by the foreground lock while the user is typing elsewhere.
    if ($try -gt 0) { [Win]::SwitchToThisWindow($h, $true) }
    Start-Sleep -Milliseconds 400
    $r = Get-Rect
    $pt = New-Object Win+POINT
    $pt.X = [int](($r.Left + $r.Right) / 2); $pt.Y = [int](($r.Top + $r.Bottom) / 2)
    $onTop = ([Win]::GetAncestor([Win]::WindowFromPoint($pt), 2) -eq $h)
}
if (-not $onTop) { Write-Error "'$Title' is covered by another window; nothing captured"; exit 2 }
$w = $r.Right - $r.Left
$ht = $r.Bottom - $r.Top
if ($w -le 0 -or $ht -le 0) { Write-Error "Window has no size"; exit 1 }

New-Item -ItemType Directory -Force (Split-Path -Parent $Out) | Out-Null
$bmp = New-Object System.Drawing.Bitmap $w, $ht
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "$Out ${w}x${ht}"
