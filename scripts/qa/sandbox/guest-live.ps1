# Live acceptance is fail-closed until the sandbox can bind every expanded
# matrix row to a pinned Vellum Enhanced model and export native session events.
# Do not call an arbitrary codex executable or spend provider tokens here.
[CmdletBinding()]
param(
    [string]$DriversPath = "C:\SandboxKit\drivers.json",
    [string]$OutFile = "C:\QaOutput\guest-live.json"
)

$ErrorActionPreference = "Stop"
$cases = @()

if (-not (Test-Path -LiteralPath $DriversPath)) {
    @{
        error = "drivers.json missing"
        cases = @()
    } | ConvertTo-Json | ForEach-Object { [System.IO.File]::WriteAllText($OutFile, $_) }
    exit 1
}

$drivers = Get-Content -LiteralPath $DriversPath -Raw -Encoding UTF8 | ConvertFrom-Json
foreach ($prop in $drivers.PSObject.Properties) {
    if ("$($prop.Value.action.kind)" -ne "live-session") { continue }
    $cases += [ordered]@{
        id = $prop.Name
        runner = $null
        verdict = "NOT_RUN"
        reason = "case-driver-unavailable"
        detail = "sandbox live adapter is not implemented: it must select the pinned model, enforce the round budget before launch, and export native Enhanced session/subagent/review events"
        assertionOk = $false
        sessionId = $null
        fileChanged = $false
        isolatedCodexHome = "C:\IsolatedHome\.codex"
    }
}

@{
    cases = $cases
    runners = @()
    note = "No live provider call was made. Environment flags and arbitrary Codex CLI output cannot satisfy Enhanced acceptance."
    at = (Get-Date).ToUniversalTime().ToString("o")
} | ConvertTo-Json -Depth 6 | ForEach-Object { [System.IO.File]::WriteAllText($OutFile, $_) }
