$ErrorActionPreference = 'Continue'
$log = 'D:\kernel-script\kdu_smoke.log'
Remove-Item $log -ErrorAction SilentlyContinue

function W($m) {
    $line = ("{0} {1}`r`n" -f (Get-Date -Format 'HH:mm:ss.fff'), $m)
    $b = [Text.Encoding]::UTF8.GetBytes($line)
    $fs = New-Object IO.FileStream($log, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::Read, 4096, [IO.FileOptions]::WriteThrough)
    $fs.Write($b, 0, $b.Length)
    $fs.Close()
}

Remove-Item 'D:\kernel-script\ks-test-run.log' -ErrorAction SilentlyContinue
W '=== ks-test minimal (KDU mode) ==='
$before = Get-Date
'' | & 'D:\kernel-script\target\release\ks-test.exe' minimal 2>&1 | ForEach-Object { W "ks-test: $_" }
W "minimal exited code=$LASTEXITCODE (elapsed $([int]((Get-Date) - $before).TotalSeconds)s)"
W '=== smoke done ==='
