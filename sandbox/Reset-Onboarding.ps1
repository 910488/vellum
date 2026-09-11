[CmdletBinding()]
param(
    # Also wipe the isolated Codex home, forcing a fresh Codex sign-in.
    # Off by default because re-authenticating is the slow part of a rerun.
    [switch]$IncludeCodexHome
)

$ErrorActionPreference = "Stop"

# This deletes real state roots, so refuse to run anywhere but the disposable VM.
if ($env:USERNAME -ne "WDAGUtilityAccount" -and -not (Test-Path "C:\SandboxKit")) {
    throw "Reset-Onboarding.ps1 only runs inside Windows Sandbox. It deletes Vellum state and would wipe host data."
}

function Remove-StateRoot {
    param([string]$Path, [string]$Label)

    if (-not (Test-Path -LiteralPath $Path)) {
        Write-Host "already clean: $Label" -ForegroundColor DarkGray
        return
    }

    Remove-Item -LiteralPath $Path -Recurse -Force
    Write-Host "removed $Label ($Path)" -ForegroundColor Green
}

Get-Process -Name "Vellum" -ErrorAction SilentlyContinue | ForEach-Object {
    $_ | Stop-Process -Force
    Write-Host "stopped running Vellum (pid $($_.Id))" -ForegroundColor Cyan
}
Start-Sleep -Seconds 1

# The "onboarding completed" flag lives in localStorage, which Tauri v2 keeps in
# the WebView2 profile under the bundle identifier — not in the app data dir.
Remove-StateRoot -Path (Join-Path $env:LOCALAPPDATA "com.vellum.desktop") -Label "WebView2 profile (localStorage)"
Remove-StateRoot -Path (Join-Path $env:LOCALAPPDATA "vellum") -Label "Vellum app data"

if ($IncludeCodexHome) {
    $codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { "C:\IsolatedHome\.codex" }
    Remove-StateRoot -Path $codexHome -Label "isolated Codex home"
    New-Item -ItemType Directory -Force -Path $codexHome | Out-Null
}

$vellumPath = "C:\Exchange\vellum-path.txt"
if (Test-Path $vellumPath) {
    $exe = (Get-Content -LiteralPath $vellumPath -Raw).Trim()
    if (Test-Path -LiteralPath $exe) {
        Write-Host "Relaunching Vellum at first run..." -ForegroundColor Cyan
        Start-Process -FilePath $exe
        exit 0
    }
}

Write-Host "State cleared. Launch Vellum from the Start menu to replay onboarding." -ForegroundColor Yellow
