param([string]$Exe, [string]$OutDir)
$bytes = [IO.File]::ReadAllBytes($Exe)
$len = $bytes.Length
for ($i = 0; $i -lt $len - 0x200; $i++) {
    if ($bytes[$i] -eq 0x4D -and $bytes[$i+1] -eq 0x5A) {
        $lfanew = [BitConverter]::ToInt32($bytes, $i + 0x3C)
        if ($lfanew -le 0 -or ($i + $lfanew + 4) -ge $len) { continue }
        if ($bytes[$i+$lfanew] -ne 0x50 -or $bytes[$i+$lfanew+1] -ne 0x45 -or $bytes[$i+$lfanew+2] -ne 0 -or $bytes[$i+$lfanew+3] -ne 0) { continue }
        $machine = [BitConverter]::ToUInt16($bytes, $i + $lfanew + 4)
        $numSec = [BitConverter]::ToUInt16($bytes, $i + $lfanew + 6)
        $optSize = [BitConverter]::ToUInt16($bytes, $i + $lfanew + 20)
        if ($machine -ne 0x8664 -or $numSec -eq 0 -or $numSec -gt 16) { continue }
        $optBase = $i + $lfanew + 24
        $magic = [BitConverter]::ToUInt16($bytes, $optBase)
        if ($magic -ne 0x020B) { continue }
        # DataDirectory: optional header + 112 for PE32+; entry 4 = certificate
        $certRva = [BitConverter]::ToUInt32($bytes, $optBase + 112 + 4 * 8)
        $certSize = [BitConverter]::ToUInt32($bytes, $optBase + 112 + 4 * 8 + 4)
        $names = @()
        $maxEnd = 0
        for ($s = 0; $s -lt $numSec; $s++) {
            $sh = $optBase + $optSize + $s * 40
            if ($sh + 40 -gt $len) { $names = @(); break }
            $name = [Text.Encoding]::ASCII.GetString($bytes, $sh, 8).TrimEnd([char]0)
            $names += $name
            $rawPtr = [BitConverter]::ToUInt32($bytes, $sh + 20)
            $rawSize = [BitConverter]::ToUInt32($bytes, $sh + 16)
            if (($rawPtr + $rawSize) -gt $maxEnd) { $maxEnd = $rawPtr + $rawSize }
        }
        if ($names -notcontains 'PAGE') { continue }
        if ($certRva -gt 0 -and ($certRva + $certSize) -gt $maxEnd) { $maxEnd = $certRva + $certSize }
        $carveLen = [Math]::Min($maxEnd, $len - $i)
        $out = Join-Path $OutDir ("procexp_sys_" + $i + ".sys")
        $slice = New-Object byte[] $carveLen
        [Array]::Copy($bytes, $i, $slice, 0, $carveLen)
        [IO.File]::WriteAllBytes($out, $slice)
        "carved: offset=$i len=$carveLen sections=$($names -join ',') cert=$certRva+$certSize -> $out"
    }
}