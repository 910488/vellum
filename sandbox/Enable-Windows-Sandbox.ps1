#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [switch]$Restart
)

$ErrorActionPreference = "Stop"
$feature = Get-WindowsOptionalFeature -Online -FeatureName Containers-DisposableClientVM

if ($feature.State -eq "Enabled") {
    Write-Host "Windows Sandbox is already enabled." -ForegroundColor Green
    exit 0
}

Write-Host "Enabling Windows Sandbox..." -ForegroundColor Cyan
$result = Enable-WindowsOptionalFeature `
    -Online `
    -FeatureName Containers-DisposableClientVM `
    -All `
    -NoRestart

Write-Host "Windows Sandbox feature state: $($result.State)" -ForegroundColor Green
if ($result.RestartNeeded) {
    if ($Restart) {
        Restart-Computer
    }

    Write-Warning "A Windows restart is required. Save your work, restart Windows, then run Launch-Sandbox.ps1."
}
