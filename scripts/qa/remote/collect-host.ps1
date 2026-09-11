# Collect machine-verifiable JSON from a live vellum-dev-host. Never invent bodies.
param(
    [Parameter(Mandatory = $true)]
    [string] $OutDir,
    [string] $Alias = "vellum-dev-host"
)

$ErrorActionPreference = "Continue"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

function Invoke-Ssh([string] $Remote) {
    $out = ssh -o BatchMode=yes -T $Alias $Remote 2>&1
    return [pscustomobject]@{
        text = ($out | Out-String)
        code = $LASTEXITCODE
    }
}

$agentCmd = 'if [ -x "$HOME/.local/bin/vellum-remote-agent" ]; then "$HOME/.local/bin/vellum-remote-agent" status; else vellum-remote-agent status; fi'
$agent = Invoke-Ssh $agentCmd
Set-Content -LiteralPath (Join-Path $OutDir "agent-status.raw.txt") -Value $agent.text -Encoding utf8
$agentJson = $null
try { $agentJson = $agent.text.Trim() | ConvertFrom-Json } catch { $agentJson = $null }
if ($agent.code -eq 0 -and $null -ne $agentJson) {
    $agent.text.Trim() | Set-Content -LiteralPath (Join-Path $OutDir "agent-status.json") -Encoding utf8
}
$agentStatus = if ($agentJson -and $agentJson.type -eq "ok") { $agentJson.result } else { $null }

# The boundary secret never leaves the host: the remote shell reads it only to
# authenticate curl, and stdout contains solely the redacted readyz response.
$readyCmd = 'key="$(cat "$HOME/.local/share/vellum-remote/secrets/__vellum_proxy_boundary__")"; curl -fsS -H "x-vellum-boundary-key: $key" http://127.0.0.1:15721/readyz'
$ready = Invoke-Ssh $readyCmd
Set-Content -LiteralPath (Join-Path $OutDir "readyz.raw.txt") -Value $ready.text -Encoding utf8
$readyJson = $null
try { $readyJson = $ready.text.Trim() | ConvertFrom-Json } catch { $readyJson = $null }
if ($ready.code -eq 0 -and $null -ne $readyJson) {
    $ready.text.Trim() | Set-Content -LiteralPath (Join-Path $OutDir "readyz.json") -Encoding utf8
}

$health = [ordered]@{
    collectedAt = (Get-Date).ToUniversalTime().ToString("o")
    alias = $Alias
    agentExit = $agent.code
    readyzExit = $ready.code
    agentPresent = Test-Path -LiteralPath (Join-Path $OutDir "agent-status.json")
    readyzPresent = Test-Path -LiteralPath (Join-Path $OutDir "readyz.json")
    ready = $null -ne $readyJson -and $readyJson.ready -eq $true
    daemonOwner = if ($agentStatus) { $agentStatus.nativeCodex.daemonOwner } else { $null }
    daemonRunning = if ($agentStatus) { $agentStatus.nativeCodex.daemonRunning } else { $false }
    sessionAuthority = if ($agentStatus) { $agentStatus.nativeCodex.sessionAuthority } else { $null }
}
($health | ConvertTo-Json -Depth 6) | Set-Content -LiteralPath (Join-Path $OutDir "health.json") -Encoding utf8

$config = [ordered]@{
    collectedAt = $health.collectedAt
    agentPresent = $health.agentPresent
}
if ($agentStatus) {
    $config.agent = $agentStatus
}
($config | ConvertTo-Json -Depth 8) | Set-Content -LiteralPath (Join-Path $OutDir "config-hash.json") -Encoding utf8

Write-Output (($health | ConvertTo-Json -Compress))

if (-not $health.agentPresent -or -not $health.readyzPresent -or -not $health.ready -or
    $health.daemonOwner -ne "codexCliDaemon" -or -not $health.daemonRunning -or
    $health.sessionAuthority -ne "codexNativeDaemon") {
    Write-Error "remote host did not satisfy ready/native-daemon invariants"
    exit 1
}
