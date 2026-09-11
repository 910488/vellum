$ErrorActionPreference = "SilentlyContinue"

$workspace = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$escapedWorkspace = [Regex]::Escape($workspace)

# Only terminate Vite processes whose command line points at this exact
# workspace. Other projects and unrelated Node processes are never touched.
$viteProcesses = Get-CimInstance Win32_Process |
    Where-Object {
        $_.Name -eq "node.exe" -and
        $_.CommandLine -match "[\\/]vite[\\/]bin[\\/]vite\.js" -and
        $_.CommandLine -match $escapedWorkspace
    }

foreach ($process in $viteProcesses) {
    Stop-Process -Id $process.ProcessId -Force
    Write-Host "Stopped stale Vellum Vite process $($process.ProcessId)."
}
