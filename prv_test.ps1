param([string]$Prv)
$ErrorActionPreference = 'Continue'
$env:KS_SDK_KDU_PRV = $Prv
$log = 'D:\kernel-script\prv_test.log'
Remove-Item $log -ErrorAction SilentlyContinue
$out = & 'D:\kernel-script\target\release\ks-test.exe' 2>&1
$out | ForEach-Object { "$_" } | Set-Content -Path $log -Encoding UTF8
"exit=$LASTEXITCODE" | Add-Content -Path $log
