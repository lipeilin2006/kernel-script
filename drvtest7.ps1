$ErrorActionPreference = 'Continue'
$log = 'D:\kernel-script\drvtest7.log'
Remove-Item $log -ErrorAction SilentlyContinue

function W($m) {
    $line = ("{0} {1}`r`n" -f (Get-Date -Format 'HH:mm:ss.fff'), $m)
    $b = [Text.Encoding]::UTF8.GetBytes($line)
    $fs = New-Object IO.FileStream($log, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::Read, 4096, [IO.FileOptions]::WriteThrough)
    $fs.Write($b, 0, $b.Length)
    $fs.Close()
}

W '=== v7: full suite (batch/mdl/chain, deepest call chains) on release driver ==='

Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Services' -ErrorAction SilentlyContinue |
    Where-Object { $_.PSChildName -like 'kstdrv*' } | ForEach-Object {
        & sc.exe delete $_.PSChildName 2>&1 | ForEach-Object { W "sc delete => $_" }
    }
Remove-Item 'D:\kernel-script\ks-test-run.log', 'C:\ks-test-run.log', 'D:\kernel-script\target\release\ks-test-run.log', 'D:\kernel-script\ks-link-stage.log' -ErrorAction SilentlyContinue
W 'stale logs cleared'

$before = Get-Date
W '--- launching ks-test.exe full ---'
'' | & 'D:\kernel-script\target\release\ks-test.exe' full 2>&1 | ForEach-Object { W "full: $_" }
W "full exited code=$LASTEXITCODE (elapsed $([int]((Get-Date) - $before).TotalSeconds)s)"

$svc = @(Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Services' -ErrorAction SilentlyContinue | Where-Object { $_.PSChildName -like 'kstdrv*' })
W "post: kstdrv services remaining = $($svc.Count)"
W '=== v7 done ==='
