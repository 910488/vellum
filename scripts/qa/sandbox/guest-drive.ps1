# dump -> invoke/observe -> wait -> dump -> shipped assert-after-action.
# UI fields come only from dump.ps1 via build-after-from-dump.mjs.
[CmdletBinding()]
param(
    [string]$DriversPath = "C:\SandboxKit\drivers.json",
    [string]$OutFile = "C:\QaOutput\guest-cases.json"
)

$ErrorActionPreference = "Stop"
$act = "C:\SandboxKit\desktop\act.ps1"
$dumpPs = "C:\SandboxKit\desktop\dump.ps1"
$node = "C:\SandboxKit\node.exe"
$assertJs = "C:\SandboxKit\assert-after-action.mjs"
$afterJs = "C:\SandboxKit\build-after-from-dump.mjs"
$results = @()

function Invoke-Control([string]$Name, [string]$ProcessName, [string]$ControlType, [string]$Shot) {
    $args = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $act,
        "-Name", $Name,
        "-ProcessName", $ProcessName,
        "-ControlType", $ControlType
    )
    if ($Shot) { $args += @("-Screenshot", $Shot) }
    $p = Start-Process -FilePath "powershell.exe" -ArgumentList $args -Wait -PassThru -NoNewWindow -RedirectStandardOutput "$env:TEMP\act-out.txt" -RedirectStandardError "$env:TEMP\act-err.txt"
    return [pscustomobject]@{
        exitCode = $p.ExitCode
        stdout = (Get-Content -LiteralPath "$env:TEMP\act-out.txt" -Raw -ErrorAction SilentlyContinue)
        stderr = (Get-Content -LiteralPath "$env:TEMP\act-err.txt" -Raw -ErrorAction SilentlyContinue)
    }
}

function Wait-Driver($wait) {
    $timeoutMs = 15000
    if ($wait.timeoutMs) { $timeoutMs = [int]$wait.timeoutMs }
    $deadline = [datetime]::UtcNow.AddMilliseconds($timeoutMs)
    do {
        if ($wait.process) {
            if (Get-Process -Name "$($wait.process)" -ErrorAction SilentlyContinue) { return $true }
        } elseif ($wait.processGone) {
            if (-not (Get-Process -Name "$($wait.processGone)" -ErrorAction SilentlyContinue)) { return $true }
        } elseif ($wait.file -and (Test-Path -LiteralPath "$($wait.file)")) {
            return $true
        } elseif ($wait.uiReady) {
            if (Get-Process -Name "Vellum" -ErrorAction SilentlyContinue) { return $true }
        } else {
            return $true
        }
        Start-Sleep -Milliseconds 200
    } while ([datetime]::UtcNow -lt $deadline)
    return $false
}

function Invoke-Dump([string]$Path) {
    if (-not (Test-Path $dumpPs)) { return "{}" }
    $raw = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $dumpPs -OutFile $Path
    if (Test-Path -LiteralPath $Path) {
        return Get-Content -LiteralPath $Path -Raw -Encoding UTF8
    }
    return "$raw"
}

function Convert-DumpToAfter([string]$DumpJson, [string]$Clicked) {
    $dumpFile = Join-Path $env:TEMP "qa-dump-in.json"
    [System.IO.File]::WriteAllText($dumpFile, $DumpJson)
    if (-not (Test-Path $node) -or -not (Test-Path $afterJs)) {
        return @{ processes = @(); activeTab = $null; uiState = $null; refreshedAt = $null }
    }
    $json = & $node $afterJs $dumpFile $Clicked
    try { return $json | ConvertFrom-Json } catch {
        return @{ processes = @(); activeTab = $null; uiState = $null; refreshedAt = $null }
    }
}

function Invoke-ShippedAssert($before, $after, $verify) {
    $dir = Join-Path $env:TEMP "qa-assert"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $dir "before.json"), ($before | ConvertTo-Json -Compress -Depth 8))
    [System.IO.File]::WriteAllText((Join-Path $dir "after.json"), ($after | ConvertTo-Json -Compress -Depth 8))
    [System.IO.File]::WriteAllText((Join-Path $dir "verify.json"), ($verify | ConvertTo-Json -Compress -Depth 8))
    if (-not (Test-Path $node) -or -not (Test-Path $assertJs)) {
        return @{ ok = $false; verdict = "FAIL"; reason = "outcome-not-verified"; detail = "shipped assert-after-action.mjs missing" }
    }
    $json = & $node $assertJs (Join-Path $dir "before.json") (Join-Path $dir "after.json") (Join-Path $dir "verify.json")
    try { return $json | ConvertFrom-Json } catch {
        return @{ ok = $false; verdict = "FAIL"; reason = "outcome-not-verified"; detail = "assert CLI did not return JSON" }
    }
}

function Restore-Driver($restore) {
    if (-not $restore -or $restore -eq "none" -or $restore -eq "destructive-last") { return }
    if ($restore -eq "stop-proxy") {
        Get-Process "vellum-proxy-desktop" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }
}

if (-not (Test-Path -LiteralPath $DriversPath)) {
    @{ error = "drivers.json missing"; cases = @() } | ConvertTo-Json | Set-Content -LiteralPath $OutFile -Encoding utf8
    exit 1
}

$drivers = Get-Content -LiteralPath $DriversPath -Raw -Encoding UTF8 | ConvertFrom-Json
foreach ($prop in $drivers.PSObject.Properties) {
    $id = $prop.Name
    $driver = $prop.Value
    $action = $driver.action
    $kind = "$($action.kind)"
    if ($kind -in @("live-session", "remote-ui")) { continue }
    if (-not (Get-Process -Name "Vellum" -ErrorAction SilentlyContinue)) {
        $results += [ordered]@{
            id = $id
            verdict = "BLOCKED"
            reason = "launcher-unavailable"
            detail = "Vellum process is not running; desktop control was not driven"
            assertionOk = $false
            actionKind = $kind
        }
        continue
    }

    $control = "$($action.control)"
    $processName = if ($action.process) { "$($action.process)" } else { "Vellum" }
    $controlType = if ($action.controlType) { "$($action.controlType)" } else { "Button" }

    $beforeDumpPath = Join-Path $env:TEMP ("dump-before-" + ($id -replace "[^\w.-]", "_") + ".json")
    $beforeJson = Invoke-Dump $beforeDumpPath
    $before = Convert-DumpToAfter $beforeJson $control

    $shot = Join-Path "C:\QaOutput\screenshots" ($id -replace "[^\w.-]", "_")
    New-Item -ItemType Directory -Force -Path (Split-Path $shot) | Out-Null
    $click = @{ exitCode = 0; stderr = "" }
    if ($kind -eq "observe" -or $kind -eq "manual-checkpoint") {
        $null = Wait-Driver $driver.wait
    } else {
        if (-not $control -or $control -eq $id) {
            $results += [ordered]@{
                id = $id
                verdict = "FAIL"
                reason = "control-not-driven"
                detail = "observe/invoke refused to click case id as a button"
                assertionOk = $false
            }
            continue
        }
        $click = Invoke-Control -Name $control -ProcessName $processName -ControlType $controlType -Shot "$shot.png"
        $null = Wait-Driver $driver.wait
    }

    $afterDumpPath = Join-Path $env:TEMP ("dump-after-" + ($id -replace "[^\w.-]", "_") + ".json")
    $afterJson = Invoke-Dump $afterDumpPath
    $after = Convert-DumpToAfter $afterJson $control

    $verify = $driver.verify
    if (-not $verify) { $verify = @{ kind = "outcome-not-verified" } }
    $assert = Invoke-ShippedAssert $before $after $verify
    if ($kind -eq "manual-checkpoint") {
        $assert = Invoke-ShippedAssert $before $after @{ kind = "manual-checkpoint" }
    }
    if ($click.exitCode -ne 0 -and $kind -ne "observe" -and $kind -ne "manual-checkpoint") {
        $assert = @{ ok = $false; verdict = "FAIL"; reason = "control-not-driven"; detail = "failed to invoke $control : $($click.stderr)" }
    }

    Restore-Driver "$($driver.restore)"

    $results += [ordered]@{
        id = $id
        verdict = $assert.verdict
        reason = $assert.reason
        detail = $assert.detail
        assertionOk = [bool]$assert.ok
        control = $control
        process = $processName
        controlType = $controlType
        actionKind = $kind
        clickExit = $click.exitCode
    }
}

@{ cases = $results; at = (Get-Date).ToUniversalTime().ToString("o") } |
    ConvertTo-Json -Depth 6 |
    ForEach-Object { [System.IO.File]::WriteAllText($OutFile, $_) }
