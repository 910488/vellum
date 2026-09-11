# Probe whether a Vellum desktop window or packaged binary is available.
# Writes JSON to stdout. Never throws past a structured failure object.

$ErrorActionPreference = "Continue"
$result = [ordered]@{
    ok          = $false
    reason      = "not-found"
    window      = $null
    process     = @()
    binary      = $null
    screenshot  = $null
}

try {
    $procs = @(Get-Process | Where-Object { $_.ProcessName -match '^(?i)vellum$' })
    $result.process = @($procs | ForEach-Object { [ordered]@{ id = $_.Id; name = $_.ProcessName } })

    Add-Type -AssemblyName UIAutomationClient -ErrorAction SilentlyContinue | Out-Null
    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $cond = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, "Vellum")
    $window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
    if ($window) {
        $result.window = $window.Current.Name
        $result.ok = $true
        $result.reason = "window"
    }

    $candidates = @(
        "$env:LOCALAPPDATA\Programs\Vellum\Vellum.exe",
        "$env:LOCALAPPDATA\Vellum\Vellum.exe"
    )
    foreach ($path in $candidates) {
        if (Test-Path -LiteralPath $path) {
            $result.binary = $path
            if (-not $result.ok) {
                $result.reason = "binary-present-not-running"
            }
            break
        }
    }
} catch {
    $result.reason = "probe-error"
    $result.detail = $_.Exception.Message
}

$result | ConvertTo-Json -Compress
