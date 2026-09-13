param(
    [Parameter(Mandatory = $true)]
    [string]$CodexRoot
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$LockPath = Join-Path $RepoRoot "enhanced-runtime.lock.json"
if (-not (Test-Path $LockPath)) {
    throw "enhanced-runtime.lock.json is missing; the lock is the source pin"
}
$Lock = Get-Content -Raw -Path $LockPath | ConvertFrom-Json
$Pin = [string]$Lock.enhancedCodexCommit
if (-not $Pin -or $Pin.Length -ne 40) {
    throw "enhanced-runtime.lock.json is missing a 40-hex enhancedCodexCommit"
}
$Source = Join-Path $RepoRoot "crates\vellum-enhanced-codex\src"
$Dest = Join-Path $CodexRoot "codex-rs\core\src\enhanced"
$CoreLib = Join-Path $CodexRoot "codex-rs\core\src\lib.rs"
$ForkOnly = @("runtime.rs", "seams.rs", "reporting.rs", "context_projection.rs", "debug_log.rs", "debug_log_tests.rs", "mod.rs")

if (-not (Test-Path $CodexRoot)) {
    throw "Codex root does not exist: $CodexRoot"
}

Push-Location $CodexRoot
try {
    $Head = (git rev-parse HEAD).Trim()
    if ($Head -ne $Pin) {
        throw "Codex clone HEAD $Head is not the pinned commit $Pin"
    }
} finally {
    Pop-Location
}

New-Item -ItemType Directory -Force -Path $Dest | Out-Null
Get-ChildItem -Path $Source -Filter "*.rs" | ForEach-Object {
    if ($_.Name -eq "lib.rs") {
        return
    }
    if ($ForkOnly -contains $_.Name) {
        throw "refusing to overwrite fork-only module $($_.Name)"
    }
    Copy-Item -Force $_.FullName (Join-Path $Dest $_.Name)
}

$Lib = [System.IO.File]::ReadAllText((Join-Path $Source "lib.rs"))
$Idx = $Lib.IndexOf("#[cfg(test)]")
if ($Idx -lt 0) {
    throw "portable lib.rs is missing the isolation test marker"
}
$Slim = $Lib.Substring(0, $Idx).TrimEnd()
$Allow = "#![allow(unused_imports, dead_code)]"
$Lines = [System.Collections.Generic.List[string]]::new()
$Lines.AddRange([string[]]($Slim -split "`r?`n", -1))
$InsertAt = 0
for ($i = 0; $i -lt $Lines.Count; $i++) {
    if ($Lines[$i] -match '^//!' -or [string]::IsNullOrWhiteSpace($Lines[$i])) {
        $InsertAt = $i + 1
        continue
    }
    break
}
$HasAllow = $false
foreach ($Line in $Lines) {
    if ($Line -match '^#!\[allow\(unused_imports') {
        $HasAllow = $true
        break
    }
}
if (-not $HasAllow) {
    $Lines.Insert($InsertAt, $Allow)
    if ($InsertAt -lt $Lines.Count - 1 -and -not [string]::IsNullOrWhiteSpace($Lines[$InsertAt + 1])) {
        $Lines.Insert($InsertAt + 1, "")
    }
}
$Slim = ($Lines -join "`r`n").TrimEnd() + "`r`n`r`npub mod runtime;`r`npub mod seams;`r`n"
$ModRs = Join-Path $Dest "mod.rs"
if (Test-Path $ModRs) {
    Write-Host "Keeping existing fork-only $ModRs"
} else {
    [System.IO.File]::WriteAllText($ModRs, $Slim)
}

if (-not (Test-Path $CoreLib)) {
    throw "Codex core lib.rs not found: $CoreLib"
}
$LibLines = [System.Collections.Generic.List[string]]::new()
$LibLines.AddRange([string[]](Get-Content -Path $CoreLib))
$HasEnhanced = $false
foreach ($Line in $LibLines) {
    if ($Line -match '^mod enhanced;') {
        $HasEnhanced = $true
        break
    }
}
if (-not $HasEnhanced) {
    $InsertAt = -1
    for ($i = 0; $i -lt $LibLines.Count; $i++) {
        if ($LibLines[$i] -match '^#!\[') {
            $InsertAt = $i + 1
        }
    }
    if ($InsertAt -lt 0) {
        throw "Codex core lib.rs is missing an inner crate attribute to insert after"
    }
    $LibLines.Insert($InsertAt, "mod enhanced;")
    [System.IO.File]::WriteAllLines($CoreLib, $LibLines)
}

$NeedleFiles = @{
    "fn run_turn" = "session\turn.rs"
    "compact" = "compact.rs"
    "function_call" = "tools\router.rs"
    "context_window" = "session\context_window.rs"
}
foreach ($Needle in $NeedleFiles.Keys) {
    $Path = Join-Path $CodexRoot ("codex-rs\core\src\" + $NeedleFiles[$Needle])
    if (-not (Test-Path $Path)) {
        throw "Pinned Codex tree is missing expected seam file: $Path"
    }
    $Text = Get-Content -Raw $Path
    if ($Text -notmatch [regex]::Escape($Needle)) {
        throw "Pinned Codex tree is missing expected seam marker: $Needle"
    }
}

Write-Host "Installed portable Enhanced modules at $Dest (slim enhanced/mod.rs, no isolation tests)."
Write-Host "Codex-only runtime.rs and seams.rs must exist beside the copied modules."
Write-Host "Wire the lifecycle in patches/openai-codex-enhanced-mvp/SEAMS.md before building."
Write-Host "Then hash the binary and write enhancedCodexCommit + artifactSha256 + targetTriple into enhanced-runtime.lock.json."
