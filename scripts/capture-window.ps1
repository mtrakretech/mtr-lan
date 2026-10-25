param([Parameter(Mandatory=$true)][int]$ProcessId, [Parameter(Mandatory=$true)][string]$OutputPath)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LanMouseWindowCapture {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
'@
$process = Get-Process -Id $ProcessId
$handle = $process.MainWindowHandle
if ($handle -eq [IntPtr]::Zero) { throw 'The process has no visible main window.' }
[LanMouseWindowCapture]::SetProcessDPIAware() | Out-Null
$rect = New-Object LanMouseWindowCapture+RECT
if (![LanMouseWindowCapture]::GetWindowRect($handle, [ref]$rect)) { throw 'Could not get window bounds.' }
$bitmap = New-Object Drawing.Bitmap ($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top)
$graphics = [Drawing.Graphics]::FromImage($bitmap)
$dc = $graphics.GetHdc()
try {
    if (![LanMouseWindowCapture]::PrintWindow($handle, $dc, 2)) { throw 'PrintWindow failed.' }
} finally { $graphics.ReleaseHdc($dc); $graphics.Dispose() }
try { $bitmap.Save($OutputPath, [Drawing.Imaging.ImageFormat]::Png) } finally { $bitmap.Dispose() }
Write-Output "Captured only MTR-LAN-Copy's window: $OutputPath"
