param([string[]]$TestArgs)
$log = 'D:\kernel-script\drvtest7.log'
Remove-Item $log -ErrorAction SilentlyContinue
'' | & 'D:\kernel-script\target\release\ks-test.exe' @TestArgs 2>&1 | ForEach-Object { Add-Content -Path $log -Value $_ -Encoding utf8 }
