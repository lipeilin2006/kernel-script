'' | & 'D:\kernel-script\target\release\ks-test.exe' shutdown *>&1 | Out-File 'D:\kernel-script\drvtest7.log' -Encoding utf8
'' | & 'D:\kernel-script\target\release\ks-test.exe' load *>&1 | Out-File 'D:\kernel-script\drvtest7.log' -Encoding utf8 -Append
