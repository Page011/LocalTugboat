# Resize the visible top-level window whose title is exactly -Title to -Width x -Height (outer
# size in physical pixels), for the 900x600 and 1440x900 layout checks. WebView2's own
# Browser.setWindowBounds does nothing, so this is Win32 SetWindowPos.
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\resize.ps1 -Title Tugboat -Width 900 -Height 600
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][int]$Width,
    [Parameter(Mandatory = $true)][int]$Height
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
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
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

[Win]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null
$h = [Win]::Find($Title)
if ($h -eq [IntPtr]::Zero) { Write-Error "No visible window titled '$Title'"; exit 1 }
[Win]::ShowWindow($h, 9) | Out-Null   # SW_RESTORE: a maximized window cannot be resized
# SWP_NOZORDER (4) | SWP_NOMOVE (2): keep the position, set the size.
[Win]::SetWindowPos($h, [IntPtr]::Zero, 0, 0, $Width, $Height, 6) | Out-Null
$r = New-Object Win+RECT
[Win]::GetWindowRect($h, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
# Never trust the BOOL: check the rectangle.
if ($w -ne $Width -or $ht -ne $Height) { Write-Error "Window is ${w}x${ht}, wanted ${Width}x${Height}"; exit 1 }
Write-Output "${w}x${ht}"
