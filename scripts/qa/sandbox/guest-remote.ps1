# Drive Remote Manager buttons, then assert hostState/readyz/daemon ownership
# through shipped assert-after-action.mjs. Docker being up is not a PASS.
[CmdletBinding()]
param(
    [string]$ButtonsPath = "C:\SandboxKit\remote-buttons.json",
    [string]$OutFile = "C:\QaOutput\remote-ui.json"
)

$ErrorActionPreference = "Continue"
$act = "C:\SandboxKit\desktop\act.ps1"
$node = "C:\SandboxKit\node.exe"
$assertJs = "C:\SandboxKit\assert-after-action.mjs"
$buttons = @()
if (Test-Path -LiteralPath $ButtonsPath) {
    $buttons = @(Get-Content -LiteralPath $ButtonsPath -Raw -Encoding UTF8 | ConvertFrom-Json)
}

$hostInfo = $null
if (Test-Path "C:\QaOutput\dedicated-host.json") {
    $hostInfo = Get-Content "C:\QaOutput\dedicated-host.json" -Raw | ConvertFrom-Json
}

function Probe-DedicatedHost {
    $result = [ordered]@{
        reachable = $false
        readyz = $false
        daemonOwner = $null
        sessionAuthority = $null
        hostState = "unreachable"
        raw = $null
    }
    if (-not $hostInfo) { return $result }
    $key = "C:\QaSecrets\remote-id_ed25519"
    if (-not (Test-Path $key)) { $result.raw = "ssh key not mapped"; return $result }
    $target = "$($hostInfo.connectHost)"
    $port = [int]$hostInfo.port
    $probe = & ssh -i $key -p $port -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=NUL vellum@$target "echo reachable" 2>&1
    if ($LASTEXITCODE -ne 0) { $result.raw = "$probe"; return $result }
    $result.reachable = $true
    $result.hostState = "reachable"
    $readyCmd = 'key="$(cat "$HOME/.local/share/vellum-remote/secrets/__vellum_proxy_boundary__" 2>/dev/null)"; if [ -n "$key" ]; then curl -fsS -H "x-vellum-boundary-key: $key" http://127.0.0.1:15721/readyz; fi'
    $ready = & ssh -i $key -p $port -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=NUL vellum@$target $readyCmd 2>&1
    $result.raw = "$ready"
    try {
        $readyJson = "$ready".Trim() | ConvertFrom-Json
        if ($readyJson.ready -eq $true) { $result.readyz = $true; $result.hostState = "ready" }
    } catch {}
    $agentCmd = 'if [ -x "$HOME/.local/bin/vellum-remote-agent" ]; then "$HOME/.local/bin/vellum-remote-agent" status; fi'
    $agent = & ssh -i $key -p $port -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=NUL vellum@$target $agentCmd 2>&1
    try {
        $agentJson = "$agent".Trim() | ConvertFrom-Json
        $st = $agentJson.result
        if ($st.nativeCodex.daemonOwner) { $result.daemonOwner = "$($st.nativeCodex.daemonOwner)" }
        if ($st.nativeCodex.sessionAuthority) { $result.sessionAuthority = "$($st.nativeCodex.sessionAuthority)" }
        if ($result.daemonOwner -eq "codexCliDaemon") { $result.hostState = "native-owned" }
    } catch {}
    return $result
}

function Invoke-ShippedAssert($before, $after, $verify) {
    $dir = Join-Path $env:TEMP "qa-assert-remote"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $dir "before.json"), ($before | ConvertTo-Json -Compress -Depth 8))
    [System.IO.File]::WriteAllText((Join-Path $dir "after.json"), ($after | ConvertTo-Json -Compress -Depth 8))
    [System.IO.File]::WriteAllText((Join-Path $dir "verify.json"), ($verify | ConvertTo-Json -Compress -Depth 8))
    $json = & $node $assertJs (Join-Path $dir "before.json") (Join-Path $dir "after.json") (Join-Path $dir "verify.json")
    try { return $json | ConvertFrom-Json } catch {
        return @{ ok = $false; verdict = "FAIL"; reason = "outcome-not-verified"; detail = "assert CLI did not return JSON" }
    }
}

$vellum = Get-Process -Name "Vellum" -ErrorAction SilentlyContinue
$beforeProbe = Probe-DedicatedHost
$rows = @()
foreach ($button in $buttons) {
    $clicked = $false
    $stderr = $null
    if ($vellum) {
        $p = Start-Process -FilePath "powershell.exe" -ArgumentList @(
            "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $act,
            "-Name", "$($button.control)",
            "-ProcessName", "Vellum",
            "-ControlType", "Button"
        ) -Wait -PassThru -NoNewWindow -RedirectStandardOutput "$env:TEMP\remote-act-out.txt" -RedirectStandardError "$env:TEMP\remote-act-err.txt"
        $clicked = $p.ExitCode -eq 0
        $stderr = Get-Content "$env:TEMP\remote-act-err.txt" -Raw -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
    $afterProbe = Probe-DedicatedHost
    $before = @{ hostState = $beforeProbe.hostState }
    $after = @{
        clicked = $clicked
        vellumRunning = [bool]$vellum
        hostState = $afterProbe.hostState
        hostStateChanged = $afterProbe.hostState -ne $beforeProbe.hostState
        readyz = [bool]$afterProbe.readyz
        daemonOwner = $afterProbe.daemonOwner
        sessionAuthority = $afterProbe.sessionAuthority
        detail = $stderr
    }
    $verify = @{
        kind = "host-state"
        requireReadyz = $button.id -match "health|clean-host|bootstrap-confirm|apply|repair|restart-native"
        requireDaemonOwner = if ($button.id -match "restart-native|health|clean-host") { "codexCliDaemon" } else { $null }
    }
    $assert = Invoke-ShippedAssert $before $after $verify
    $rows += [ordered]@{
        id = $button.id
        control = $button.control
        clicked = $clicked
        assertionOk = [bool]$assert.ok
        verdict = $assert.verdict
        reason = $assert.reason
        detail = $assert.detail
        hostStateBefore = $beforeProbe.hostState
        hostStateAfter = $afterProbe.hostState
        readyz = $afterProbe.readyz
        daemonOwner = $afterProbe.daemonOwner
        probeRaw = $afterProbe.raw
    }
    $beforeProbe = $afterProbe
}

@{
    buttons = $rows
    dedicatedHost = @{
        container = $hostInfo.container
        connectHost = $hostInfo.connectHost
        port = $hostInfo.port
    }
    note = "backend docker success is not a substitute for these button rows"
    at = (Get-Date).ToUniversalTime().ToString("o")
} | ConvertTo-Json -Depth 6 | ForEach-Object { [System.IO.File]::WriteAllText($OutFile, $_) }
