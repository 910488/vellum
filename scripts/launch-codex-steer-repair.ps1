param(
    [string]$RepairDirectory = (Join-Path $env:LOCALAPPDATA 'vellum/codex-steer-repair/26.928.2636-native'),
    [string]$DataRoot = (Join-Path $env:LOCALAPPDATA 'vellum'),
    [switch]$CheckOnly
)
$ErrorActionPreference = 'Stop'
$repair = (Resolve-Path -LiteralPath $RepairDirectory).Path
$metadata = Get-Content -LiteralPath (Join-Path $repair 'vellum-steer-repair.json') -Raw | ConvertFrom-Json
if ($metadata.scope -ne 'composer-quota-disable-only') { throw 'Unsupported repair manifest.' }
& node (Join-Path $PSScriptRoot 'codex-steer-repair.mjs') --verify $repair
if ($LASTEXITCODE -ne 0) { throw 'Repair archive verification failed.' }
$lease = Get-Content -LiteralPath (Join-Path $DataRoot 'enhanced-runtime/env-lease.json') -Raw | ConvertFrom-Json
$bridge = $lease.appliedValue
if (-not (Test-Path -LiteralPath $bridge -PathType Leaf) -or
    [IO.Path]::GetFileNameWithoutExtension($bridge) -notlike 'vellum-codex-app-server*') {
    throw 'Enable the Vellum proxy and its Desktop bridge first.'
}
$pool = Get-Content -LiteralPath (Join-Path $DataRoot 'codex_oauth_accounts.json') -Raw | ConvertFrom-Json
if (-not $pool.quota_pool.enabled) { throw 'Enable the Vellum quota pool first.' }
$launch = Get-Content -LiteralPath (Join-Path $DataRoot 'enhanced-runtime/launch-manifest.json') -Raw | ConvertFrom-Json
if ($launch.launchId -ne $lease.launchId -or
    [Environment]::GetEnvironmentVariable('CODEX_CLI_PATH', 'User') -ne $bridge) {
    throw 'Enable the Vellum proxy again to prepare a current Desktop launch.'
}
$executable = Join-Path $repair 'ChatGPT.exe'
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw 'Repair copy has no Desktop executable.' }
if ($CheckOnly) { Write-Output 'Repair prerequisites are satisfied.'; return }
if (Get-Process -Name ChatGPT -ErrorAction SilentlyContinue) {
    throw 'Close Codex Desktop after its running tasks finish, then run this launcher again. No process was stopped.'
}
$previousBridge = $env:CODEX_CLI_PATH
try {
    $env:CODEX_CLI_PATH = $bridge
    Start-Process -FilePath $executable -WorkingDirectory $repair -WindowStyle Hidden
} finally { $env:CODEX_CLI_PATH = $previousBridge }
