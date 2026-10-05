# Restore crash-dump settings to the configuration that produced working
# minidumps on 2026-09-19/20/26: kernel dump in the boot pagefile plus a
# minidump in C:\Windows\Minidump. The DedicatedDumpFile/DumpFileSize pair
# (added 2026-10-03, file never created) coincides with every dump failing
# with volmgr 161 BugCheckProgress 0x00020042.
$ErrorActionPreference = 'Stop'
$log = 'D:\kernel-script\dumpfix.log'
function W($m) { $line = "$(Get-Date -Format 'HH:mm:ss.fff') $m"; Add-Content -LiteralPath $log -Value $line -Encoding UTF8 }
Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue
W '=== dumpfix start ==='

$cc = 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl'
foreach ($name in 'DedicatedDumpFile', 'DumpFileSize') {
    try {
        Remove-ItemProperty -Path $cc -Name $name -ErrorAction Stop
        W "removed $name"
    } catch { W "$name not present ($($_.Exception.Message))" }
}
Set-ItemProperty -Path $cc -Name CrashDumpEnabled -Value 2 -Type DWord
W 'CrashDumpEnabled = 2 (kernel dump)'

$cur = Get-ItemProperty -Path $cc
W ("now: CrashDumpEnabled={0} DedicatedDumpFile={1} DumpFileSize={2} AutoReboot={3}" -f `
    $cur.CrashDumpEnabled, $cur.DedicatedDumpFile, $cur.DumpFileSize, $cur.AutoReboot)
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
W "RAM = ${ram} GB; AutomaticManagedPagefile = $((Get-CimInstance Win32_ComputerSystem).AutomaticManagedPagefile)"
W '=== dumpfix done; REBOOT REQUIRED ==='
