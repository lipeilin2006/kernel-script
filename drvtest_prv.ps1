param([string]$Prv = '', [switch]$Sc, [string]$Victim = '')
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

if ($Prv) { $env:KS_SDK_KDU_PRV = $Prv; W "provider override: $Prv" }
if ($Sc) { $env:KS_SDK_MAP = '1'; W "KS_SDK_MAP: 1 (manual mapping only)" }
if ($Victim) { $env:KS_SDK_VICTIM = $Victim; W "victim override: $Victim" }

$before = Get-Date
W '--- launching ks-test.exe full ---'
'' | & 'D:\kernel-script\target\release\ks-test.exe' full 2>&1 | ForEach-Object { W "full: $_" }
W "full exited code=$LASTEXITCODE (elapsed $([int]((Get-Date) - $before).TotalSeconds)s)"
W '=== done ==='
