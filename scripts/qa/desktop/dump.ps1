# Read Vellum UI/process/diagnostic state. Does not take a click target.
# Writes JSON to stdout (and optional -OutFile). Missing fields stay null.
param(
    [string] $ProcessName = "Vellum",
    [string] $OutFile
)

$ErrorActionPreference = "Continue"
Add-Type -AssemblyName UIAutomationClient -ErrorAction SilentlyContinue | Out-Null

$dump = [ordered]@{
    processes = @()
    proxyRunning = $false
    activeTab = $null
    refreshedAt = $null
    uiState = $null
    uiValue = $null
    backendValue = $null
    fileChanged = $false
    unit = $null
    usageSource = $null
    attestation = $null
    digest = $null
    backendSources = @()
    window = $null
}

$dump.processes = @(Get-Process -ErrorAction SilentlyContinue | ForEach-Object { $_.ProcessName })
$dump.proxyRunning = [bool](Get-Process "vellum-proxy-desktop" -ErrorAction SilentlyContinue)

$procs = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero })
if ($procs) {
    $window = $null
    foreach ($proc in $procs) {
        $candidate = [System.Windows.Automation.AutomationElement]::FromHandle($proc.MainWindowHandle)
        if ($candidate) { $window = $candidate; break }
    }
    if ($window) {
        $dump.window = $window.Current.Name
        $tabType = [System.Windows.Automation.ControlType]::TabItem
        $typeCond = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty, $tabType)
        $tabs = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $typeCond)
        for ($i = 0; $i -lt $tabs.Count; $i++) {
            $tab = $tabs.Item($i)
            try {
                $sel = $tab.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
                if ($sel.Current.IsSelected) {
                    $dump.activeTab = $tab.Current.Name
                    $dump.uiState = $tab.Current.Name
                    break
                }
            } catch {}
        }
        foreach ($autoId in @("refreshedAt", "status-refreshed-at", "last-refresh", "statusbar-time")) {
            $idCond = New-Object System.Windows.Automation.PropertyCondition(
                [System.Windows.Automation.AutomationElement]::AutomationIdProperty, $autoId)
            $el = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $idCond)
            if ($el -and $el.Current.Name) {
                $dump.refreshedAt = $el.Current.Name
                break
            }
        }
        foreach ($autoId in @("quota-remaining", "usage-value", "status-quota")) {
            $idCond = New-Object System.Windows.Automation.PropertyCondition(
                [System.Windows.Automation.AutomationElement]::AutomationIdProperty, $autoId)
            $el = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $idCond)
            if ($el -and $el.Current.Name) {
                $dump.uiValue = $el.Current.Name
                break
            }
        }
    }
}

foreach ($root in @("$env:APPDATA\com.vellum.desktop", "$env:LOCALAPPDATA\com.vellum.desktop")) {
    if (-not (Test-Path $root)) { continue }
    $att = Get-ChildItem $root -Recurse -Filter "attestation.json" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($att) {
        try {
            $dump.attestation = Get-Content -LiteralPath $att.FullName -Raw | ConvertFrom-Json
            $dump.digest = $dump.attestation.digest
            $dump.backendSources += "attestation.json"
        } catch {}
    }
    $quota = Get-ChildItem $root -Recurse -Filter "*quota*" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($quota -and -not $dump.backendValue) {
        try {
            $q = Get-Content -LiteralPath $quota.FullName -Raw | ConvertFrom-Json
            if ($q.remaining) { $dump.backendValue = "$($q.remaining)" }
            elseif ($q.quota) { $dump.backendValue = "$($q.quota)" }
            $dump.backendSources += $quota.Name
        } catch {}
    }
}

$json = ($dump | ConvertTo-Json -Compress -Depth 6)
if ($OutFile) {
    $dir = Split-Path -Parent $OutFile
    if ($dir) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    [System.IO.File]::WriteAllText($OutFile, $json)
}
Write-Output $json
