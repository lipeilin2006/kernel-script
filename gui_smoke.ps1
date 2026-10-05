param([string]$Exe = 'D:\kernel-script\target\gui-release\ks-gui.exe')
$ErrorActionPreference = 'Continue'
$log = 'D:\kernel-script\gui_smoke.log'
Remove-Item $log -ErrorAction SilentlyContinue

function W($m) {
    $line = ("{0} {1}`r`n" -f (Get-Date -Format 'HH:mm:ss.fff'), $m)
    $b = [Text.Encoding]::UTF8.GetBytes($line)
    $fs = New-Object IO.FileStream($log, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::Read, 4096, [IO.FileOptions]::WriteThrough)
    $fs.Write($b, 0, $b.Length)
    $fs.Close()
}

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class GuiClose {
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
}
'@

$exe = $Exe
$work = Split-Path -Parent $exe
$guiLog = Join-Path $work 'ks-gui.log'
Remove-Item $guiLog -ErrorAction SilentlyContinue
W '=== ks-gui runtime smoke (elevated) ==='
W "exe=$exe"
W 'auto-start: built in (no env var)'

$env:RUST_BACKTRACE = '1'

$errPath = Join-Path $work 'gui-stderr.txt'
$outPath = Join-Path $work 'gui-stdout.txt'
Remove-Item $errPath, $outPath -ErrorAction SilentlyContinue
$proc = Start-Process -FilePath $exe -WorkingDirectory $work -RedirectStandardError $errPath -RedirectStandardOutput $outPath -PassThru
W "gui started pid=$($proc.Id)"

$exited = $false
foreach ($i in 1..20) {
    Start-Sleep -Milliseconds 500
    $proc.Refresh()
    if ($proc.HasExited) { $exited = $true; break }
}
if ($exited) {
    W "gui exited early code=$($proc.ExitCode)"
} else {
    W 'gui alive after 10s'
    $key = Test-Path 'HKLM:\SOFTWARE\KernelScript'
    W "registry key present=$key"
    if ($key) {
        $k = Get-Item 'HKLM:\SOFTWARE\KernelScript'
        $k.GetValueNames() | ForEach-Object { W "  value: $_ = $($k.GetValue($_))" }
    }

    # Screenshot the top-left corner so the status window can be verified
    # visually (driver: running above the Stop button).
    try {
        Add-Type -AssemblyName System.Drawing
        Add-Type -AssemblyName System.Windows.Forms
        $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bmp = New-Object System.Drawing.Bitmap($bounds.Width, [Math]::Min(400, $bounds.Height))
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen(0, 0, 0, 0, $bmp.Size)
        $shot = 'D:\kernel-script\gui_status.png'
        Remove-Item $shot -ErrorAction SilentlyContinue
        $bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
        $g.Dispose(); $bmp.Dispose()
        W "screenshot saved=$shot"
    } catch { W "screenshot failed: $_" }

    # Close through the same path the Stop button takes: WM_CLOSE makes
    # GLFW set should_close, the render loop returns and the GUI stops
    # the driver itself on the way out.
    $proc.Refresh()
    $hwnd = $proc.MainWindowHandle
    if ($hwnd -ne [IntPtr]::Zero) {
        $posted = [GuiClose]::PostMessage($hwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
        W "posted WM_CLOSE to hwnd=$hwnd ok=$posted"
    } else {
        W 'no main window handle for WM_CLOSE'
    }
    $closed = $false
    foreach ($i in 1..60) {
        Start-Sleep -Milliseconds 500
        $proc.Refresh()
        if ($proc.HasExited) { $closed = $true; break }
    }
    if ($closed) {
        W "gui exited after close code=$($proc.ExitCode)"
        W "registry key present after gui exit=$(Test-Path 'HKLM:\SOFTWARE\KernelScript')"
    } else {
        W 'gui did NOT exit after close; force stopping'
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
}

W "ks-gui.log exists=$(Test-Path $guiLog)"
if (Test-Path $guiLog) {
    Get-Content $guiLog | ForEach-Object { W "  log: $_" }
}
if (Test-Path $errPath) {
    Get-Content $errPath | ForEach-Object { W "  stderr: $_" }
}
if (Test-Path $outPath) {
    Get-Content $outPath | ForEach-Object { W "  stdout: $_" }
}
W "exe still present=$(Test-Path $exe)"

$dets = Get-MpThreatDetection -ErrorAction SilentlyContinue | Select-Object -First 5
if ($dets) { $dets | ForEach-Object { W "  defender: $($_.Resources) ThreatID=$($_.ThreatID)" } }
else { W 'defender: no detections reported' }

W 'cleanup: ks-test shutdown'
$shutdown = Start-Process -FilePath 'D:\kernel-script\target\release\ks-test.exe' -ArgumentList 'shutdown' -WorkingDirectory 'D:\kernel-script' -PassThru -Wait
W "shutdown exit=$($shutdown.ExitCode)"
W "registry key present after cleanup=$(Test-Path 'HKLM:\SOFTWARE\KernelScript')"
W '=== gui smoke done ==='
