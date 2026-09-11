param(
  [switch]$Background
)

$ErrorActionPreference = "Stop"

$srcTap = Join-Path $PSScriptRoot "tap.mjs"
$logDir = Join-Path $env:TEMP "zcode-host-tap"
$tap = Join-Path $logDir "tap.mjs"
$zcode = Join-Path $env:LOCALAPPDATA "Programs\ZCode\ZCode.exe"
$node = (Get-Command node).Source

if (-not (Test-Path $zcode)) { throw "ZCode.exe not found: $zcode" }
if (-not (Test-Path $srcTap)) { throw "tap.mjs not found: $srcTap" }

New-Item -ItemType Directory -Force -Path $logDir | Out-Null
Copy-Item -Force $srcTap $tap
Get-ChildItem $logDir -Filter "tap-*.jsonl" -ErrorAction SilentlyContinue | Remove-Item -Force
Get-ChildItem $logDir -Filter "tap-*.listen.json" -ErrorAction SilentlyContinue | Remove-Item -Force
Remove-Item (Join-Path $logDir "inject-queue.jsonl") -ErrorAction SilentlyContinue

$escapedTap = $tap.Replace("\", "\\")
$env:ZCODE_AGENT_SERVER_COMMAND = $node
$env:ZCODE_AGENT_SERVER_ARGS_JSON = '["' + $escapedTap + '"]'
$env:ZCODE_TAP_LOG_DIR = $logDir
$env:ZCODE_TAP_INNER_EXE = $zcode
$env:ZCODE_TAP_INNER_CJS = Join-Path $env:LOCALAPPDATA "Programs\ZCode\resources\glm\zcode.cjs"

Write-Host "ZCODE_AGENT_SERVER_COMMAND=$env:ZCODE_AGENT_SERVER_COMMAND"
Write-Host "ZCODE_AGENT_SERVER_ARGS_JSON=$env:ZCODE_AGENT_SERVER_ARGS_JSON"
Write-Host "ZCODE_TAP_LOG_DIR=$env:ZCODE_TAP_LOG_DIR"

$running = Get-Process ZCode -ErrorAction SilentlyContinue
if ($running) {
  Write-Host "Stopping $($running.Count) ZCode process(es) so the new env is inherited..."
  $running | Stop-Process -Force
  Start-Sleep -Seconds 2
  $left = Get-Process ZCode -ErrorAction SilentlyContinue
  if ($left) {
    $left | Stop-Process -Force
    Start-Sleep -Seconds 1
  }
}

Write-Host "Starting ZCode Desktop with stdio tap..."
if ($Background) {
  $desktopProcess = Start-Process -FilePath $zcode -ArgumentList "--disable-notifications" -WindowStyle Hidden -PassThru
  Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class VellumZcodeWindow {
  [DllImport("user32.dll")]
  public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
}
'@
  $hideDeadline = (Get-Date).AddSeconds(15)
  do {
    Start-Sleep -Milliseconds 250
    $desktopProcess.Refresh()
    if ($desktopProcess.MainWindowHandle -ne 0) {
      [VellumZcodeWindow]::ShowWindowAsync($desktopProcess.MainWindowHandle, 0) | Out-Null
      break
    }
  } while ((Get-Date) -lt $hideDeadline -and -not $desktopProcess.HasExited)
  Write-Host "Background companion mode requested. ZCode still runs because Desktop owns Start Plan auth/CAPTCHA."
} else {
  Start-Process -FilePath $zcode
}
$listenDeadline = (Get-Date).AddSeconds(20)
do {
  $listener = Get-ChildItem $logDir -Filter "tap-*.listen.json" -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
  if ($listener) { break }
  Start-Sleep -Milliseconds 250
} while ((Get-Date) -lt $listenDeadline)
if (-not $listener) {
  throw "ZCode launched but the tap listener did not become ready within 20 seconds. Inspect $logDir"
}
Write-Host "ZCode tap ready: $($listener.FullName)"
Write-Host "Inject: node scripts/zcode-desktop-host-tap/inject-send.mjs --content `"Reply with exactly PONG`""
