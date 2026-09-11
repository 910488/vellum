[CmdletBinding()]
param(
    [switch]$Build
)

$ErrorActionPreference = "Stop"
$sandboxExe = Join-Path $env:WINDIR "System32\WindowsSandbox.exe"
$configurationTemplate = Join-Path $PSScriptRoot "Vellum-Onboarding.wsb"
$repoRoot = Split-Path -Parent $PSScriptRoot
$installerDirectory = Join-Path $repoRoot "src-tauri\target\release\bundle\nsis"

if (-not (Test-Path $sandboxExe)) {
    throw "Windows Sandbox is not enabled. Double-click Enable-Windows-Sandbox.cmd, approve the administrator prompt, then restart Windows."
}

if ($Build) {
    Write-Host "Building the Vellum installer (pnpm tauri build)..." -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        & pnpm tauri build
        if ($LASTEXITCODE -ne 0) {
            throw "pnpm tauri build failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

$installer = Get-ChildItem -LiteralPath $installerDirectory -Filter "*-setup.exe" -File `
    -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1

if (-not $installer) {
    throw "No Vellum installer was found in: $installerDirectory`nRun this script with -Build, or run 'pnpm tauri build' first."
}

if (-not (Test-Path $configurationTemplate)) {
    throw "Sandbox configuration is missing: $configurationTemplate"
}

$exchangeDirectory = Join-Path $PSScriptRoot "exchange"
New-Item -ItemType Directory -Force -Path $exchangeDirectory | Out-Null

$configuration = Join-Path $env:TEMP "Vellum-Onboarding.generated.wsb"
$xml = Get-Content -LiteralPath $configurationTemplate -Raw
$xml = $xml.Replace("__VELLUM_SANDBOX_ROOT__", $PSScriptRoot)
$xml = $xml.Replace("__VELLUM_INSTALLER_ROOT__", $installerDirectory)
$xml = $xml.Replace("__VELLUM_EXCHANGE_ROOT__", $exchangeDirectory)
Set-Content -LiteralPath $configuration -Value $xml -Encoding utf8

$age = [int]((Get-Date) - $installer.LastWriteTime).TotalMinutes
Write-Host "Starting the Vellum onboarding sandbox with $($installer.Name) (built $age minute(s) ago)..." -ForegroundColor Cyan
Start-Process -FilePath $sandboxExe -ArgumentList ('"{0}"' -f $configuration)
