# Click a Vellum automation control by Name. Best-effort UI Automation.
param(
    [Parameter(Mandatory = $true)]
    [string] $Name,
    [string] $Screenshot
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName UIAutomationClient
$root = [System.Windows.Automation.AutomationElement]::RootElement
$windowCond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, "Vellum")
$window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $windowCond)
if (-not $window) { throw "Vellum window not found" }

$nameCond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, $Name)
$node = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $nameCond)
if (-not $node) { throw "control not found: $Name" }

$invoke = $node.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
$invoke.Invoke()

if ($Screenshot) {
    & (Join-Path $PSScriptRoot "screenshot.ps1") -OutFile $Screenshot | Out-Null
}

Write-Output ([ordered]@{ ok = $true; name = $Name } | ConvertTo-Json -Compress)
