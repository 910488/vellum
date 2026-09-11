# Drive one named control by process identity, control type, and name/AutomationId.
param(
    [Parameter(Mandatory = $true)]
    [string] $Name,
    [string] $ProcessName = "Vellum",
    [string] $ControlType = "Button",
    [string] $AutomationId,
    [string] $SetValueFile,
    [string] $Screenshot
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName UIAutomationClient

function Get-ControlType([string] $name) {
    switch ($name.ToLowerInvariant()) {
        "button" { return [System.Windows.Automation.ControlType]::Button }
        "tabitem" { return [System.Windows.Automation.ControlType]::TabItem }
        "edit" { return [System.Windows.Automation.ControlType]::Edit }
        "text" { return [System.Windows.Automation.ControlType]::Text }
        "listitem" { return [System.Windows.Automation.ControlType]::ListItem }
        "menu" { return [System.Windows.Automation.ControlType]::Menu }
        "menuitem" { return [System.Windows.Automation.ControlType]::MenuItem }
        "hyperlink" { return [System.Windows.Automation.ControlType]::Hyperlink }
        "combobox" { return [System.Windows.Automation.ControlType]::ComboBox }
        "checkbox" { return [System.Windows.Automation.ControlType]::CheckBox }
        default { return [System.Windows.Automation.ControlType]::Button }
    }
}

function Invoke-Node([System.Windows.Automation.AutomationElement] $node) {
    try {
        $invoke = $node.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $invoke.Invoke()
        return "invoke"
    } catch {}
    try {
        $toggle = $node.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
        $toggle.Toggle()
        return "toggle"
    } catch {}
    try {
        $select = $node.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
        $select.Select()
        return "select"
    } catch {}
    throw "control has no Invoke/Toggle/Select pattern: $($node.Current.Name)"
}

$procs = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero })
if (-not $procs) {
    throw "process not found with a main window: $ProcessName"
}
$window = $null
foreach ($proc in $procs) {
    $candidate = [System.Windows.Automation.AutomationElement]::FromHandle($proc.MainWindowHandle)
    if ($candidate) { $window = $candidate; break }
}
if (-not $window) { throw "no UI Automation window for process $ProcessName" }

$type = Get-ControlType $ControlType
$nameCond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, $Name)
$typeCond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty, $type)
$and = New-Object System.Windows.Automation.AndCondition($nameCond, $typeCond)
$node = $null
if ($Name -eq "*") {
    $node = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $typeCond)
} else {
    $node = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $and)
}
if (-not $node -and $AutomationId) {
    $idCond = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::AutomationIdProperty, $AutomationId)
    $node = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants,
        (New-Object System.Windows.Automation.AndCondition($idCond, $typeCond)))
}
if (-not $node) { throw "control not found: process=$ProcessName type=$ControlType name=$Name" }

$pattern = "invoke"
$secret = $null
if ($SetValueFile -and (Test-Path -LiteralPath $SetValueFile)) {
    $secret = [System.IO.File]::ReadAllText($SetValueFile)
    Remove-Item -LiteralPath $SetValueFile -Force -ErrorAction SilentlyContinue
}
if ($null -ne $secret -and $secret -ne "") {
    $value = $node.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
    $value.SetValue($secret)
    $pattern = "set-value"
} else {
    $pattern = Invoke-Node $node
}

if ($Screenshot) {
    & (Join-Path $PSScriptRoot "screenshot.ps1") -OutFile $Screenshot | Out-Null
    if (-not (Test-Path -LiteralPath $Screenshot)) { throw "screenshot missing: $Screenshot" }
}

Write-Output ([ordered]@{
    ok = $true
    name = $Name
    process = $ProcessName
    controlType = $ControlType
    automationId = $AutomationId
    pattern = $pattern
} | ConvertTo-Json -Compress)
