[CmdletBinding()]
param(
    [string]$DriverPath = (Join-Path $PSScriptRoot '..\..\ks-driver.sys'),
    [string]$InfPath = (Join-Path $PSScriptRoot '..\ks_driver.inf'),
    [string]$ServiceName = 'ks-driver',
    [string]$CertificateThumbprint,
    [string]$CatalogPath,
    [switch]$SkipSignatureCheck,
    [switch]$KeepService
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell session.'
}

$driverPath = (Resolve-Path -LiteralPath $DriverPath).Path
$infPath = (Resolve-Path -LiteralPath $InfPath).Path
$packageDriverPath = Join-Path (Split-Path -Parent $infPath) 'ks-driver.sys'
if (([IO.Path]::GetFullPath($driverPath) -ne [IO.Path]::GetFullPath($packageDriverPath)) -and -not (Test-Path -LiteralPath $packageDriverPath)) {
    throw "pnputil requires ks-driver.sys next to the INF. Copy '$driverPath' to '$packageDriverPath' or pass matching package paths."
}

function Invoke-NativeChecked([string]$File, [string[]]$Arguments) {
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$File failed with exit code 0x{0:X8}" -f ([uint32]$LASTEXITCODE)
    }
}

if (-not $SkipSignatureCheck) {
    $signtool = (Get-Command signtool.exe -ErrorAction Stop).Source
    if ($CertificateThumbprint) {
        Invoke-NativeChecked $signtool @('sign', '/sha1', $CertificateThumbprint, '/fd', 'sha256', $driverPath)
        if ($CatalogPath) {
            $catalog = (Resolve-Path -LiteralPath $CatalogPath).Path
            Invoke-NativeChecked $signtool @('sign', '/sha1', $CertificateThumbprint, '/fd', 'sha256', $catalog)
        }
    }
    Invoke-NativeChecked $signtool @('verify', '/kp', '/v', $driverPath)
}

$serviceWasPresent = $null -ne (Get-CimInstance Win32_Service -Filter "Name='$ServiceName'")
$installed = $false
try {
    Invoke-NativeChecked pnputil.exe @('/add-driver', $infPath, '/install')
    $service = Get-CimInstance Win32_Service -Filter "Name='$ServiceName'"
    if (-not $service) { throw "INF installation did not create service '$ServiceName'." }
    $installed = $true

    sc.exe start $ServiceName | Out-Host
    $startCode = $LASTEXITCODE
    if ($startCode -ne 0) {
        sc.exe queryex $ServiceName | Out-Host
        $service = Get-CimInstance Win32_Service -Filter "Name='$ServiceName'"
        $nativeStatus = if ($service) { '{0:X8}' -f ([uint32]$service.ExitCode) } else { 'unknown' }
        throw "driver start failed: Win32 exit 0x{0:X8}, service status 0x$nativeStatus" -f ([uint32]$startCode)
    }
    Write-Host "Driver started successfully. Service: $ServiceName"
}
finally {
    if ($installed -and -not $KeepService -and -not $serviceWasPresent) {
        sc.exe stop $ServiceName | Out-Host
        Start-Sleep -Milliseconds 500
        sc.exe delete $ServiceName | Out-Host
    }
}
