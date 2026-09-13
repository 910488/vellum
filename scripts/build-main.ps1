[CmdletBinding()]
param(
    [string] $SourceRef = "origin/main",
    [string] $SourceWorktree,
    [switch] $AllowDevelopment,
    [switch] $ValidateOnly,
    [switch] $SkipFetch
)

$ErrorActionPreference = "Stop"

# Windows PowerShell 5.1 otherwise decodes Git's UTF-8 output with the active
# legacy code page. That corrupts worktree paths containing non-ASCII
# characters and makes an existing main worktree look missing.
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$invokingWorktree = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
. (Join-Path $PSScriptRoot "build-provenance.ps1")

$commonGitDir = (& git -C $invokingWorktree rev-parse --path-format=absolute --git-common-dir).Trim()
if ($LASTEXITCODE -ne 0 -or -not $commonGitDir) {
    throw "Unable to locate the shared Git directory."
}

$source = Resolve-VellumBuildSource `
    -InvokingWorktree $invokingWorktree `
    -SourceWorktree $SourceWorktree `
    -ExpectedRef $SourceRef `
    -AllowDevelopment:$AllowDevelopment `
    -FetchRemote:(-not $SkipFetch)

$packageJson = [IO.File]::ReadAllText((Join-Path $source.SourceWorktree "package.json"), [Text.Encoding]::UTF8)
$version = ($packageJson | ConvertFrom-Json).version

$primaryWorktree = Split-Path -Parent $commonGitDir
$layout = Get-VellumArtifactLayout -PrimaryWorktree $primaryWorktree -ReleaseKind $source.ReleaseKind -Commit $source.Commit
$targetDir = $layout.TargetDir

Write-Host "Building source:    $($source.SourceWorktree)"
Write-Host "Source ref:         $($source.SourceRef)"
Write-Host "Commit:             $($source.Commit)"
Write-Host "origin/main:        $($source.OriginMainCommit)"
Write-Host "Release kind:       $($source.ReleaseKind)"
Write-Host "Clean:              $($source.Clean)"
Write-Host "Fingerprint:        $($source.SourceFingerprint)"
Write-Host "Tauri output:       $targetDir"
Write-Host "Renderer output:    $(Join-Path $source.SourceWorktree 'dist')"

if ($ValidateOnly) {
    # Source-only gate. Never rewrite a release manifest: an existing binary
    # in a shared target must not be relabeled with the current source commit.
    $info = [ordered]@{
        sourceRef = $source.SourceRef
        commit = $source.Commit
        expectedCommit = $source.ExpectedCommit
        originMainCommit = $source.OriginMainCommit
        branch = $source.Branch
        clean = [bool]$source.Clean
        releaseKind = $source.ReleaseKind
        mainRelease = [bool]$source.MainRelease
        version = $version
        sourceWorktree = $source.SourceWorktree
        sourceFingerprint = $source.SourceFingerprint
        validateOnly = $true
        manifestWritten = $false
        artifactDir = $layout.CommitRoot
    }
    if (Test-Path -LiteralPath $layout.CommitManifest -PathType Leaf) {
        $existing = Get-Content -LiteralPath $layout.CommitManifest -Raw -Encoding utf8 | ConvertFrom-Json
        if ($existing.commit -ne $source.Commit) {
            throw "Existing artifact manifest commit '$($existing.commit)' does not match source $($source.Commit)"
        }
        if ($existing.version -and $existing.version -ne $version) {
            throw "Existing artifact version '$($existing.version)' does not match package.json $version"
        }
        if ($existing.releaseKind -and $existing.releaseKind -ne $source.ReleaseKind) {
            throw "Existing artifact releaseKind '$($existing.releaseKind)' does not match $($source.ReleaseKind)"
        }
        $info.existingArtifactVerified = $true
        $info.existingManifest = $layout.CommitManifest
        Write-Host "VALIDATE_ONLY source ok; existing commit artifact matches"
    } else {
        $info.existingArtifactVerified = $false
        Write-Host "VALIDATE_ONLY source ok (no release manifest written)"
    }
    Write-Output ($info | ConvertTo-Json -Depth 6)
    return
}

New-Item -ItemType Directory -Force -Path $layout.CommitRoot | Out-Null

$previousTargetDir = $env:CARGO_TARGET_DIR
$previousCommit = $env:VELLUM_GIT_COMMIT
$previousVersion = $env:VELLUM_BUILD_VERSION
try {
    $env:CARGO_TARGET_DIR = $targetDir
    $env:VELLUM_GIT_COMMIT = $source.Commit
    $env:VELLUM_BUILD_VERSION = $version
    & pnpm --dir $source.SourceWorktree run build
    if ($LASTEXITCODE -ne 0) {
        throw "source build failed with exit code $LASTEXITCODE"
    }
}
finally {
    $env:CARGO_TARGET_DIR = $previousTargetDir
    $env:VELLUM_GIT_COMMIT = $previousCommit
    $env:VELLUM_BUILD_VERSION = $previousVersion
}

$bundleRoot = Join-Path $targetDir "release\bundle"
$bundles = @(
    Get-ChildItem -LiteralPath $bundleRoot -Recurse -File -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty FullName
)
$desktopExecutable = Join-Path $targetDir "release\vellum-proxy-desktop.exe"
$installer = $bundles | Where-Object { $_ -like "*-setup.exe" } | Select-Object -First 1

$info = New-VellumBuildInfo `
    -Source $source `
    -Version $version `
    -Executable $(if (Test-Path -LiteralPath $desktopExecutable) { $desktopExecutable } else { $null }) `
    -Installer $installer `
    -Bundles $bundles `
    -Target $targetDir `
    -Renderer (Join-Path $source.SourceWorktree "dist")

$info["artifactDir"] = $layout.CommitRoot
$manifestJson = $info | ConvertTo-Json -Depth 6
New-Item -ItemType Directory -Force -Path $layout.CommitRoot | Out-Null
New-Item -ItemType Directory -Force -Path $layout.KindRoot | Out-Null
$manifestJson | Set-Content -LiteralPath $layout.CommitManifest -Encoding utf8
$manifestJson | Set-Content -LiteralPath $layout.PointerManifest -Encoding utf8
Write-Host ""
if ($source.MainRelease) {
    Write-Host "main-release build completed."
} else {
    Write-Host "development build completed (not a main release)."
}
Write-Host "Commit artifact: $($layout.CommitRoot)"
Write-Host "Build manifest:  $($layout.PointerManifest)"

if ($info.executableSha256) {
    Write-Host "Executable SHA-256: $($info.executableSha256)"
}
if ($info.installerSha256) {
    Write-Host "Installer SHA-256:  $($info.installerSha256)"
}
if ($bundles.Count -gt 0) {
    Write-Host "Installers:"
    $bundles | ForEach-Object { Write-Host "  $_" }
}
elseif (Test-Path -LiteralPath $desktopExecutable) {
    Write-Host "Executable: $desktopExecutable"
}
